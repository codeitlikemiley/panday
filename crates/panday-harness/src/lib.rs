//! # panday-harness
//!
//! The heart: an event-sourced session state machine (docs/13-harness.md).
//! This seed ships the tool/permission/store traits, the turn phase enum,
//! and — critically — the `fold`: state reconstruction from the event log,
//! which everything else (resume, replay, audit, billing) hangs off.

use async_trait::async_trait;
use panday_types::event::{Envelope, Event, PermDecision};
use panday_types::model::{StopReason, Usage};
use panday_types::{CallId, Json, SessionId};
use serde::{Deserialize, Serialize};

pub mod actor;
pub mod expand;
pub mod permissions;
pub mod testing;
pub mod tools;

pub use actor::{CollectSink, EventSink, HarnessError, MemoryStore, SessionActor, TurnOutcome};
pub use expand::ExpandArtifact;
pub use permissions::{Gate, PermissionEngine, Profile};
pub use tools::{Tool, ToolCtx, ToolOutcome, ToolReq, ToolSpec};

/// Where events live. PG in cloud, SQLite/file in `panday local`; in-memory
/// in tests. Contract: `append` is fsync-durable before it returns
/// (docs/13 §persist-before-proceed).
#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(&self, e: Envelope) -> Result<(), StoreError>;
    async fn read_after(
        &self,
        session: SessionId,
        after_seq: u64,
    ) -> Result<Vec<Envelope>, StoreError>;
    async fn next_seq(&self, session: SessionId) -> Result<u64, StoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store io: {0}")]
    Io(String),
    #[error("seq conflict at {0} (single-writer invariant violated)")]
    SeqConflict(u64),
}

/// The turn state machine phases (docs/13 §state machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Assembling,
    Streaming,
    Gating,
    Executing,
    Finalizing,
}

/// Bounds checked at every transition; exceeding one is a normal stop,
/// never a panic (docs/13 §invariants).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TurnBudget {
    pub max_steps: u32,
    pub max_wall_ms: u64,
    pub max_spend_micros: u64,
}

impl Default for TurnBudget {
    fn default() -> Self {
        Self {
            max_steps: 20,
            max_wall_ms: 600_000,
            max_spend_micros: 2_000_000,
        }
    }
}

/// Materialized view of a session — always reconstructable via [`fold`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionState {
    pub last_seq: u64,
    pub phase: Phase,
    pub steps_this_turn: u32,
    pub usage_total: Usage,
    pub cost_micros_total: u64,
    /// Calls awaiting a permission decision (parked, NOT dispatched).
    pub pending_permissions: Vec<CallId>,
    /// Calls dispatched (or approved for dispatch) with no ToolResult yet —
    /// the crash-replay set. Permission-parked calls are excluded; a
    /// finished turn clears it.
    pub inflight_calls: Vec<(CallId, String, Json)>,
    pub finished_turns: u32,
    pub last_stop: Option<StopReason>,
}

/// State = fold(log). If a state can't be reconstructed from the log, the
/// missing event is a bug (ADR-002). Deltas are ephemeral and never appear
/// in a persisted log.
pub fn fold(events: &[Envelope]) -> SessionState {
    let mut s = SessionState::default();
    for env in events {
        s.last_seq = env.seq;
        match &env.event {
            Event::UserMessage { .. } => {
                s.phase = Phase::Assembling;
            }
            Event::TurnStarted { .. } => {
                s.phase = Phase::Streaming;
                s.steps_this_turn = 0;
            }
            Event::ToolCall {
                call_id,
                tool,
                args,
                ..
            } => {
                s.phase = Phase::Executing;
                s.steps_this_turn += 1;
                s.inflight_calls
                    .push((*call_id, tool.clone(), args.clone()));
            }
            Event::ToolResult { call_id, .. } => {
                s.inflight_calls.retain(|(id, _, _)| id != call_id);
                if s.inflight_calls.is_empty() {
                    s.phase = Phase::Assembling;
                }
            }
            Event::PermissionRequest { call_id, .. } => {
                s.phase = Phase::Gating;
                s.pending_permissions.push(*call_id);
                // Parked, not dispatched: not part of the crash-replay set
                // until a decision allows it.
                s.inflight_calls.retain(|(id, _, _)| id != call_id);
            }
            Event::PermissionDecision {
                call_id, decision, ..
            } => {
                s.pending_permissions.retain(|id| id != call_id);
                if *decision == PermDecision::Deny {
                    s.inflight_calls.retain(|(id, _, _)| id != call_id);
                }
                // On Allow the harness re-emits dispatch state via the
                // subsequent ToolResult; between decision and result the call
                // is intentionally NOT in the replay set unless re-observed —
                // the actor re-adds it when it actually dispatches (M13.3).
            }
            Event::AssistantMessage { usage, .. } => {
                // Usage is counted from per-model-call AssistantMessage
                // events ONLY. TurnFinished.usage is a redundant summary
                // (must equal the sum of the turn's AssistantMessage usage;
                // property-tested at M3.5) and is NOT re-added here —
                // counting both double-bills.
                s.usage_total.add(*usage);
            }
            Event::TurnFinished {
                reason,
                cost_micros,
                ..
            } => {
                s.cost_micros_total += cost_micros;
                s.finished_turns += 1;
                s.last_stop = Some(*reason);
                s.phase = Phase::Idle;
                // A finished turn (any stop reason, including Cancelled and
                // BudgetExceeded) has nothing left to replay or approve.
                s.inflight_calls.clear();
                s.pending_permissions.clear();
            }
            _ => {}
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use panday_types::event::{ClientKind, ReducedOutput};
    use panday_types::model::{ContentBlock, ModelRef};
    use panday_types::TurnId;

    fn env(sid: SessionId, seq: u64, ev: Event) -> Envelope {
        Envelope::new(sid, seq, Some(TurnId::new()), ev)
    }

    #[test]
    fn fold_reconstructs_a_simple_tool_turn() {
        let sid = SessionId::new();
        let call = CallId::new();
        let log = vec![
            env(
                sid,
                1,
                Event::UserMessage {
                    content: vec![ContentBlock::Text {
                        text: "fix it".into(),
                    }],
                    source: ClientKind::Cli,
                },
            ),
            env(
                sid,
                2,
                Event::TurnStarted {
                    model: ModelRef::auto(),
                    parent: None,
                },
            ),
            env(
                sid,
                3,
                Event::ToolCall {
                    call_id: call,
                    tool: "bash".into(),
                    args: serde_json::json!({"cmd": "cargo test"}),
                    provider_call_id: None,
                },
            ),
            env(
                sid,
                4,
                Event::ToolResult {
                    call_id: call,
                    output: ReducedOutput {
                        text: "1 failed: test_x".into(),
                        tokens_raw: 900,
                        tokens_kept: 40,
                        strategy: "generic_headtail_v1".into(),
                    },
                    raw_ref: None,
                    duration_ms: 1200,
                    is_error: true,
                },
            ),
            env(
                sid,
                5,
                Event::AssistantMessage {
                    content: vec![ContentBlock::Text {
                        text: "fixed".into(),
                    }],
                    usage: Usage {
                        input_tokens: 100,
                        output_tokens: 50,
                        ..Default::default()
                    },
                },
            ),
            env(
                sid,
                6,
                Event::TurnFinished {
                    reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 100,
                        output_tokens: 50,
                        ..Default::default()
                    },
                    cost_micros: 777,
                },
            ),
        ];
        let s = fold(&log);
        assert_eq!(s.last_seq, 6);
        assert_eq!(s.phase, Phase::Idle);
        assert!(s.inflight_calls.is_empty());
        assert_eq!(s.finished_turns, 1);
        assert_eq!(s.cost_micros_total, 777);
        assert_eq!(s.last_stop, Some(StopReason::EndTurn));
        // usage counted ONCE (from AssistantMessage), not re-added at TurnFinished
        assert_eq!(s.usage_total.input_tokens, 100);
        assert_eq!(s.usage_total.output_tokens, 50);
    }

    #[test]
    fn finished_turn_clears_replay_and_permission_sets() {
        let sid = SessionId::new();
        let call = CallId::new();
        let log = vec![
            env(
                sid,
                1,
                Event::TurnStarted {
                    model: ModelRef::auto(),
                    parent: None,
                },
            ),
            env(
                sid,
                2,
                Event::ToolCall {
                    call_id: call,
                    tool: "bash".into(),
                    args: serde_json::json!({"cmd": "sleep 999"}),
                    provider_call_id: None,
                },
            ),
            env(
                sid,
                3,
                Event::TurnFinished {
                    reason: StopReason::Cancelled,
                    usage: Usage::default(),
                    cost_micros: 0,
                },
            ),
        ];
        let s = fold(&log);
        assert_eq!(s.phase, Phase::Idle);
        assert!(
            s.inflight_calls.is_empty(),
            "cancelled turn must not leave replayable calls"
        );
        assert!(s.pending_permissions.is_empty());
    }

    #[test]
    fn permission_parked_calls_are_not_in_the_replay_set() {
        let sid = SessionId::new();
        let call = CallId::new();
        let log = vec![
            env(
                sid,
                1,
                Event::TurnStarted {
                    model: ModelRef::auto(),
                    parent: None,
                },
            ),
            env(
                sid,
                2,
                Event::ToolCall {
                    call_id: call,
                    tool: "write_file".into(),
                    args: serde_json::json!({"path": "x"}),
                    provider_call_id: None,
                },
            ),
            env(
                sid,
                3,
                Event::PermissionRequest {
                    call_id: call,
                    tool: "write_file".into(),
                    action: "write x".into(),
                    options: vec!["allow".into(), "deny".into()],
                },
            ),
            // crash here, before any decision
        ];
        let s = fold(&log);
        assert_eq!(s.phase, Phase::Gating);
        assert!(
            s.inflight_calls.is_empty(),
            "parked call must not be replayed on resume"
        );
        assert_eq!(s.pending_permissions, vec![call]);
    }

    #[test]
    fn crash_mid_execution_leaves_inflight_calls_for_replay() {
        let sid = SessionId::new();
        let call = CallId::new();
        let log = vec![
            env(
                sid,
                1,
                Event::TurnStarted {
                    model: ModelRef::auto(),
                    parent: None,
                },
            ),
            env(
                sid,
                2,
                Event::ToolCall {
                    call_id: call,
                    tool: "bash".into(),
                    args: serde_json::json!({"cmd": "cargo build"}),
                    provider_call_id: None,
                },
            ),
            // crash here: no ToolResult
        ];
        let s = fold(&log);
        assert_eq!(s.phase, Phase::Executing);
        assert_eq!(
            s.inflight_calls.len(),
            1,
            "resume must replay or refuse this call"
        );
    }
}
