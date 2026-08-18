//! The OpenAI Chat Completions dialect, and its mapping to/from the model IR.
//!
//! One adapter, many bases (docs/11): Together, Fireworks, Groq, vLLM,
//! llama-server, mistral.rs. Only the base URL and auth differ.
//!
//! These types are deliberately *not* re-exported from the crate root: nothing
//! outside this module should see a provider-shaped type. docs/10 acceptance:
//! "no public API returns a provider-specific type".

use ferrum_types::model::{ChatRequest, ContentBlock, Message, Role, StopReason, ToolDef, Usage};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireRequest {
    /// Bare model name — the `provider/` prefix is stripped; the base URL
    /// already decided which server this goes to.
    pub model: String,
    pub messages: Vec<WireMessage>,
    pub stream: bool,
    /// Ask for a final usage chunk. Without this an OpenAI-compatible server
    /// streams no usage at all and the ledger silently books zero (docs/11:
    /// "usage capture ... what it cost").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireMessage {
    pub role: &'static str,
    pub content: String,
    /// Present only on tool replies: which call this answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireTool {
    pub r#type: &'static str,
    pub function: WireFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Flatten IR content blocks to the dialect's plain-string content.
///
/// The IR is block-structured so images/artifacts do not force a protocol bump
/// (docs/03). Chat Completions' `content` is a string for text-only turns, so
/// blocks concatenate. `Artifact` blocks contribute their summary: the handle
/// is meaningless to a provider, but dropping the block entirely would hide
/// from the model that something was elided.
fn flatten(content: &[ContentBlock]) -> String {
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

fn to_wire_message(m: &Message) -> WireMessage {
    WireMessage {
        role: role_str(m.role),
        content: flatten(&m.content),
        // Quote the PROVIDER's id, never our UUID — the server issued
        // `call_abc123` and will reject anything else. Falling back to our
        // UUID would be wrong on the wire, so an absent provider id stays
        // absent and the server tells us plainly.
        tool_call_id: m.provider_call_id.clone(),
    }
}

fn to_wire_tool(t: &ToolDef) -> WireTool {
    WireTool {
        r#type: "function",
        function: WireFunction {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
        },
    }
}

impl WireRequest {
    /// Map the IR request into the dialect. `stream` is forced on: this
    /// adapter only implements the streaming path (M11.1).
    pub fn from_ir(req: &ChatRequest) -> Self {
        // "provider/model" -> "model"; a bare name passes through unchanged.
        let model = match req.model.split() {
            Some((_, name)) => name.to_string(),
            None => req.model.0.clone(),
        };

        WireRequest {
            model,
            messages: req.messages.iter().map(to_wire_message).collect(),
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            temperature: req.sampling.temperature,
            top_p: req.sampling.top_p,
            max_tokens: req.sampling.max_tokens,
            stop: req.sampling.stop.clone(),
            tools: req.tools.iter().map(to_wire_tool).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming response
// ---------------------------------------------------------------------------

/// One `data:` record from the stream.
///
/// Every field is optional: servers in this family disagree about which are
/// present on which chunk, and a missing field must never fail the stream.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WireChunk {
    pub choices: Vec<WireChoice>,
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WireChoice {
    pub delta: WireDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WireDelta {
    pub content: Option<String>,
    pub tool_calls: Vec<WireToolCallDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WireToolCallDelta {
    /// Position in the assistant's tool-call list. THE correlation key while
    /// streaming: `id` and `name` arrive only on the first fragment.
    pub index: u32,
    pub id: Option<String>,
    pub function: Option<WireFunctionDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WireFunctionDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct WireUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub prompt_tokens_details: Option<WirePromptDetails>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct WirePromptDetails {
    pub cached_tokens: u64,
}

impl WireUsage {
    /// Map to the IR's `Usage`.
    ///
    /// The IR's CONVENTION (see `ferrum_types::model::Usage`) is that cache
    /// counts are SUBSETS of `input_tokens`. This dialect already reports
    /// `cached_tokens` as a subset of `prompt_tokens`, so no normalization is
    /// needed here — unlike the Anthropic adapter (M11.2), which must add its
    /// disjoint counts in.
    ///
    /// Cache *writes* stay zero: automatic prefix caching carries no write
    /// premium (ADR-007), so there is nothing for the ledger to price.
    pub fn to_ir(self) -> Usage {
        Usage {
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
            cache_read_tokens: self
                .prompt_tokens_details
                .map(|d| d.cached_tokens)
                .unwrap_or(0),
            cache_write_tokens: 0,
            cache_write_1h_tokens: 0,
        }
    }
}

/// Map a `finish_reason` to the IR.
///
/// An unrecognised reason maps to `EndTurn` rather than `Error`: the stream did
/// finish, and inventing a failure the server never reported would make the
/// harness retry a completed turn.
pub fn stop_reason(raw: &str) -> StopReason {
    match raw {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::Error,
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrum_types::id::{AccountId, CallId, RequestId};
    use ferrum_types::model::{CallMeta, ModelRef, Sampling};

    fn req(model: &str) -> ChatRequest {
        ChatRequest {
            model: ModelRef(model.into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
                call_id: None,
                provider_call_id: None,
            }],
            tools: vec![],
            sampling: Sampling::default(),
            cache: Default::default(),
            stream: false,
            metadata: CallMeta {
                account: AccountId(uuid_for_test()),
                request: RequestId(uuid_for_test()),
                session: None,
                turn: None,
                task: None,
            },
        }
    }

    fn uuid_for_test() -> uuid::Uuid {
        uuid::Uuid::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0001)
    }

    #[test]
    fn strips_the_provider_prefix_from_the_model_name() {
        assert_eq!(
            WireRequest::from_ir(&req("local/qwen3.5-4b")).model,
            "qwen3.5-4b"
        );
        // A bare name (llama-server often ignores it entirely) passes through.
        assert_eq!(WireRequest::from_ir(&req("qwen3.5-4b")).model, "qwen3.5-4b");
    }

    #[test]
    fn always_requests_usage_in_the_stream() {
        // Without this the ledger books zero for every local call.
        let w = WireRequest::from_ir(&req("local/m"));
        assert!(w.stream);
        assert_eq!(
            w.stream_options,
            Some(StreamOptions {
                include_usage: true
            })
        );
    }

    #[test]
    fn omits_unset_sampling_fields_entirely() {
        // Sending `"temperature": null` makes some servers in this family 400.
        let json = serde_json::to_value(WireRequest::from_ir(&req("local/m"))).unwrap();
        assert!(json.get("temperature").is_none());
        assert!(json.get("max_tokens").is_none());
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn flattens_content_blocks_to_a_string() {
        let mut r = req("local/m");
        r.messages[0].content = vec![
            ContentBlock::Text {
                text: "first".into(),
            },
            ContentBlock::ToolOutput {
                call_id: CallId(uuid_for_test()),
                text: "second".into(),
            },
            ContentBlock::Artifact {
                artifact: ferrum_types::id::ArtifactRef {
                    hash: "abc".into(),
                    size: 1,
                    media_type: None,
                },
                summary: "third".into(),
            },
        ];
        assert_eq!(
            WireRequest::from_ir(&r).messages[0].content,
            "first\nsecond\nthird"
        );
    }

    #[test]
    fn cached_tokens_are_a_subset_of_input_not_an_addition() {
        // The IR convention: cache counts are SUBSETS of input_tokens.
        let u = WireUsage {
            prompt_tokens: 1000,
            completion_tokens: 50,
            prompt_tokens_details: Some(WirePromptDetails { cached_tokens: 800 }),
        }
        .to_ir();
        assert_eq!(u.input_tokens, 1000, "input must stay the total, not 200");
        assert_eq!(u.cache_read_tokens, 800);
        assert_eq!(u.output_tokens, 50);
        // Automatic prefix caching has no write premium (ADR-007).
        assert_eq!(u.cache_write_tokens, 0);
        assert_eq!(u.cache_write_1h_tokens, 0);
    }

    #[test]
    fn usage_without_cache_details_reads_as_zero_cached() {
        let u = WireUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            prompt_tokens_details: None,
        }
        .to_ir();
        assert_eq!(u.cache_read_tokens, 0);
        assert_eq!(u.input_tokens, 10);
    }

    #[test]
    fn maps_finish_reasons() {
        assert_eq!(stop_reason("stop"), StopReason::EndTurn);
        assert_eq!(stop_reason("length"), StopReason::MaxTokens);
        assert_eq!(stop_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(stop_reason("content_filter"), StopReason::Error);
        // Unknown reasons must not fabricate a failure.
        assert_eq!(stop_reason("something_new"), StopReason::EndTurn);
    }

    #[test]
    fn chunks_parse_with_every_field_missing() {
        // Servers in this family disagree about which fields appear when.
        let c: WireChunk = serde_json::from_str("{}").unwrap();
        assert!(c.choices.is_empty());
        assert!(c.usage.is_none());

        let c: WireChunk = serde_json::from_str(r#"{"choices":[{"index":0,"delta":{}}]}"#).unwrap();
        assert_eq!(c.choices.len(), 1);
        assert!(c.choices[0].delta.content.is_none());
        assert!(c.choices[0].finish_reason.is_none());
    }

    #[test]
    fn parses_a_tool_call_fragment() {
        let c: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[
                 {"index":0,"id":"call_abc","type":"function",
                  "function":{"name":"run_tests","arguments":"{\"pkg\":"}}]}}]}"#,
        )
        .unwrap();
        let tc = &c.choices[0].delta.tool_calls[0];
        assert_eq!(tc.index, 0);
        assert_eq!(tc.id.as_deref(), Some("call_abc"));
        let f = tc.function.as_ref().unwrap();
        assert_eq!(f.name.as_deref(), Some("run_tests"));
        assert_eq!(f.arguments.as_deref(), Some("{\"pkg\":"));
    }

    #[test]
    fn ignores_unknown_chunk_fields() {
        // Provider drift must break a fixture, not production.
        let c: WireChunk = serde_json::from_str(
            r#"{"id":"x","object":"chat.completion.chunk","created":1,
                "model":"m","system_fingerprint":"fp","choices":[]}"#,
        )
        .unwrap();
        assert!(c.choices.is_empty());
    }
}
