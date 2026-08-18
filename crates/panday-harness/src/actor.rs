//! The session actor and the turn state machine (docs/13 §the actor,
//! §the turn state machine).
//!
//! ```text
//! Idle → Assembling → Streaming → [Gating] → Executing ─┐
//!          ▲                                            │
//!          └────────────── observations folded in ──────┘
//! ```
//!
//! The actor is the **only writer** to its log, so `seq` is gapless without
//! locks (docs/13). Everything it does is an event; state is a fold over
//! those events, never a parallel truth.

use crate::context::{ContextBuilder, ReducerSummarizer, Summarizer};
use crate::hooks::{HookEngine, PreTool};
use crate::permissions::Gate;
use crate::tools::{SideEffects, ToolCtx, ToolRegistry};
use crate::{fold, EventStore, PermissionEngine, Phase, SessionState, StoreError, TurnBudget};
use panday_reducer::{ReduceCtx, Reducer};
use panday_sdk::{ModelClient, PandayError};
use panday_types::event::{Envelope, Event, ReducedOutput};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    Usage,
};
use panday_types::{AccountId, CallId, Json, SessionId, TurnId};
use std::sync::Arc;
use std::time::Instant;

/// Runs a child session on the parent's behalf (docs/13 §subagents).
///
/// A trait because a subagent needs everything a session needs — a model, a
/// store, a tool registry — and a `Tool` cannot conjure those. The parent
/// supplies the factory; the child gets its own log and a restricted registry.
#[async_trait::async_trait]
pub trait SubagentFactory: Send + Sync {
    /// Run a child to completion and return its **reduced result**.
    ///
    /// docs/13: "Parent receives `SubagentFinished{result_ref}` — the reduced
    /// result, not the child's transcript." Returning the transcript would
    /// defeat the entire point, which is that "be comprehensive" scales
    /// without one context window eating the bill.
    async fn run(
        &self,
        parent: SessionId,
        brief: String,
        budget: TurnBudget,
        depth: u8,
    ) -> Result<SubagentResult, HarnessError>;
}

/// What a child hands back.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub child: SessionId,
    /// The reduced answer — never the transcript.
    pub summary: String,
    pub usage: Usage,
}

/// The reserved tool name that spawns a child (docs/13 §tools).
pub const SPAWN_SUBAGENT: &str = "spawn_subagent";

/// docs/13: "Depth ≤ 2".
pub const MAX_SUBAGENT_DEPTH: u8 = 2;

/// Where streamed events go for connected clients (WS, ACP bridge).
///
/// Distinct from the log on purpose: `AssistantDelta` is emitted here and
/// **never persisted** (docs/03: "Deltas are ephemeral; messages are
/// durable"), which is what keeps replay deterministic.
pub trait EventSink: Send + Sync {
    fn emit(&self, envelope: &Envelope);
}

/// Collects emitted events; the CLI renders from one of these.
#[derive(Default)]
pub struct CollectSink {
    events: std::sync::Mutex<Vec<Envelope>>,
}

impl CollectSink {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn snapshot(&self) -> Vec<Envelope> {
        self.events.lock().unwrap().clone()
    }
}

impl EventSink for CollectSink {
    fn emit(&self, envelope: &Envelope) {
        self.events.lock().unwrap().push(envelope.clone());
    }
}

/// In-memory event store. PG lands at M3.3; `panday local` uses SQLite.
#[derive(Default)]
pub struct MemoryStore {
    events: std::sync::Mutex<Vec<Envelope>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn all(&self) -> Vec<Envelope> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl EventStore for MemoryStore {
    async fn append(&self, e: Envelope) -> Result<(), StoreError> {
        let mut log = self.events.lock().unwrap();
        // The single-writer invariant is what makes `seq` gapless; a gap or
        // repeat means two writers, which corrupts every fold downstream.
        if let Some(last) = log.last() {
            if e.seq != last.seq + 1 {
                return Err(StoreError::SeqConflict(e.seq));
            }
        }
        log.push(e);
        Ok(())
    }

    async fn read_after(
        &self,
        session: SessionId,
        after_seq: u64,
    ) -> Result<Vec<Envelope>, StoreError> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.session_id == session && e.seq > after_seq)
            .cloned()
            .collect())
    }

    async fn next_seq(&self, _session: SessionId) -> Result<u64, StoreError> {
        Ok(self.events.lock().unwrap().last().map_or(1, |e| e.seq + 1))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("event store: {0}")]
    Store(#[from] StoreError),
    #[error("model: {0}")]
    Model(#[from] PandayError),
}

/// How a turn ended.
///
/// `AwaitingPermission` is not a stop: the turn is parked mid-flight with
/// calls gated, and resumes when decisions arrive (M13.3). Modelling it as a
/// `StopReason` would make a paused turn indistinguishable from a finished
/// one in the log.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    Finished(StopReason),
    AwaitingPermission(Vec<CallId>),
}

/// One live session.
pub struct SessionActor {
    session: SessionId,
    account: AccountId,
    model_ref: ModelRef,
    state: SessionState,
    seq: u64,
    log: Arc<dyn EventStore>,
    model: Arc<dyn ModelClient>,
    tools: ToolRegistry,
    permissions: PermissionEngine,
    reducer: Box<dyn Reducer>,
    budget: TurnBudget,
    subs: Vec<Arc<dyn EventSink>>,
    /// Set for the duration of a turn so every event it emits correlates.
    current_turn: Option<TurnId>,
    /// Calls parked awaiting a decision, kept with their arguments so an
    /// `Allow` can dispatch exactly what was gated — re-deriving it from the
    /// log would risk dispatching something subtly different from what the
    /// human approved.
    parked: Vec<PendingCall>,
    /// Set by [`SessionActor::cancel`]; checked at every transition.
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// Cache-aligned layout (ADR-008). Built once so the stable band is
    /// byte-identical every turn.
    context: ContextBuilder,
    /// Present when this session may spawn children.
    subagents: Option<Arc<dyn SubagentFactory>>,
    /// 0 for a user-facing session. Children get parent + 1.
    depth: u8,
    /// How many transcript messages have been folded into summaries.
    compacted_upto: usize,
    /// Extension points (docs/13 §hooks). Empty by default.
    hooks: HookEngine,
}

impl SessionActor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: SessionId,
        account: AccountId,
        model_ref: ModelRef,
        log: Arc<dyn EventStore>,
        model: Arc<dyn ModelClient>,
        tools: ToolRegistry,
        permissions: PermissionEngine,
        reducer: Box<dyn Reducer>,
        budget: TurnBudget,
    ) -> Self {
        Self {
            session,
            account,
            model_ref,
            state: SessionState::default(),
            seq: 0,
            log,
            model,
            tools,
            permissions,
            reducer,
            budget,
            subs: Vec::new(),
            current_turn: None,
            parked: Vec::new(),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            context: ContextBuilder::new(
                "You are Panday, a coding agent. Work in the sandboxed workspace. \
                 Prefer reading before editing, and verify changes by running tests.",
                Vec::new(),
            ),
            compacted_upto: 0,
            subagents: None,
            depth: 0,
            hooks: HookEngine::new(),
        }
    }

    /// Install hooks. They run inside the loop and their failures are
    /// contained (docs/13: "log, skip, continue").
    pub fn with_hooks(mut self, hooks: HookEngine) -> Self {
        self.hooks = hooks;
        self
    }

    /// Allow this session to spawn children.
    pub fn with_subagents(mut self, factory: Arc<dyn SubagentFactory>, depth: u8) -> Self {
        self.subagents = Some(factory);
        self.depth = depth;
        self
    }

    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// The budget a child receives: half of this session's, floored so a
    /// child is never handed a budget it cannot do anything with.
    ///
    /// docs/13 says "a fraction of the parent budget"; halving is the simplest
    /// rule that makes depth bounded in *cost* as well as in levels — a
    /// two-level tree cannot spend more than the root was allowed.
    pub fn child_budget(&self) -> TurnBudget {
        TurnBudget {
            max_steps: (self.budget.max_steps / 2).max(1),
            max_wall_ms: (self.budget.max_wall_ms / 2).max(1_000),
            max_spend_micros: self.budget.max_spend_micros / 2,
        }
    }

    /// Replace the system prompt and register the tool schemas in the stable
    /// band. Must be called before the first turn: mutating the stable band
    /// mid-session is a cache break (ADR-008, docs/13 §registry discipline).
    pub fn with_context(mut self, builder: ContextBuilder) -> Self {
        self.context = builder;
        self
    }

    /// A handle that can request cancellation from another task — the client
    /// pressing ctrl-c is not on the actor's task.
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle(self.cancelled.clone())
    }

    /// Request cancellation. docs/13: "Cancellation is a first-class
    /// transition from every state."
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn subscribe(&mut self, sink: Arc<dyn EventSink>) {
        self.subs.push(sink);
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Rebuild in-memory state from the log — crash recovery (docs/13:
    /// "Crash recovery = reload fold of log").
    pub async fn resume(&mut self) -> Result<(), HarnessError> {
        let events = self.log.read_after(self.session, 0).await?;
        self.seq = events.last().map_or(0, |e| e.seq);
        self.state = fold(&events);
        Ok(())
    }

    /// Append to the log, then notify subscribers.
    ///
    /// **Persist-before-proceed** (docs/13): this awaits the durable write
    /// before the caller may advance. A store failure aborts the turn rather
    /// than continuing against a log that does not describe reality.
    async fn commit(&mut self, event: Event) -> Result<Envelope, HarnessError> {
        self.seq += 1;
        let envelope = Envelope {
            v: panday_types::PROTOCOL_VERSION,
            session_id: self.session,
            seq: self.seq,
            turn_id: self.current_turn,
            at: time::OffsetDateTime::now_utc(),
            event,
        };
        self.log.append(envelope.clone()).await?;
        self.state = fold_one(&self.state, &envelope);
        for s in &self.subs {
            s.emit(&envelope);
        }
        Ok(envelope)
    }

    /// Emit to subscribers WITHOUT persisting. Deltas only.
    fn emit_ephemeral(&self, event: Event) {
        let envelope = Envelope {
            v: panday_types::PROTOCOL_VERSION,
            session_id: self.session,
            seq: self.seq,
            turn_id: self.current_turn,
            at: time::OffsetDateTime::now_utc(),
            event,
        };
        for s in &self.subs {
            s.emit(&envelope);
        }
    }

    /// Drive one user input to completion (or to a permission park).
    pub async fn handle_user_input(&mut self, text: &str) -> Result<TurnOutcome, HarnessError> {
        self.commit(Event::UserMessage {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            source: panday_types::event::ClientKind::Cli,
        })
        .await?;

        self.run_loop().await
    }

    async fn run_loop(&mut self) -> Result<TurnOutcome, HarnessError> {
        use tracing::Instrument;

        // docs/21 §traces: one span tree per turn,
        // `turn > assemble > model.call > tool.gate > sandbox.exec > reduce`.
        //
        // The RAW uuid, not `Display`. docs/03: the `sess_01J…` short form is
        // "a display encoding, not a second id" — recording it would mean a
        // span could not be joined against the event log by string equality,
        // which is the entire point of the id scheme.
        let turn_span = tracing::info_span!(
            "turn",
            session_id = %self.session.0,
            account_id = %self.account.0,
        );
        // `.instrument()`, NOT `enter()`: an `enter()` guard is thread-local
        // and a multi-thread runtime moves the future between polls, which
        // drops the span mid-turn without any error.
        self.run_loop_inner().instrument(turn_span).await
    }

    async fn run_loop_inner(&mut self) -> Result<TurnOutcome, HarnessError> {
        self.hooks.pre_turn();
        let started = Instant::now();
        let mut steps: u32 = 0;
        let mut turn_usage = Usage::default();

        loop {
            // --- bounds are checked at every transition; exceeding one is a
            // NORMAL stop, never a panic (docs/13 §invariants) ---
            if steps >= self.budget.max_steps {
                return self.finalize(StopReason::MaxSteps, turn_usage).await;
            }
            if started.elapsed().as_millis() as u64 >= self.budget.max_wall_ms {
                return self.finalize(StopReason::BudgetExceeded, turn_usage).await;
            }
            if self.is_cancelled() {
                return self.finalize(StopReason::Cancelled, turn_usage).await;
            }

            self.current_turn = Some(TurnId::new());
            self.commit(Event::TurnStarted {
                model: self.model_ref.clone(),
                parent: None,
            })
            .await?;
            steps += 1;

            // --- Streaming ---
            let mut req = {
                use tracing::Instrument;
                self.assemble()
                    .instrument(tracing::info_span!("assemble"))
                    .await?
            };
            self.hooks.pre_model(&mut req);
            let model_span = tracing::info_span!(
                "model.call",
                model = %req.model.0,
                input_tokens = tracing::field::Empty,
                output_tokens = tracing::field::Empty,
                cache_read_tokens = tracing::field::Empty,
            );
            let streamed = {
                use tracing::Instrument;
                let model = self.model.clone();
                async {
                    let mut stream = model.chat(req).await?;
                    self.consume_stream(&mut stream).await
                }
                .instrument(model_span.clone())
                .await?
            };
            model_span.record("input_tokens", streamed.usage.input_tokens);
            model_span.record("output_tokens", streamed.usage.output_tokens);
            model_span.record("cache_read_tokens", streamed.usage.cache_read_tokens);
            self.hooks.post_model(&streamed.text);
            turn_usage.add(streamed.usage);

            self.commit(Event::AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: streamed.text.clone(),
                }],
                usage: streamed.usage,
            })
            .await?;

            if streamed.calls.is_empty() {
                return self.finalize(streamed.stop, turn_usage).await;
            }

            // --- Gating ---
            let mut queued: Vec<PendingCall> = Vec::new();
            let mut parked: Vec<CallId> = Vec::new();

            for mut call in streamed.calls {
                // Hooks see the call first: docs/13 puts DLP and command
                // rewrites here, and a redaction must happen before a human is
                // asked to approve the *original* arguments.
                match self.hooks.pre_tool(&call.name, &call.args) {
                    PreTool::Proceed => {}
                    PreTool::Rewrite(args) => call.args = args,
                    PreTool::Veto(reason) => {
                        // A veto is recorded like any other refusal, so the
                        // model learns and the log explains why.
                        self.commit(Event::ToolCall {
                            call_id: call.id,
                            tool: call.name.clone(),
                            args: call.args.clone(),
                            provider_call_id: call.provider_id.clone(),
                        })
                        .await?;
                        self.commit(Event::ToolResult {
                            call_id: call.id,
                            output: ReducedOutput {
                                text: format!("vetoed by hook: {reason}"),
                                tokens_raw: 0,
                                tokens_kept: 0,
                                strategy: "hook_veto".into(),
                            },
                            raw_ref: None,
                            duration_ms: 0,
                            is_error: true,
                        })
                        .await?;
                        continue;
                    }
                }

                let req = self
                    .tools
                    .get(&call.name)
                    .map(|t| t.requirements())
                    .unwrap_or(crate::tools::ToolReq {
                        sandbox_tier: panday_sandbox::SandboxTier::T0InProcess,
                        side_effects: SideEffects::None,
                        independent: true,
                        replay: crate::tools::Replay::Unsafe,
                    });

                let gate = {
                    let _s = tracing::info_span!("tool.gate", tool = %call.name).entered();
                    self.permissions.gate(&call.name, &req, &call.args)
                };
                tracing::debug!(tool = %call.name, gate = ?gate, "permission decision");

                match gate {
                    Gate::Allow => queued.push(call),
                    Gate::Deny => {
                        // A denial is still a tool call that happened and an
                        // observation the model must see, so both are logged
                        // — the model learns it was refused and can adapt.
                        self.commit(Event::ToolCall {
                            call_id: call.id,
                            tool: call.name.clone(),
                            args: call.args.clone(),
                            provider_call_id: call.provider_id.clone(),
                        })
                        .await?;
                        self.commit(Event::ToolResult {
                            call_id: call.id,
                            output: ReducedOutput {
                                text: format!("denied by policy: {}", call.name),
                                tokens_raw: 0,
                                tokens_kept: 0,
                                strategy: "permission_denied".into(),
                            },
                            raw_ref: None,
                            duration_ms: 0,
                            is_error: true,
                        })
                        .await?;
                    }
                    Gate::Ask => {
                        self.commit(Event::ToolCall {
                            call_id: call.id,
                            tool: call.name.clone(),
                            args: call.args.clone(),
                            provider_call_id: call.provider_id.clone(),
                        })
                        .await?;
                        self.commit(Event::PermissionRequest {
                            call_id: call.id,
                            tool: call.name.clone(),
                            action: describe(&call.name, &call.args),
                            options: vec!["allow".into(), "allow_remember".into(), "deny".into()],
                        })
                        .await?;
                        parked.push(call.id);
                        self.parked.push(call.clone());
                    }
                }
            }

            // Parking suspends the turn. Executing the allowed calls first
            // would let the model act on a half-approved plan.
            if !parked.is_empty() {
                return Ok(TurnOutcome::AwaitingPermission(parked));
            }

            if self.is_cancelled() {
                return self.finalize(StopReason::Cancelled, turn_usage).await;
            }

            // --- Executing ---
            //
            // Calls that declare independence run concurrently (docs/13: "run
            // queued calls (parallel where tools declare independence)");
            // anything else runs in order, because a dependent call may rely
            // on an earlier one's effect.
            //
            // Execution is parallel; *logging* stays sequential. The actor is
            // the only writer to its log, which is what makes `seq` gapless
            // without locks — so results are committed one at a time even when
            // the work overlapped.
            for group in group_for_execution(&self.tools, queued) {
                if self.is_cancelled() {
                    // Stop dispatching immediately; anything already running
                    // is killed when its stream is dropped.
                    return self.finalize(StopReason::Cancelled, turn_usage).await;
                }

                for call in &group {
                    self.commit(Event::ToolCall {
                        call_id: call.id,
                        tool: call.name.clone(),
                        args: call.args.clone(),
                        provider_call_id: call.provider_id.clone(),
                    })
                    .await?;
                }

                if group.len() == 1 {
                    let call = group.into_iter().next().expect("len 1");
                    self.execute(call).await?;
                    continue;
                }

                let outcomes = self.run_concurrently(&group).await;
                for (call, outcome) in group.into_iter().zip(outcomes) {
                    self.record_outcome(call, outcome, 0).await?;
                }
            }
        }
    }

    /// Deal with calls that were dispatched but whose result never reached
    /// the log — the crash-resume path (docs/13 §persist-before-proceed).
    ///
    /// > "resume **replays idempotent calls and refuses irreversible ones** —
    /// > surfacing the refusal with the call's args, since the tool may or may
    /// > not have run."
    ///
    /// The decision comes from [`crate::tools::Replay`], not `side_effects`:
    /// `bash` is `Idempotent` for consent reasons yet must never be re-run.
    /// A tool the registry no longer knows is refused too — its replay
    /// safety is unknowable, and guessing "safe" is the dangerous direction.
    pub async fn resume_pending(&mut self) -> Result<Vec<CallId>, HarnessError> {
        let pending = self.state.inflight_calls.clone();
        let mut refused = Vec::new();

        for (call_id, tool, args) in pending {
            let safe = self
                .tools
                .get(&tool)
                .map(|t| t.requirements().replay == crate::tools::Replay::Safe)
                .unwrap_or(false);

            if safe {
                self.execute(PendingCall {
                    id: call_id,
                    name: tool,
                    provider_id: None,
                    args,
                })
                .await?;
            } else {
                // The refusal carries the arguments because the human reading
                // it has to decide whether the call already took effect — and
                // cannot without seeing what was run.
                self.commit(Event::ToolResult {
                    call_id,
                    output: ReducedOutput {
                        text: format!(
                            "refused on resume: `{tool}` was dispatched before a crash and may                              or may not have run. It is not safe to replay automatically.                              Arguments were: {args}"
                        ),
                        tokens_raw: 0,
                        tokens_kept: 0,
                        strategy: "replay_refused".into(),
                    },
                    raw_ref: None,
                    duration_ms: 0,
                    is_error: true,
                })
                .await?;
                refused.push(call_id);
            }
        }
        Ok(refused)
    }

    /// Answer a parked permission request and continue the turn.
    ///
    /// docs/13: decisions are events, "so grants are auditable and replayable
    /// like everything else". The decision is committed before anything is
    /// dispatched, so a crash between consent and execution leaves the
    /// consent on the record rather than losing it.
    pub async fn decide(
        &mut self,
        call_id: CallId,
        decision: panday_types::event::PermDecision,
        by: panday_types::event::Actor,
    ) -> Result<TurnOutcome, HarnessError> {
        use panday_types::event::PermDecision;

        let Some(index) = self.parked.iter().position(|c| c.id == call_id) else {
            return Err(HarnessError::Model(PandayError::Protocol(format!(
                "no parked call {call_id} awaiting a decision"
            ))));
        };
        let call = self.parked.remove(index);

        self.commit(Event::PermissionDecision {
            call_id,
            decision,
            by,
        })
        .await?;

        match decision {
            PermDecision::Deny => {
                // The model must see the refusal, or it will simply try again.
                self.commit(Event::ToolResult {
                    call_id,
                    output: ReducedOutput {
                        text: format!("denied by the user: {}", call.name),
                        tokens_raw: 0,
                        tokens_kept: 0,
                        strategy: "permission_denied".into(),
                    },
                    raw_ref: None,
                    duration_ms: 0,
                    is_error: true,
                })
                .await?;
            }
            PermDecision::Allow | PermDecision::AllowRemember => {
                if decision == PermDecision::AllowRemember {
                    self.permissions.remember(&call.name, &call.args, decision);
                }
                self.execute(call).await?;
            }
        }

        // Other calls from the same step may still be waiting; the turn
        // resumes only once every gate has an answer.
        if !self.parked.is_empty() {
            return Ok(TurnOutcome::AwaitingPermission(
                self.parked.iter().map(|c| c.id).collect(),
            ));
        }

        self.run_loop().await
    }

    /// Run several independent calls at once, returning outcomes in order.
    ///
    /// Only `&self` is needed here, which is precisely why this can be
    /// concurrent — nothing touches the log until the results come back.
    async fn run_concurrently(&self, calls: &[PendingCall]) -> Vec<crate::tools::ToolOutcome> {
        let futures = calls.iter().map(|call| async move {
            match self.tools.get(&call.name) {
                Some(tool) => {
                    let ctx = ToolCtx {
                        account: self.account,
                        session: self.session,
                        turn: TurnId::new(),
                    };
                    tool.call(ctx, call.args.clone()).await
                }
                None => crate::tools::ToolOutcome {
                    raw: format!("no such tool: {}", call.name),
                    is_error: true,
                },
            }
        });
        futures_util::future::join_all(futures).await
    }

    /// Reduce an outcome and commit its `ToolResult`.
    async fn record_outcome(
        &mut self,
        call: PendingCall,
        outcome: crate::tools::ToolOutcome,
        duration_ms: u64,
    ) -> Result<(), HarnessError> {
        let _reduce_span = tracing::info_span!("reduce", tool = %call.name).entered();

        // Tool output NEVER enters context raw (ADR-007).
        let reduced = self.reducer.reduce(
            &outcome.raw,
            &ReduceCtx {
                tool: call.name.clone(),
                task: None,
                expected_reads: 1,
                price_per_token_micros: 0,
                aggressive: false,
            },
        );

        tracing::debug!(
            tool = %call.name,
            tokens_raw = reduced.tokens_raw,
            tokens_kept = reduced.tokens_kept,
            strategy = %reduced.strategy,
            is_error = outcome.is_error,
            "tool observation reduced"
        );
        self.hooks.post_tool(&call.name, &reduced);

        self.commit(Event::ToolResult {
            call_id: call.id,
            output: reduced,
            raw_ref: None,
            duration_ms,
            is_error: outcome.is_error,
        })
        .await?;
        Ok(())
    }

    /// Spawn a child session and fold its reduced result back in.
    async fn spawn_subagent(&mut self, call: PendingCall) -> Result<(), HarnessError> {
        let brief = call
            .args
            .get("brief")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let Some(factory) = self.subagents.clone() else {
            return self
                .record_outcome(
                    call,
                    crate::tools::ToolOutcome {
                        raw: "subagents are not enabled for this session".into(),
                        is_error: true,
                    },
                    0,
                )
                .await;
        };

        if self.depth >= MAX_SUBAGENT_DEPTH {
            // docs/13 caps depth at 2. Refusing is an observation the model
            // can work with; recursing would let one request fan out without
            // bound and bill the parent for all of it.
            return self
                .record_outcome(
                    call,
                    crate::tools::ToolOutcome {
                        raw: format!(
                            "subagent depth limit reached ({MAX_SUBAGENT_DEPTH}); \
                             do this work directly instead of delegating"
                        ),
                        is_error: true,
                    },
                    0,
                )
                .await;
        }

        if brief.trim().is_empty() {
            return self
                .record_outcome(
                    call,
                    crate::tools::ToolOutcome {
                        raw: "spawn_subagent needs a `brief`: the child gets no other context"
                            .into(),
                        is_error: true,
                    },
                    0,
                )
                .await;
        }

        let began = Instant::now();
        let budget = self.child_budget();
        let result = factory
            .run(self.session, brief.clone(), budget, self.depth + 1)
            .await;

        match result {
            Ok(res) => {
                self.commit(Event::SubagentSpawned {
                    child: res.child,
                    brief,
                })
                .await?;
                self.commit(Event::SubagentFinished {
                    child: res.child,
                    // The child's log IS the artifact; a separate spill would
                    // duplicate it.
                    result_ref: panday_types::id::ArtifactRef {
                        hash: format!("log:{}", res.child),
                        size: 0,
                        media_type: Some("application/vnd.panday.transcript".into()),
                    },
                })
                .await?;

                self.record_outcome(
                    call,
                    crate::tools::ToolOutcome {
                        raw: res.summary,
                        is_error: false,
                    },
                    began.elapsed().as_millis() as u64,
                )
                .await
            }
            Err(e) => {
                self.record_outcome(
                    call,
                    crate::tools::ToolOutcome {
                        raw: format!("subagent failed: {e}"),
                        is_error: true,
                    },
                    began.elapsed().as_millis() as u64,
                )
                .await
            }
        }
    }

    /// Run one tool and fold its (reduced) observation back in.
    async fn execute(&mut self, call: PendingCall) -> Result<(), HarnessError> {
        if call.name == SPAWN_SUBAGENT {
            return self.spawn_subagent(call).await;
        }

        let began = Instant::now();

        // Synchronous span entry is correct here only because nothing between
        // this point and the await is thread-sensitive; the await itself is
        // instrumented below.
        let exec_span = tracing::info_span!("sandbox.exec", tool = %call.name);

        let outcome = match self.tools.get(&call.name) {
            Some(tool) => {
                let ctx = ToolCtx {
                    account: self.account,
                    session: self.session,
                    turn: TurnId::new(),
                };
                let cancelled = self.cancelled.clone();
                let _entered = exec_span.clone().entered();
                tokio::select! {
                    outcome = tool.call(ctx, call.args.clone()) => outcome,
                    // Dropping the tool future drops its ExecStream, which is
                    // what kills the child process (see the sandbox note on
                    // receiver-drop). Polling a flag is enough here because a
                    // cancelled session is being torn down either way.
                    _ = wait_for_cancel(cancelled) => crate::tools::ToolOutcome {
                        raw: "cancelled".into(),
                        is_error: true,
                    },
                }
            }
            // An unknown tool is the model's mistake, not a crash: report it
            // as a tool error so the loop can continue and correct.
            None => crate::tools::ToolOutcome {
                raw: format!("no such tool: {}", call.name),
                is_error: true,
            },
        };

        self.record_outcome(call, outcome, began.elapsed().as_millis() as u64)
            .await
    }

    async fn finalize(
        &mut self,
        reason: StopReason,
        usage: Usage,
    ) -> Result<TurnOutcome, HarnessError> {
        tracing::info!(stop_reason = ?reason, "turn finished");
        self.hooks.on_stop(reason);
        self.commit(Event::TurnFinished {
            reason,
            usage,
            // Pricing the turn is the ledger's job (M11.4); the harness must
            // not invent a number it cannot substantiate.
            cost_micros: 0,
        })
        .await?;
        Ok(TurnOutcome::Finished(reason))
    }

    /// Build the model request from the log, cache-aligned (ADR-008).
    async fn assemble(&mut self) -> Result<ChatRequest, HarnessError> {
        let events = self.log.read_after(self.session, 0).await?;
        let full = transcript(&events);

        // The current turn starts at the last user message; everything before
        // it is settled and therefore cacheable.
        let hot_from = full
            .iter()
            .rposition(|m| m.role == Role::User)
            .unwrap_or(full.len());

        let live = &full[self.compacted_upto.min(full.len())..];
        let hot_from = hot_from.saturating_sub(self.compacted_upto);

        let mut ctx = self.context.build(live, hot_from);

        // Compaction: fold the oldest settled span into the semi-stable band
        // rather than letting the window grow without bound. The full text
        // stays in the log — this is a view optimisation, never data loss.
        if self.context.should_compact(&ctx) && hot_from > 2 {
            let split = hot_from / 2;
            let before = ctx.approx_tokens();

            let summariser = ReducerSummarizer(panday_reducer::StructuralReducer::new(
                panday_reducer::GenericReducer::default(),
            ));
            let summary = summariser.summarize(&live[..split]);
            self.context.add_summary(summary);
            self.compacted_upto += split;

            let live = &full[self.compacted_upto.min(full.len())..];
            ctx = self.context.build(live, hot_from.saturating_sub(split));

            self.hooks.on_compaction(before, ctx.approx_tokens());
            self.commit(Event::Compaction {
                from_seq: 0,
                to_seq: self.seq,
                // The span is recoverable from the log itself, which is the
                // artifact; a separate spill would duplicate it.
                summary_ref: panday_types::id::ArtifactRef {
                    hash: format!("log:{}:{}", self.session, self.compacted_upto),
                    size: 0,
                    media_type: Some("application/vnd.panday.transcript".into()),
                },
                tokens_before: before,
                tokens_after: ctx.approx_tokens(),
            })
            .await?;
        }

        Ok(ChatRequest {
            model: self.model_ref.clone(),
            messages: ctx.messages,
            tools: self
                .tools
                .specs()
                .into_iter()
                .map(|s| panday_types::model::ToolDef {
                    name: s.name,
                    description: s.description,
                    parameters: s.parameters,
                })
                .collect(),
            sampling: Sampling::default(),
            cache: ctx.cache,
            stream: true,
            metadata: CallMeta {
                account: self.account,
                request: panday_types::RequestId::new(),
                session: Some(self.session),
                turn: None,
                task: None,
            },
        })
    }

    async fn consume_stream(
        &mut self,
        stream: &mut panday_sdk::ItemStream,
    ) -> Result<Streamed, HarnessError> {
        use futures_util::StreamExt;

        let mut out = Streamed::default();
        let mut partial: Vec<(CallId, String, Option<String>, String)> = Vec::new();

        while let Some(item) = stream.next().await {
            match item? {
                StreamItem::Delta { text } => {
                    out.text.push_str(&text);
                    // Streamed to clients, never written to the log.
                    self.emit_ephemeral(Event::AssistantDelta { text });
                }
                StreamItem::ToolCallStart {
                    id,
                    name,
                    provider_id,
                } => partial.push((id, name, provider_id, String::new())),
                StreamItem::ToolCallDelta { id, args_fragment } => {
                    if let Some(p) = partial.iter_mut().find(|p| p.0 == id) {
                        p.3.push_str(&args_fragment);
                    }
                }
                StreamItem::Usage { usage } => out.usage = usage,
                StreamItem::Done { reason } => out.stop = reason,
            }
        }

        for (id, name, provider_id, args) in partial {
            out.calls.push(PendingCall {
                id,
                name,
                provider_id,
                // Malformed arguments are the model's error to see, not a
                // parse panic: hand it back as-is and let the tool reject it.
                args: serde_json::from_str(&args).unwrap_or(Json::Null),
            });
        }
        Ok(out)
    }
}

/// Requests cancellation of a running session from another task.
#[derive(Clone)]
pub struct CancelHandle(Arc<std::sync::atomic::AtomicBool>);

impl CancelHandle {
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Resolve once cancellation is requested.
async fn wait_for_cancel(flag: Arc<std::sync::atomic::AtomicBool>) {
    loop {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[derive(Debug, Clone)]
pub struct PendingCall {
    pub id: CallId,
    pub name: String,
    pub provider_id: Option<String>,
    pub args: Json,
}

struct Streamed {
    text: String,
    calls: Vec<PendingCall>,
    usage: Usage,
    stop: StopReason,
}

impl Default for Streamed {
    fn default() -> Self {
        Self {
            text: String::new(),
            calls: Vec::new(),
            usage: Usage::default(),
            // A stream that ends without saying why ended its turn; the
            // adapters guarantee a Done frame, so this is belt-and-braces.
            stop: StopReason::EndTurn,
        }
    }
}

/// Split queued calls into groups that may run together.
///
/// A run of consecutive calls whose tools all declare `independent` becomes one
/// concurrent group. Anything else is its own group of one: a dependent call
/// may rely on an earlier call's effect, and reordering or overlapping it would
/// be a correctness bug the model has no way to see.
///
/// `spawn_subagent` is always alone — it runs a whole session, and the budget
/// split assumes one child at a time.
fn group_for_execution(tools: &ToolRegistry, queued: Vec<PendingCall>) -> Vec<Vec<PendingCall>> {
    let mut groups: Vec<Vec<PendingCall>> = Vec::new();

    for call in queued {
        let independent = call.name != SPAWN_SUBAGENT
            && tools
                .get(&call.name)
                .map(|t| t.requirements().independent)
                .unwrap_or(false);

        match groups.last_mut() {
            Some(last) if independent && last_is_independent(tools, last) => last.push(call),
            _ => groups.push(vec![call]),
        }
    }
    groups
}

fn last_is_independent(tools: &ToolRegistry, group: &[PendingCall]) -> bool {
    group.iter().all(|c| {
        c.name != SPAWN_SUBAGENT
            && tools
                .get(&c.name)
                .map(|t| t.requirements().independent)
                .unwrap_or(false)
    })
}

/// Incremental fold — the same rules as [`crate::fold`], applied to one
/// event so the actor need not re-read its whole log per step.
fn fold_one(prev: &SessionState, envelope: &Envelope) -> SessionState {
    let mut next = prev.clone();
    next.last_seq = envelope.seq;
    match &envelope.event {
        Event::UserMessage { .. } => next.phase = Phase::Assembling,
        Event::TurnStarted { .. } => {
            next.phase = Phase::Streaming;
            next.steps_this_turn = 0;
        }
        Event::ToolCall {
            call_id,
            tool,
            args,
            ..
        } => {
            next.phase = Phase::Executing;
            next.steps_this_turn += 1;
            next.inflight_calls
                .push((*call_id, tool.clone(), args.clone()));
        }
        Event::ToolResult { call_id, .. } => {
            next.inflight_calls.retain(|(id, _, _)| id != call_id);
            if next.inflight_calls.is_empty() {
                next.phase = Phase::Assembling;
            }
        }
        Event::PermissionRequest { call_id, .. } => {
            next.phase = Phase::Gating;
            next.pending_permissions.push(*call_id);
            next.inflight_calls.retain(|(id, _, _)| id != call_id);
        }
        Event::PermissionDecision {
            call_id, decision, ..
        } => {
            next.pending_permissions.retain(|id| id != call_id);
            if *decision == panday_types::event::PermDecision::Deny {
                next.inflight_calls.retain(|(id, _, _)| id != call_id);
            }
        }
        Event::AssistantMessage { usage, .. } => next.usage_total.add(*usage),
        Event::TurnFinished {
            reason,
            cost_micros,
            ..
        } => {
            next.cost_micros_total += cost_micros;
            next.finished_turns += 1;
            next.last_stop = Some(*reason);
            next.phase = Phase::Idle;
            next.inflight_calls.clear();
            next.pending_permissions.clear();
        }
        _ => {}
    }
    next
}

/// Project the log into the model's message list.
pub fn transcript(events: &[Envelope]) -> Vec<Message> {
    let mut out = Vec::new();
    // A tool result must quote the provider's id, so remember it from the
    // matching ToolCall (docs/03 §two ids per tool call).
    let mut provider_ids: std::collections::HashMap<CallId, Option<String>> = Default::default();

    for env in events {
        match &env.event {
            Event::UserMessage { content, .. } => out.push(Message {
                role: Role::User,
                content: content.clone(),
                call_id: None,
                provider_call_id: None,
            }),
            Event::AssistantMessage { content, .. } => out.push(Message {
                role: Role::Assistant,
                content: content.clone(),
                call_id: None,
                provider_call_id: None,
            }),
            Event::ToolCall {
                call_id,
                provider_call_id,
                ..
            } => {
                provider_ids.insert(*call_id, provider_call_id.clone());
            }
            Event::ToolResult {
                call_id, output, ..
            } => out.push(Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolOutput {
                    call_id: *call_id,
                    text: output.text.clone(),
                }],
                call_id: Some(*call_id),
                provider_call_id: provider_ids.get(call_id).cloned().flatten(),
            }),
            _ => {}
        }
    }
    out
}

fn describe(tool: &str, args: &Json) -> String {
    match args {
        Json::Object(map) => {
            let mut parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k}={}", compact(v)))
                .collect();
            parts.sort();
            format!("{tool}({})", parts.join(", "))
        }
        other => format!("{tool}({})", compact(other)),
    }
}

fn compact(v: &Json) -> String {
    let s = match v {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(60).collect::<String>())
    } else {
        s
    }
}
