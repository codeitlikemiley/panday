//! The Anthropic Messages API dialect, and its mapping to/from the model IR.
//!
//! Three things differ from Chat Completions in ways that matter to the
//! ledger and to ADR-008, and each is handled explicitly below:
//!
//! 1. **`system` is top-level**, not a message role.
//! 2. **`max_tokens` is required.**
//! 3. **Cache is explicit**: `cache_control` breakpoints, with a TTL that
//!    changes the price of a write (1.25x at 5m, 2x at 1h).

use panday_types::model::{
    CacheHints, ChatRequest, ContentBlock, Message, Role, StopReason, ToolDef, Usage,
};
use serde::{Deserialize, Serialize};

/// Anthropic requires an explicit version header on every request.
pub const API_VERSION: &str = "2023-06-01";

/// Messages API requires `max_tokens`; the IR leaves it optional. This is the
/// value used when the caller did not say. Deliberately generous: truncating
/// a coding answer to save output tokens is a false economy, and the harness
/// sets a real budget when it cares.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<WireBlock>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireMessage {
    /// Only `user` and `assistant` exist here; tool results ride as `user`.
    pub role: &'static str,
    pub content: Vec<WireBlock>,
}

/// A content block. `cache_control` on a block marks a cache breakpoint that
/// covers everything up to and including it (ADR-008).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CacheControl {
    pub r#type: &'static str,
    /// Absent means the 5-minute default (1.25x write). `"1h"` costs 2x to
    /// write, so it is only ever set when the caller explicitly asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<&'static str>,
}

fn omits_temperature(model: &str) -> bool {
    model.starts_with("claude-opus-5")
        || model.starts_with("claude-sonnet-5")
        || model.starts_with("claude-fable-")
        || model.starts_with("claude-mythos-")
}

impl CacheControl {
    fn new(extended: bool) -> Self {
        Self {
            r#type: "ephemeral",
            ttl: extended.then_some("1h"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

fn flatten_text(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        let piece = match block {
            ContentBlock::Text { text } => text.as_str(),
            ContentBlock::ToolOutput { text, .. } => text.as_str(),
            ContentBlock::Artifact { summary, .. } => summary.as_str(),
        };
        if !out.is_empty() && !piece.is_empty() {
            out.push('\n');
        }
        out.push_str(piece);
    }
    out
}

impl WireRequest {
    pub fn from_ir(req: &ChatRequest) -> Self {
        let model = match req.model.split() {
            Some((_, name)) => name.to_string(),
            None => req.model.0.clone(),
        };

        // System messages are hoisted out of the transcript entirely.
        let system: Vec<WireBlock> = req
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| WireBlock::Text {
                text: flatten_text(&m.content),
                cache_control: None,
            })
            .collect();

        let conversation: Vec<&Message> = req
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .collect();

        let messages: Vec<WireMessage> = conversation
            .iter()
            .map(|m| match m.role {
                Role::Tool => WireMessage {
                    // A tool result is a `user` turn carrying a tool_result
                    // block — there is no `tool` role in this dialect.
                    role: "user",
                    content: vec![WireBlock::ToolResult {
                        // Quote the PROVIDER's id. Our UUID means nothing to
                        // Anthropic and the call would be rejected.
                        tool_use_id: m.provider_call_id.clone().unwrap_or_default(),
                        content: flatten_text(&m.content),
                        cache_control: None,
                    }],
                },
                Role::Assistant => WireMessage {
                    role: "assistant",
                    content: vec![WireBlock::Text {
                        text: flatten_text(&m.content),
                        cache_control: None,
                    }],
                },
                _ => WireMessage {
                    role: "user",
                    content: vec![WireBlock::Text {
                        text: flatten_text(&m.content),
                        cache_control: None,
                    }],
                },
            })
            .collect();

        let temperature = if omits_temperature(&model) {
            None
        } else {
            req.sampling.temperature
        };
        let mut out = WireRequest {
            model,
            max_tokens: req.sampling.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            messages,
            system,
            tools: req
                .tools
                .iter()
                .map(|t: &ToolDef| WireTool {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    input_schema: t.parameters.clone(),
                })
                .collect(),
            stream: true,
            temperature,
            top_p: req.sampling.top_p,
            stop_sequences: req.sampling.stop.clone(),
        };
        out.apply_cache_hints(&req.cache);
        out
    }

    /// Place cache breakpoints (ADR-008).
    ///
    /// `breakpoints_after` indexes the IR's message list, whose stable head is
    /// the system prompt + tool schemas. Index 0 therefore almost always means
    /// "after the system block", which is exactly the stable prefix worth
    /// caching. Indexes past the end are ignored rather than rejected: a
    /// stale hint should not fail a request that is otherwise fine.
    fn apply_cache_hints(&mut self, hints: &CacheHints) {
        for &idx in &hints.breakpoints_after {
            let cc = CacheControl::new(hints.extended_ttl);
            if idx == 0 && !self.system.is_empty() {
                if let Some(WireBlock::Text { cache_control, .. }) = self.system.last_mut() {
                    *cache_control = Some(cc);
                    continue;
                }
            }
            // Otherwise mark the last block of that conversation message.
            let target = (idx as usize).saturating_sub(usize::from(!self.system.is_empty()));
            if let Some(msg) = self.messages.get_mut(target) {
                if let Some(block) = msg.content.last_mut() {
                    match block {
                        WireBlock::Text { cache_control, .. }
                        | WireBlock::ToolResult { cache_control, .. } => *cache_control = Some(cc),
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming response
// ---------------------------------------------------------------------------

/// One decoded `data:` record. The `type` tag drives everything.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireEvent {
    MessageStart {
        message: WireMessageStart,
    },
    ContentBlockStart {
        index: u32,
        content_block: WireContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: WireDelta,
    },
    ContentBlockStop {
        #[allow(dead_code)]
        index: u32,
    },
    MessageDelta {
        delta: WireMessageDelta,
        #[serde(default)]
        usage: Option<WireUsage>,
    },
    MessageStop,
    Ping,
    /// A mid-stream error frame — the API reports overload this way *after*
    /// a 200, so it must not be mistaken for content.
    Error {
        error: WireError,
    },
    /// Forward compatibility: an event type this build does not know must not
    /// break the stream (same discipline as AEP, docs/03).
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireMessageStart {
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireContentBlock {
    Text {
        #[serde(default)]
        #[allow(dead_code)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WireMessageDelta {
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireError {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub message: String,
}

/// Usage as Anthropic reports it: the counts are **disjoint**.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct WireUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    /// Flat form: total cache writes, TTL unspecified.
    pub cache_creation_input_tokens: u64,
    /// Split form, when the API breaks writes out by TTL tier.
    pub cache_creation: Option<WireCacheCreation>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct WireCacheCreation {
    pub ephemeral_5m_input_tokens: u64,
    pub ephemeral_1h_input_tokens: u64,
}

impl WireUsage {
    /// Normalize into the IR.
    ///
    /// **This is the load-bearing part of the adapter for the ledger.**
    /// `panday_types::model::Usage` states the convention: cache counts are
    /// SUBSETS of `input_tokens`, and "Adapters for providers that report
    /// disjoint counts (e.g. Anthropic reports cache reads/writes separately
    /// from `input_tokens`) MUST normalize by adding them into
    /// `input_tokens`." Anthropic's `input_tokens` is the *fresh* remainder
    /// only, so the total is the sum of all three.
    ///
    /// Getting this wrong under-reports input by exactly the cached portion —
    /// which on a long agent session is most of it.
    pub fn to_ir(self) -> Usage {
        let (w5m, w1h) = match self.cache_creation {
            Some(c) => (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens),
            // Flat form: attribute to the 5m tier, the cheaper assumption.
            // Over-charging a tenant on a guess is worse than under-charging.
            None => (self.cache_creation_input_tokens, 0),
        };
        Usage {
            input_tokens: self.input_tokens + self.cache_read_input_tokens + w5m + w1h,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_input_tokens,
            cache_write_tokens: w5m,
            cache_write_1h_tokens: w1h,
        }
    }
}

/// Map a Messages API stop reason to the IR.
pub fn stop_reason(raw: &str) -> StopReason {
    match raw {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        // A refusal is a completed turn whose content is the refusal — not a
        // failure. `Error` would make the harness retry something that will
        // refuse again.
        "refusal" => StopReason::EndTurn,
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use panday_types::id::{AccountId, CallId, RequestId};
    use panday_types::model::{CallMeta, ModelRef, Sampling};

    fn uuid() -> uuid::Uuid {
        uuid::Uuid::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0001)
    }

    fn req(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: ModelRef("anthropic/claude-sonnet-4-5".into()),
            messages,
            tools: vec![],
            sampling: Sampling::default(),
            cache: CacheHints::default(),
            stream: true,
            metadata: CallMeta {
                account: AccountId(uuid()),
                request: RequestId(uuid()),
                session: None,
                turn: None,
                task: None,
            },
        }
    }

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            call_id: None,
            provider_call_id: None,
        }
    }

    #[test]
    fn hoists_system_out_of_the_transcript() {
        // `system` is top-level here, not a message role. Leaving it in the
        // list would make the API reject the request.
        let w = WireRequest::from_ir(&req(vec![
            msg(Role::System, "you are a coding agent"),
            msg(Role::User, "hi"),
        ]));
        assert_eq!(w.system.len(), 1);
        assert_eq!(w.messages.len(), 1, "system must not remain a message");
        assert_eq!(w.messages[0].role, "user");
    }

    #[test]
    fn always_sends_max_tokens_because_the_api_requires_it() {
        let w = WireRequest::from_ir(&req(vec![msg(Role::User, "hi")]));
        assert_eq!(w.max_tokens, DEFAULT_MAX_TOKENS);

        let mut r = req(vec![msg(Role::User, "hi")]);
        r.sampling.max_tokens = Some(256);
        assert_eq!(WireRequest::from_ir(&r).max_tokens, 256);
    }

    #[test]
    fn a_tool_result_becomes_a_user_turn_quoting_the_providers_id() {
        // There is no `tool` role in this dialect, and the id must be
        // Anthropic's `toolu_…`, never our UUID.
        let mut m = msg(Role::Tool, "3 passed");
        m.call_id = Some(CallId(uuid()));
        m.provider_call_id = Some("toolu_01XYZ".into());

        let w = WireRequest::from_ir(&req(vec![m]));
        assert_eq!(w.messages[0].role, "user");
        match &w.messages[0].content[0] {
            WireBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_01XYZ");
                assert_eq!(content, "3 passed");
            }
            other => panic!("expected a tool_result block, got {other:?}"),
        }
    }

    #[test]
    fn strips_the_provider_prefix_from_the_model_name() {
        let w = WireRequest::from_ir(&req(vec![msg(Role::User, "hi")]));
        assert_eq!(w.model, "claude-sonnet-4-5");
    }

    // -- cache breakpoints (ADR-008) ----------------------------------------

    #[test]
    fn a_breakpoint_at_zero_marks_the_stable_system_prefix() {
        let mut r = req(vec![
            msg(Role::System, "system + tool schemas"),
            msg(Role::User, "hi"),
        ]);
        r.cache.breakpoints_after = vec![0];

        let w = WireRequest::from_ir(&r);
        match &w.system[0] {
            WireBlock::Text { cache_control, .. } => {
                let cc = cache_control.as_ref().expect("system must be breakpointed");
                assert_eq!(cc.r#type, "ephemeral");
                assert_eq!(cc.ttl, None, "default 5m tier unless asked otherwise");
            }
            other => panic!("unexpected system block {other:?}"),
        }
    }

    #[test]
    fn extended_ttl_is_only_requested_when_asked() {
        // A 1h write costs 2x vs 1.25x (ADR-007); never opt in silently.
        let mut r = req(vec![msg(Role::System, "s"), msg(Role::User, "hi")]);
        r.cache.breakpoints_after = vec![0];
        r.cache.extended_ttl = true;

        let w = WireRequest::from_ir(&r);
        match &w.system[0] {
            WireBlock::Text { cache_control, .. } => {
                assert_eq!(cache_control.as_ref().unwrap().ttl, Some("1h"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn no_hints_means_no_cache_control_anywhere() {
        let w = WireRequest::from_ir(&req(vec![msg(Role::System, "s"), msg(Role::User, "hi")]));
        let json = serde_json::to_string(&w).unwrap();
        assert!(
            !json.contains("cache_control"),
            "breakpoints must be opt-in: {json}"
        );
    }

    #[test]
    fn a_stale_breakpoint_index_is_ignored_not_fatal() {
        let mut r = req(vec![msg(Role::User, "hi")]);
        r.cache.breakpoints_after = vec![99];
        // Must not panic; a stale hint should not fail an otherwise fine call.
        let _ = WireRequest::from_ir(&r);
    }

    // -- usage normalization -------------------------------------------------

    #[test]
    fn flat_cache_creation_is_attributed_to_the_cheaper_tier() {
        // Guessing 1h would over-charge the tenant; 5m is the safe assumption.
        let u = WireUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 100,
            cache_creation_input_tokens: 50,
            cache_creation: None,
        }
        .to_ir();
        assert_eq!(u.cache_write_tokens, 50);
        assert_eq!(u.cache_write_1h_tokens, 0);
        assert_eq!(u.input_tokens, 160);
    }

    #[test]
    fn stop_reasons_map_to_the_ir() {
        assert_eq!(stop_reason("end_turn"), StopReason::EndTurn);
        assert_eq!(stop_reason("max_tokens"), StopReason::MaxTokens);
        assert_eq!(stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(stop_reason("stop_sequence"), StopReason::StopSequence);
        // A refusal is a completed turn, not a failure to retry.
        assert_eq!(stop_reason("refusal"), StopReason::EndTurn);
        assert_eq!(stop_reason("something_new"), StopReason::EndTurn);
    }
}
