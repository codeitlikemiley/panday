//! The embedded agent (M10.5, docs/10 Layer 4).
//!
//! ```ignore
//! let agent = Agent::builder()
//!     .model("auto")
//!     .tools(registry)
//!     .policy(Profile::Unleashed)   // their process, their rules
//!     .build(client);
//! let run = agent.run("where is order 123?").await?;
//! println!("{}", run.text);
//! ```
//!
//! ## Why this is in `panday-harness` and not `panday-sdk`
//!
//! docs/10 documents it as the SDK's Layer 4, and docs/10 also says it "embeds
//! `panday-harness` ... the same state machine that powers the cloud, which is the honesty
//! guarantee". Both cannot be literally true of one crate: `panday-harness` depends on
//! `panday-sdk` for `ModelClient`, so the reverse would be a cycle. It lives here, next to
//! the state machine it wraps, and a crate embedding an agent depends on both — the same
//! arrangement `#[panday_sdk::tool]` already has (M10.4).
//!
//! ## What it is, and what it deliberately is not
//!
//! It is a thin builder over `SessionActor`: no second loop, no second gate, no second
//! reducer. Anything an embedded agent does differently from the hosted one is drift, and
//! drift is the thing docs/10 says this design exists to prevent. The differences that
//! *are* intended come from the arguments — an in-memory store, and whatever permission
//! profile the embedder chose for their own process.

use crate::tools::ToolRegistry;
use crate::{
    CollectSink, HarnessError, MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget,
    TurnOutcome,
};
use panday_sdk::ModelClient;
use panday_types::event::Envelope;
use panday_types::model::{ModelRef, StopReason, Usage};
use panday_types::{AccountId, SessionId};
use std::sync::Arc;

/// One completed run.
#[derive(Debug, Clone)]
pub struct Run {
    /// The assistant's final text.
    pub text: String,
    pub stop: StopReason,
    /// Every event, so a caller can audit, render or persist the run — it is the same log
    /// the hosted product keeps, and `panday_harness::render` will print it.
    pub events: Vec<Envelope>,
    pub usage: Usage,
    /// Calls that stopped for a decision. Non-empty means the run is *paused*, not
    /// finished, and `decide` continues it.
    pub awaiting: Vec<panday_types::CallId>,
}

impl Run {
    pub fn finished(&self) -> bool {
        self.awaiting.is_empty()
    }

    /// How many tools ran. Cheap, and the thing an embedder most often asserts on.
    pub fn tool_calls(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e.event, panday_types::event::Event::ToolResult { .. }))
            .count()
    }
}

#[derive(Default)]
pub struct AgentBuilder {
    model: Option<ModelRef>,
    tools: Option<ToolRegistry>,
    profile: Option<Profile>,
    budget: Option<TurnBudget>,
    account: Option<AccountId>,
    pricing: Option<panday_reducer::Pricing>,
}

impl AgentBuilder {
    /// `auto` lets the router decide (docs/12); a concrete `provider/model` pins it.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(ModelRef(model.into()));
        self
    }

    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Some(tools);
        self
    }

    /// docs/10's `PermissionPolicy` is docs/13's `Profile`: one gate, not two.
    ///
    /// The default is `Dev` rather than `Unleashed` even though this runs in the
    /// embedder's own process — an embedded agent with no gate is a library that can delete
    /// its host's files because a model asked, and the caller who wants that should have to
    /// type it.
    pub fn policy(mut self, profile: Profile) -> Self {
        self.profile = Some(profile);
        self
    }

    pub fn budget(mut self, budget: TurnBudget) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Attribution. Optional here and required on the wire (`CallMeta`), because an
    /// embedded agent is billed to whoever owns the API key rather than per end user.
    pub fn account(mut self, account: AccountId) -> Self {
        self.account = Some(account);
        self
    }

    /// What the model costs, if the embedder knows. Turns on the reducer's dollar
    /// accounting (M15.6); without it, savings are reported as token volume only.
    pub fn pricing(mut self, pricing: panday_reducer::Pricing) -> Self {
        self.pricing = Some(pricing);
        self
    }

    pub fn build(self, client: Arc<dyn ModelClient>) -> Agent {
        let store = Arc::new(MemoryStore::new());
        let sink = Arc::new(CollectSink::new());
        let session = SessionId::new();

        let mut actor = SessionActor::new(
            session,
            self.account.unwrap_or_default(),
            self.model.unwrap_or_else(ModelRef::auto),
            store.clone(),
            client,
            self.tools.unwrap_or_default(),
            PermissionEngine::new(self.profile.unwrap_or(Profile::Dev)),
            // The shipping stack, not a simpler one: an embedded agent that reduced
            // differently would give different answers than the hosted one on the same
            // input, which is exactly the drift this design exists to prevent.
            Box::new(panday_reducer::SpillingReducer::new(
                panday_reducer::StructuralReducer::new(panday_reducer::GenericReducer::default()),
                Arc::new(panday_reducer::MemoryArtifactStore::default()),
            )),
            self.budget.unwrap_or_default(),
        );
        if let Some(pricing) = self.pricing {
            actor = actor.with_pricing(pricing);
        }
        actor.subscribe(sink.clone());

        Agent {
            actor,
            store,
            session,
        }
    }
}

/// An agent loop running in the caller's process.
pub struct Agent {
    actor: SessionActor,
    store: Arc<MemoryStore>,
    session: SessionId,
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::default()
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    /// The whole log so far. An embedder that wants to persist a run keeps this — it is the
    /// same shape `panday replay` reads (`JsonlStore` writes exactly these lines).
    pub fn events(&self) -> Vec<Envelope> {
        self.store.all()
    }

    /// Run one turn.
    ///
    /// Returns when the turn finishes *or* when it stops for a permission decision. Those
    /// are different outcomes and `Run::finished` distinguishes them: a caller that treated
    /// a paused run as a finished one would report a task complete that never ran.
    pub async fn run(&mut self, prompt: &str) -> Result<Run, HarnessError> {
        let before = self.store.all().len();
        let outcome = self.actor.handle_user_input(prompt).await?;
        Ok(self.collect(outcome, before))
    }

    /// Answer a permission request and continue.
    pub async fn decide(
        &mut self,
        call_id: panday_types::CallId,
        allow: bool,
    ) -> Result<Run, HarnessError> {
        use panday_types::event::{Actor, PermDecision};
        let before = self.store.all().len();
        let outcome = self
            .actor
            .decide(
                call_id,
                if allow {
                    PermDecision::Allow
                } else {
                    PermDecision::Deny
                },
                // The embedder is the human here: they wrote the code that called this.
                Actor::User,
            )
            .await?;
        Ok(self.collect(outcome, before))
    }

    /// Stop the current turn (docs/13 §cancellation).
    pub fn cancel(&self) -> crate::CancelHandle {
        self.actor.cancel_handle()
    }

    fn collect(&self, outcome: TurnOutcome, from: usize) -> Run {
        use panday_types::event::Event;
        let events = self.store.all();
        let fresh = &events[from.min(events.len())..];

        let mut text = String::new();
        let mut usage = Usage::default();
        for envelope in fresh {
            if let Event::AssistantMessage { content, usage: u } = &envelope.event {
                usage.add(*u);
                // The last assistant message is the answer; earlier ones are the model
                // narrating its tool use, which a caller asking for `text` does not want
                // concatenated in.
                text = content
                    .iter()
                    .filter_map(|b| match b {
                        panday_types::model::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect();
            }
        }

        let (stop, awaiting) = match outcome {
            TurnOutcome::Finished(reason) => (reason, Vec::new()),
            // `ToolUse` is the honest reason for a paused turn: it stopped because a tool
            // needs approval, and reporting `EndTurn` would say it finished.
            TurnOutcome::AwaitingPermission(ids) => (StopReason::ToolUse, ids),
        };

        Run {
            text,
            stop,
            events: fresh.to_vec(),
            usage,
            awaiting,
        }
    }
}
