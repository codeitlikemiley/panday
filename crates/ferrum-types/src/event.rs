//! AEP — the Agent Event Protocol. The append-only event log IS the session
//! (docs/03, ADR-002). State is a fold over these events.

use crate::id::{ArtifactRef, CallId, SessionId, TurnId};
use crate::model::{ContentBlock, ModelRef, StopReason, Usage};
use crate::{Json, Timestamp};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;

/// Every event on the wire and in the log wears this envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Protocol version. Bump rules in docs/03 §Versioning.
    pub v: u16,
    pub session_id: SessionId,
    /// Gapless, per-session. THE ordering primitive; clients resume with
    /// `after_seq`.
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: Timestamp,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Cli,
    Acp,
    Web,
    Api,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermDecision {
    Allow,
    AllowRemember,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    User,
    Policy { rule: String },
    System,
}

/// Reduced tool output as it enters context; the raw is in `raw_ref`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReducedOutput {
    pub text: String,
    pub tokens_raw: u32,
    pub tokens_kept: u32,
    /// Which reducer strategy produced this (docs/15).
    pub strategy: String,
}

/// The event vocabulary. Unknown kinds MUST be ignored-and-preserved by
/// clients (tested in the golden suite).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    // ---- conversation ----
    UserMessage {
        content: Vec<ContentBlock>,
        source: ClientKind,
    },
    /// Streaming only — never persisted (docs/03: deltas are ephemeral).
    AssistantDelta {
        text: String,
    },
    AssistantMessage {
        content: Vec<ContentBlock>,
        usage: Usage,
    },

    // ---- tools ----
    ToolCall {
        call_id: CallId,
        tool: String,
        args: Json,
    },
    ToolResult {
        call_id: CallId,
        output: ReducedOutput,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_ref: Option<ArtifactRef>,
        duration_ms: u64,
        is_error: bool,
    },

    // ---- control ----
    PermissionRequest {
        call_id: CallId,
        tool: String,
        action: String,
        options: Vec<String>,
    },
    PermissionDecision {
        call_id: CallId,
        decision: PermDecision,
        by: Actor,
    },

    // ---- context economy ----
    Compaction {
        from_seq: u64,
        to_seq: u64,
        summary_ref: ArtifactRef,
        tokens_before: u32,
        tokens_after: u32,
    },

    // ---- lifecycle ----
    TurnStarted {
        model: ModelRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<TurnId>,
    },
    TurnFinished {
        reason: StopReason,
        usage: Usage,
        cost_micros: u64,
    },
    SubagentSpawned {
        child: SessionId,
        brief: String,
    },
    SubagentFinished {
        child: SessionId,
        result_ref: ArtifactRef,
    },
    SessionForked {
        from_seq: u64,
    },
    Error {
        code: String,
        message: String,
        retryable: bool,
    },
}

impl Envelope {
    pub fn new(session_id: SessionId, seq: u64, turn_id: Option<TurnId>, event: Event) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            session_id,
            seq,
            turn_id,
            at: time::OffsetDateTime::now_utc(),
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Usage;

    #[test]
    fn envelope_round_trips() {
        let sid = SessionId::new();
        let env = Envelope::new(
            sid,
            7,
            Some(TurnId::new()),
            Event::TurnFinished {
                reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 1200,
                    output_tokens: 300,
                    cache_read_tokens: 900,
                    ..Default::default()
                },
                cost_micros: 4200,
            },
        );
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env, back);
        // the tag must be flattened into the envelope object
        assert!(json.contains("\"event\":\"turn_finished\""), "{json}");
    }

    #[test]
    fn events_are_snake_case_tagged() {
        let ev = Event::UserMessage {
            content: vec![ContentBlock::Text { text: "hi".into() }],
            source: ClientKind::Cli,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["event"], "user_message");
        assert_eq!(json["source"], "cli");
    }
}
