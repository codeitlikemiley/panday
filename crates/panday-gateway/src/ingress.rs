//! OpenAI-compatible ingress (docs/11 §also serves, M11.5).
//!
//! > "`POST /v1/chat/completions` accepting the standard dialect, mapped to IR.
//! > Any existing tool (aider, continue.dev, curl scripts) can point at panday
//! > with an API key and inherit routing/metering/caching. This is the
//! > platform's cheapest adoption wedge and its best A/B harness (compare us vs
//! > direct)."
//!
//! The A/B property is why the surface has to be *exactly* the standard
//! dialect: a client must be redirectable by changing a base URL and nothing
//! else, or the comparison is not like-for-like.

use crate::Gateway;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use panday_sdk::{ModelClient, PandayError};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Incoming: the standard dialect
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct IngressRequest {
    pub model: String,
    pub messages: Vec<IngressMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stop: Option<StopField>,
}

/// `stop` is a string or an array in the wild; accepting only one shape would
/// reject real clients.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum StopField {
    One(String),
    Many(Vec<String>),
}

impl StopField {
    fn into_vec(self) -> Vec<String> {
        match self {
            StopField::One(s) => vec![s],
            StopField::Many(v) => v,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct IngressMessage {
    pub role: String,
    /// Absent on assistant messages that only carried tool calls.
    #[serde(default)]
    pub content: Option<String>,
}

impl IngressRequest {
    /// Map the standard dialect onto the IR.
    pub fn into_ir(self, account: AccountId) -> ChatRequest {
        let messages = self
            .messages
            .into_iter()
            .map(|m| Message {
                role: match m.role.as_str() {
                    "system" => Role::System,
                    "assistant" => Role::Assistant,
                    "tool" | "function" => Role::Tool,
                    _ => Role::User,
                },
                content: vec![ContentBlock::Text {
                    text: m.content.unwrap_or_default(),
                }],
                call_id: None,
                provider_call_id: None,
            })
            .collect();

        ChatRequest {
            // A caller may pin a model or send `auto` and let the router
            // decide — which is the point of pointing a tool at us.
            model: ModelRef(self.model),
            messages,
            tools: Vec::new(),
            sampling: Sampling {
                temperature: self.temperature,
                top_p: self.top_p,
                max_tokens: self.max_tokens,
                stop: self.stop.map(StopField::into_vec).unwrap_or_default(),
            },
            cache: Default::default(),
            stream: self.stream,
            metadata: CallMeta {
                account,
                request: RequestId::new(),
                session: None,
                turn: None,
                // Unset on purpose: the router's classifier decides, which is
                // how an external client inherits routing without knowing it
                // exists.
                task: None,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Outgoing: the standard dialect, again
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Completion {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: CompletionUsage,
}

#[derive(Debug, Serialize)]
pub struct Choice {
    pub index: u32,
    pub message: OutMessage,
    pub finish_reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct OutMessage {
    pub role: &'static str,
    pub content: String,
}

#[derive(Debug, Serialize, Default)]
pub struct CompletionUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// Map an IR stop reason back to the dialect.
fn finish_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
        StopReason::StopSequence => "stop",
        // A client speaking this dialect has no vocabulary for our internal
        // stops, and inventing one would break parsers. `length` is the
        // closest honest signal that output was cut short.
        StopReason::BudgetExceeded | StopReason::MaxSteps => "length",
        StopReason::Cancelled | StopReason::Error => "stop",
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct IngressState {
    pub gateway: Arc<Gateway>,
    /// Until accounts exist (M17.1) every ingress call is billed to one
    /// account. The shape is already right, so the ledger can adopt it.
    pub account: AccountId,
}

pub fn router(state: IngressState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/metrics", axum::routing::get(metrics_endpoint))
        .with_state(state)
}

/// `GET /metrics` — Prometheus text exposition (docs/21 §Metrics, M21.2).
///
/// Unauthenticated and unconditional: a metrics endpoint that needs a key is a
/// metrics endpoint nobody scrapes. It exposes counts and decisions only — never
/// content, never an id — so the deployment can bind it wherever it likes
/// (docs/22 puts it behind the mesh).
async fn metrics_endpoint() -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            panday_sdk::metrics::CONTENT_TYPE,
        )],
        panday_sdk::metrics::render(),
    )
        .into_response()
}

/// Map an IR error onto the status code a standard client expects.
fn error_response(e: PandayError) -> Response {
    use axum::http::StatusCode;
    let status = match &e {
        PandayError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        PandayError::BudgetExceeded { .. } | PandayError::EntitlementDenied { .. } => {
            StatusCode::PAYMENT_REQUIRED
        }
        PandayError::ModelUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
        PandayError::PermissionDenied(_) => StatusCode::FORBIDDEN,
        PandayError::Protocol(_) => StatusCode::BAD_REQUEST,
        PandayError::Provider { .. } | PandayError::Other(_) => StatusCode::BAD_GATEWAY,
    };

    // The standard error envelope: a client that only understands OpenAI's
    // shape must be able to read our failures too.
    (
        status,
        Json(serde_json::json!({
            "error": {
                "message": e.to_string(),
                "type": match &e {
                    PandayError::RateLimited { .. } => "rate_limit_error",
                    PandayError::Protocol(_) => "invalid_request_error",
                    _ => "api_error",
                },
            }
        })),
    )
        .into_response()
}

async fn chat_completions(
    State(state): State<IngressState>,
    Json(req): Json<IngressRequest>,
) -> Response {
    let streaming = req.stream;
    let model_label = req.model.clone();
    let ir = req.into_ir(state.account);

    let stream = match state.gateway.chat(ir).await {
        Ok(s) => s,
        Err(e) => return error_response(e),
    };

    if streaming {
        stream_sse(stream, model_label).await
    } else {
        buffer_json(stream, model_label).await
    }
}

/// Non-streaming: collect and answer once.
async fn buffer_json(stream: panday_sdk::ItemStream, model: String) -> Response {
    use futures_util::StreamExt;

    let mut text = String::new();
    let mut usage = CompletionUsage::default();
    let mut reason = StopReason::EndTurn;
    let mut stream = stream;

    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamItem::Delta { text: t }) => text.push_str(&t),
            Ok(StreamItem::Usage { usage: u }) => {
                usage = CompletionUsage {
                    prompt_tokens: u.input_tokens,
                    completion_tokens: u.output_tokens,
                    total_tokens: u.input_tokens + u.output_tokens,
                };
            }
            Ok(StreamItem::Done { reason: r }) => reason = r,
            Ok(_) => {}
            // A mid-stream failure on a non-streaming request cannot be
            // partially delivered, so it becomes an error response — the only
            // honest option for a client expecting one JSON body.
            Err(e) => return error_response(e),
        }
    }

    Json(Completion {
        id: format!("chatcmpl-{}", RequestId::new().0.simple()),
        object: "chat.completion",
        created: now_secs(),
        model,
        choices: vec![Choice {
            index: 0,
            message: OutMessage {
                role: "assistant",
                content: text,
            },
            finish_reason: finish_reason(reason),
        }],
        usage,
    })
    .into_response()
}

/// Streaming: re-emit as `chat.completion.chunk` SSE.
async fn stream_sse(stream: panday_sdk::ItemStream, model: String) -> Response {
    use futures_util::StreamExt;

    let id = format!("chatcmpl-{}", RequestId::new().0.simple());
    let created = now_secs();

    let body = stream.flat_map(move |item| {
        let id = id.clone();
        let model = model.clone();
        let frames: Vec<Result<String, std::convert::Infallible>> = match item {
            Ok(StreamItem::Delta { text }) => vec![Ok(sse(&serde_json::json!({
                "id": id, "object": "chat.completion.chunk", "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
            })))],
            Ok(StreamItem::Usage { usage }) => vec![Ok(sse(&serde_json::json!({
                "id": id, "object": "chat.completion.chunk", "created": created,
                "model": model, "choices": [],
                "usage": {
                    "prompt_tokens": usage.input_tokens,
                    "completion_tokens": usage.output_tokens,
                    "total_tokens": usage.input_tokens + usage.output_tokens,
                    // Cache splits are ours, not the dialect's; exposing them
                    // under a nonstandard key would break strict parsers.
                    "prompt_tokens_details": {"cached_tokens": usage.cache_read_tokens}
                }
            })))],
            Ok(StreamItem::Done { reason }) => vec![
                Ok(sse(&serde_json::json!({
                    "id": id, "object": "chat.completion.chunk", "created": created,
                    "model": model,
                    "choices": [{"index": 0, "delta": {},
                                 "finish_reason": finish_reason(reason)}]
                }))),
                // The sentinel every client in this family waits for.
                Ok("data: [DONE]\n\n".to_string()),
            ],
            Ok(_) => vec![],
            // Mid-stream: report it in-band and terminate. docs/11 leaves the
            // decision to the caller, and a standard client's only channel for
            // that is an error frame followed by [DONE].
            Err(e) => vec![
                Ok(sse(&serde_json::json!({
                    "error": {"message": e.to_string(), "type": "api_error"}
                }))),
                Ok("data: [DONE]\n\n".to_string()),
            ],
        };
        futures_util::stream::iter(frames)
    });

    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from_stream(body))
        .expect("a static response builder cannot fail")
}

fn sse(value: &serde_json::Value) -> String {
    format!("data: {value}\n\n")
}
