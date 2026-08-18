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

    /// An event kind this build does not know.
    ///
    /// docs/03 §Versioning discipline: "New event kinds: minor; unknown kinds
    /// MUST be ignored-and-preserved by clients." A newer harness may emit
    /// events an older CLI has never heard of; that CLI must neither crash nor
    /// silently drop them from a log it may re-persist or relay.
    ///
    /// The whole object — including the `event` tag itself — is captured
    /// verbatim, so re-serializing yields the original bytes. Never construct
    /// this variant deliberately: it exists to be tolerant on read.
    #[serde(untagged)]
    Unknown {
        #[serde(flatten)]
        payload: serde_json::Map<String, Json>,
    },
}

impl Event {
    /// The wire tag for this event, whether or not this build knows the kind.
    /// Returns `None` only for a malformed `Unknown` that carries no `event`
    /// key — which the deserializer cannot actually produce.
    pub fn kind(&self) -> Option<&str> {
        Some(match self {
            Event::UserMessage { .. } => "user_message",
            Event::AssistantDelta { .. } => "assistant_delta",
            Event::AssistantMessage { .. } => "assistant_message",
            Event::ToolCall { .. } => "tool_call",
            Event::ToolResult { .. } => "tool_result",
            Event::PermissionRequest { .. } => "permission_request",
            Event::PermissionDecision { .. } => "permission_decision",
            Event::Compaction { .. } => "compaction",
            Event::TurnStarted { .. } => "turn_started",
            Event::TurnFinished { .. } => "turn_finished",
            Event::SubagentSpawned { .. } => "subagent_spawned",
            Event::SubagentFinished { .. } => "subagent_finished",
            Event::SessionForked { .. } => "session_forked",
            Event::Error { .. } => "error",
            Event::Unknown { payload } => return payload.get("event").and_then(|v| v.as_str()),
        })
    }

    /// True when this build did not recognise the event kind on the wire.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Event::Unknown { .. })
    }
}

/// Every event tag this build knows. Kept in sync with `Event` by
/// `tests/golden.rs::corpus_covers_every_event_variant`.
pub const KNOWN_EVENT_TAGS: &[&str] = &[
    "user_message",
    "assistant_delta",
    "assistant_message",
    "tool_call",
    "tool_result",
    "permission_request",
    "permission_decision",
    "compaction",
    "turn_started",
    "turn_finished",
    "subagent_spawned",
    "subagent_finished",
    "session_forked",
    "error",
];

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
