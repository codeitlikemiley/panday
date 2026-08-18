//! The model IR: one request/response vocabulary across Anthropic,
//! OpenAI-compatible, and local backends. See `docs/10-sdk.md` §Layer 1.

use crate::id::{AccountId, CallId, RequestId, SessionId, TurnId};
use crate::Json;
use serde::{Deserialize, Serialize};

/// A model reference: `provider/model` (e.g. `anthropic/claude-sonnet-4-5`,
/// `local/qwen3.5-4b`) or the literal `auto` to let the router decide.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct ModelRef(pub String);

impl ModelRef {
    pub fn auto() -> Self {
        Self("auto".into())
    }
    pub fn is_auto(&self) -> bool {
        self.0 == "auto"
    }
    /// (provider, model) split; `auto` has neither.
    pub fn split(&self) -> Option<(&str, &str)> {
        self.0.split_once('/')
    }
}

/// Coarse task classification driving the router (docs/12). Callers that
/// know, say; otherwise a classifier guesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TaskClass {
    Chat,
    Code,
    Summarize,
    Extract,
    Route,
    Embed,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Content is block-structured from day one so images/artifacts don't force
/// a protocol bump later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// Reduced tool output living inline (small) — large output is an artifact.
    ToolOutput {
        call_id: CallId,
        text: String,
    },
    /// Reference to spilled content; `expand_artifact` can pull ranges.
    Artifact {
        artifact: crate::id::ArtifactRef,
        summary: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    /// Present on `Role::Tool` messages: which call this answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<CallId>,
}

/// Tool definition as sent to models. Schema is JSON Schema; kept as a raw
/// value because open-ended schemas have no fixed shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Json,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Sampling {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
}

/// Prompt-cache layout hints (docs/04 ADR-008). Indexes are message
/// positions after which a cache breakpoint should be placed (Anthropic
/// explicit breakpoints; ignored where caching is automatic).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CacheHints {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakpoints_after: Vec<u32>,
    /// Request extended TTL where the provider offers it (costs more to write).
    #[serde(default)]
    pub extended_ttl: bool,
}

/// REQUIRED attribution on every model call. An unattributed call is a
/// compile error by construction — this is how cost attribution stays total.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CallMeta {
    pub account: AccountId,
    pub request: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskClass>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChatRequest {
    pub model: ModelRef,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(default)]
    pub sampling: Sampling,
    #[serde(default)]
    pub cache: CacheHints,
    #[serde(default)]
    pub stream: bool,
    pub metadata: CallMeta,
}

/// Token accounting with cache splits — the ledger prices these at different
/// rates: fresh input 1x; cache reads ~0.1x (both major providers); cache
/// writes are Anthropic-only surcharges at 1.25x (5-minute TTL) or 2x
/// (1-hour TTL) — OpenAI-style automatic caching has no write premium.
///
/// CONVENTION (normative, enforced at the adapter boundary): the cache
/// counts are SUBSETS of `input_tokens`. `input_tokens` is the total input;
/// `cache_read + cache_write_5m + cache_write_1h <= input_tokens`, and the
/// fresh remainder is the difference. Adapters for providers that report
/// disjoint counts (e.g. Anthropic reports cache reads/writes separately
/// from `input_tokens`) MUST normalize by adding them into `input_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Cache-write tokens at the short-TTL (5m-class) rate.
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Cache-write tokens at the extended-TTL (1h-class) rate — priced
    /// differently (~2x vs ~1.25x); requested via `CacheHints::extended_ttl`.
    #[serde(default)]
    pub cache_write_1h_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.cache_write_1h_tokens += other.cache_write_1h_tokens;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Cancelled,
    BudgetExceeded,
    MaxSteps,
    Error,
}

/// Items yielded by a streaming chat call (docs/10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamItem {
    Delta { text: String },
    ToolCallStart { id: CallId, name: String },
    ToolCallDelta { id: CallId, args_fragment: String },
    Usage { usage: Usage },
    Done { reason: StopReason },
}
