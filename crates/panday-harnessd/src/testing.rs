//! A scripted `SessionDriver`, public for the same reason `panday_harness::testing`
//! is (docs/02 §testing philosophy): the SDK's client suite and the CLI's dogfood
//! test both need a server that produces real events without a model, and two copies
//! of it would drift.

use crate::{AppState, SessionDriver};
use panday_types::event::{Actor, ClientKind, Envelope, Event, PermDecision};
use panday_types::model::{ContentBlock, ModelRef, StopReason, Usage};
use panday_types::{CallId, SessionId};
use std::sync::atomic::{AtomicU64, Ordering};

/// Appends a fixed turn per user input.
///
/// The `seq` counter lives here rather than being derived per call because the log's
/// gapless-`seq` invariant is enforced by the store: two drivers on one session both
/// starting at 1 is a single-writer violation, and it is the store's job to say so.
#[derive(Default)]
pub struct ScriptedDriver {
    seq: AtomicU64,
    /// Park on a permission request instead of finishing the turn.
    pub park: bool,
}

impl ScriptedDriver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn parking() -> Self {
        Self {
            seq: AtomicU64::new(0),
            park: true,
        }
    }

    /// Append one event, minting the next `seq`.
    pub async fn append(&self, state: &AppState, session: SessionId, event: Event) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        state
            .publish(
                session,
                Envelope {
                    v: 1,
                    session_id: session,
                    seq,
                    at: time::OffsetDateTime::UNIX_EPOCH,
                    turn_id: None,
                    event,
                },
            )
            .await
            .expect("publish");
        seq
    }
}

#[async_trait::async_trait]
impl SessionDriver for ScriptedDriver {
    async fn user_input(&self, state: &AppState, session: SessionId, text: String) {
        self.append(
            state,
            session,
            Event::UserMessage {
                content: vec![ContentBlock::Text { text }],
                source: ClientKind::Api,
            },
        )
        .await;
        self.append(
            state,
            session,
            Event::TurnStarted {
                model: ModelRef("local/test".into()),
                parent: None,
            },
        )
        .await;
        if self.park {
            self.append(
                state,
                session,
                Event::PermissionRequest {
                    call_id: CallId(uuid::Uuid::nil()),
                    tool: "bash".into(),
                    action: "bash(cmd=cargo test)".into(),
                    options: vec!["allow".into(), "deny".into()],
                },
            )
            .await;
            return;
        }
        self.append(
            state,
            session,
            Event::AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "on it".into(),
                }],
                usage: Usage {
                    input_tokens: 1_000,
                    output_tokens: 20,
                    cache_read_tokens: 900,
                    ..Default::default()
                },
            },
        )
        .await;
        self.append(
            state,
            session,
            Event::TurnFinished {
                reason: StopReason::EndTurn,
                usage: Usage::default(),
                cost_micros: 0,
            },
        )
        .await;
    }

    async fn decision(
        &self,
        state: &AppState,
        session: SessionId,
        call_id: CallId,
        decision: PermDecision,
    ) {
        self.append(
            state,
            session,
            Event::PermissionDecision {
                call_id,
                decision,
                by: Actor::User,
            },
        )
        .await;
        self.append(
            state,
            session,
            Event::TurnFinished {
                reason: StopReason::EndTurn,
                usage: Usage::default(),
                cost_micros: 0,
            },
        )
        .await;
    }
}
