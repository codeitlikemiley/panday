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
use axum::routing::{get, post};
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

/// Who is calling (M17.3).
///
/// A trait because the ingress must not know about Postgres or key hashing — it knows that a
/// request carries a bearer token and that something can turn one into an account. `panday-platform`
/// is that something; `panday chat`, `panday local` and every test wire the no-auth default.
#[async_trait::async_trait]
pub trait Authenticator: Send + Sync {
    /// Resolve a bearer token. `Err` is a refusal; the caller turns it into 401 without repeating
    /// the reason, because the caller learns nothing from "revoked" that they do not learn from
    /// "no".
    async fn authenticate(&self, bearer: &str) -> Result<Caller, AuthError>;
}

/// The identity a request runs as.
#[derive(Debug, Clone)]
pub struct Caller {
    pub account: AccountId,
    /// Stable per key, for rate limiting and for logs. Not the key.
    pub key_id: String,
    pub scopes: Vec<String>,
}

impl Caller {
    pub fn allows(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("no key")]
    Missing,
    #[error("not a valid key")]
    Invalid,
    #[error("this key does not carry the `{0}` scope")]
    MissingScope(String),
}

/// Accepts everything, as one fixed account.
///
/// The default, and correct for every deployment that has no accounts: `panday local`, a solo
/// gateway on a laptop, the test suites. An ingress that demanded a key before the platform exists
/// would make the offline tier impossible.
pub struct NoAuth {
    pub account: AccountId,
}

#[async_trait::async_trait]
impl Authenticator for NoAuth {
    async fn authenticate(&self, _bearer: &str) -> Result<Caller, AuthError> {
        Ok(Caller {
            account: self.account,
            key_id: "anonymous".into(),
            scopes: vec!["models".into(), "sessions".into()],
        })
    }
}

/// Per-key request rate limiting (docs/17 M17.3).
///
/// **Per process, deliberately.** A shared limiter needs Redis or a database round trip on every
/// request; neither is in docs/02's dependency table and both cost more than the thing they bound.
/// With N gateway instances the effective limit is N×, which is stated here rather than discovered
/// later — and is the right trade until the deployment shape that needs a shared one exists
/// (docs/22 shape 3).
pub struct RateLimiter {
    per_minute: u32,
    /// key_id → (window start, count).
    seen: std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, u32)>>,
}

impl RateLimiter {
    pub fn per_minute(limit: u32) -> Self {
        Self {
            per_minute: limit,
            seen: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// A fixed window rather than a token bucket: a bucket is smoother and needs a timestamp per
    /// key *and* a refill rate, and the thing being defended here is a database, which cares about
    /// requests per minute rather than about burst shape.
    pub fn check(&self, key_id: &str) -> Result<(), PandayError> {
        let mut seen = self.seen.lock().unwrap();
        let now = std::time::Instant::now();
        let entry = seen.entry(key_id.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= std::time::Duration::from_secs(60) {
            *entry = (now, 0);
        }
        entry.1 += 1;
        if entry.1 > self.per_minute {
            // The retry hint is the rest of the window, so a client that honours it stops hammering.
            let elapsed = now.duration_since(entry.0).as_millis() as u64;
            return Err(PandayError::RateLimited {
                retry_after_ms: 60_000u64.saturating_sub(elapsed),
            });
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct IngressState {
    pub gateway: Arc<Gateway>,
    /// The account a request runs as when there is no authenticator (M17.1 gave accounts meaning;
    /// M17.3 gave requests identity).
    pub account: AccountId,
    pub auth: Arc<dyn Authenticator>,
    /// `None` means unlimited, which is right for a laptop and wrong for anything public.
    pub rate_limit: Option<Arc<RateLimiter>>,
}

impl IngressState {
    /// The unauthenticated shape: one account, no keys, no limit.
    pub fn open(gateway: Arc<Gateway>, account: AccountId) -> Self {
        Self {
            gateway,
            account,
            auth: Arc::new(NoAuth { account }),
            rate_limit: None,
        }
    }

    pub fn with_auth(mut self, auth: Arc<dyn Authenticator>) -> Self {
        self.auth = auth;
        self
    }

    pub fn with_rate_limit(mut self, limiter: Arc<RateLimiter>) -> Self {
        self.rate_limit = Some(limiter);
        self
    }
}

pub fn router(state: IngressState) -> Router {
    // The paths are named once, in `openapi::ROUTES`, so the description and the server cannot
    // disagree about what exists (M10.6).
    Router::new()
        .route(crate::openapi::ROUTES[0].1, post(chat_completions))
        .route(crate::openapi::ROUTES[1].1, get(metrics_endpoint))
        .route(crate::openapi::ROUTES[2].1, get(list_models))
        .route(crate::openapi::ROUTES[3].1, post(crate::messages::create))
        .route(
            crate::openapi::ROUTES[4].1,
            get(crate::gemini_api::list_models),
        )
        .route(
            crate::openapi::ROUTES[5].1,
            post(crate::gemini_api::generate),
        )
        .with_state(state)
}

/// Bare names from Claude Code / Antigravity become `provider/model`.
pub(crate) fn qualify_model(raw: &str, default_provider: &str) -> String {
    let raw = raw.trim().trim_start_matches("models/");
    if raw.is_empty() || raw == "auto" || raw.contains('/') {
        raw.to_string()
    } else {
        format!("{default_provider}/{raw}")
    }
}

/// `GET /v1/models` — what the signed-in providers actually list, not the YAML catalog.
async fn list_models(
    State(state): State<IngressState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Err(response) = authenticate(&state, &headers).await {
        return response;
    }
    let models = state.gateway.live_models().await;
    Json(serde_json::json!({
        "object": "list",
        "data": models.iter().map(|m| serde_json::json!({
            "id": m.id,
            "object": "model",
            "owned_by": m.id.split_once('/').map(|(p, _)| p).unwrap_or(""),
        })).collect::<Vec<_>>(),
    }))
    .into_response()
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

/// Pull the bearer token out and resolve it.
///
/// One 401 for every failure — missing, malformed, unknown, revoked. The caller learns nothing from
/// the distinction that they could not learn by trying, and telling them "revoked" confirms the key
/// was once real, which is a fact worth having if you found it in a log.
pub(crate) async fn authenticate(
    state: &IngressState,
    headers: &axum::http::HeaderMap,
) -> Result<Caller, Response> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            headers
                .get("x-goog-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("");

    match state.auth.authenticate(bearer).await {
        Ok(caller) => {
            // The ingress is the model plane; a key without `models` has no business here even if it
            // is otherwise valid.
            if !caller.allows("models") {
                return Err(unauthorized("this key does not carry the `models` scope"));
            }
            Ok(caller)
        }
        Err(_) => Err(unauthorized("invalid API key")),
    }
}

fn unauthorized(message: &str) -> Response {
    use axum::http::StatusCode;
    (
        StatusCode::UNAUTHORIZED,
        // The same envelope as every other error, so a client that understands OpenAI's shape can
        // read this one too.
        Json(serde_json::json!({
            "error": { "message": message, "type": "authentication_error" }
        })),
    )
        .into_response()
}

/// The `Retry-After` a 429 should carry, or no headers at all when no upstream
/// stated a wait.
///
/// Shared by all three ingress dialects. RFC 9110 is not a dialect feature, and
/// a caller should not have to know which envelope it asked for to learn when to
/// come back. Omission is deliberate: `retry_after_ms` of 0 means *unknown*
/// (docs/25 M25.3), and `Retry-After: 0` would say "retry now" — the one
/// instruction this header exists to prevent.
pub(crate) fn retry_after_headers(e: &PandayError) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    if let Some(secs) = e.retry_after_secs() {
        if let Ok(value) = axum::http::HeaderValue::from_str(&secs.to_string()) {
            headers.insert(axum::http::header::RETRY_AFTER, value);
        }
    }
    headers
}

/// The HTTP status for an IR error — for **every** ingress (docs/11 M11.8).
///
/// Status is protocol; only the envelope is dialect. The three ingresses used to
/// decide this separately and had drifted: the same exhausted chain was a 503 on
/// `/v1/chat/completions` and a 404 on `/v1/messages`, and a `PermissionDenied`
/// that was a 403 here fell through to 502 on the other two. Nothing asserted
/// they agreed, so nothing caught it — `every_ingress_agrees_on_status` does now.
///
/// `ModelUnavailable` is 503 and means the chain was tried and failed;
/// `ModelNotFound` is 404 and means nothing could be tried at all. They were one
/// variant until M11.9, which is why whichever status it carried was wrong half
/// the time.
pub(crate) fn status_for(e: &PandayError) -> axum::http::StatusCode {
    use axum::http::StatusCode;
    match e {
        PandayError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        PandayError::BudgetExceeded { .. } | PandayError::EntitlementDenied { .. } => {
            StatusCode::PAYMENT_REQUIRED
        }
        PandayError::ModelUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
        // Nothing was dialled and nothing can be until the configuration
        // changes. 503 would tell the caller to come back later about a request
        // that can only ever fail (docs/11 M11.9).
        PandayError::ModelNotFound { .. } => StatusCode::NOT_FOUND,
        PandayError::PermissionDenied(_) => StatusCode::FORBIDDEN,
        PandayError::Protocol(_) => StatusCode::BAD_REQUEST,
        PandayError::Provider { .. } | PandayError::Other(_) => StatusCode::BAD_GATEWAY,
    }
}

/// Map an IR error onto the status code a standard client expects.
pub(crate) fn error_response(e: PandayError) -> Response {
    let status = status_for(&e);

    // The standard error envelope: a client that only understands OpenAI's
    // shape must be able to read our failures too.
    (
        status,
        retry_after_headers(&e),
        Json(serde_json::json!({
            "error": {
                "message": e.to_string(),
                "type": match &e {
                    PandayError::RateLimited { .. } => "rate_limit_error",
                    PandayError::Protocol(_) => "invalid_request_error",
                    // OpenAI's own code for this, so a client that branches on
                    // the real API branches the same way here.
                    PandayError::ModelNotFound { .. } => "model_not_found",
                    _ => "api_error",
                },
            }
        })),
    )
        .into_response()
}

async fn chat_completions(
    State(state): State<IngressState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<IngressRequest>,
) -> Response {
    // Authenticate first: a request that will be refused should not reach the router, the cache or
    // the provider, and an unauthenticated caller must not be able to make us do work.
    let caller = match authenticate(&state, &headers).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    if let Some(limiter) = &state.rate_limit {
        if let Err(e) = limiter.check(&caller.key_id) {
            return error_response(e);
        }
    }

    let streaming = req.stream;
    let model_label = req.model.clone();
    // Billed to the key's account, not to the process's: this is the line that makes the ledger's
    // per-account totals mean anything on a shared gateway.
    let ir = req.into_ir(caller.account);

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

#[cfg(test)]
mod tests {
    use super::{error_response, qualify_model, retry_after_headers, status_for};
    use panday_sdk::PandayError;

    fn retry_after_of(e: PandayError) -> Option<String> {
        error_response(e)
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[test]
    fn a_stated_wait_reaches_the_client_as_delay_seconds() {
        assert_eq!(
            retry_after_of(PandayError::RateLimited {
                retry_after_ms: 8_000
            }),
            Some("8".into())
        );
    }

    #[test]
    fn an_unknown_wait_sends_no_header_at_all() {
        // 0 is "no upstream stated one" (docs/25 M25.3). `Retry-After: 0` would
        // say "retry now", which is the opposite instruction.
        assert_eq!(
            retry_after_of(PandayError::RateLimited { retry_after_ms: 0 }),
            None
        );
    }

    #[test]
    fn a_partial_second_rounds_up_rather_than_down() {
        // Truncating 1500ms to 1s sends the client back inside the window, and
        // several providers extend the penalty when you retry too early.
        assert_eq!(
            retry_after_of(PandayError::RateLimited {
                retry_after_ms: 1_500
            }),
            Some("2".into())
        );
        // A sub-second wait must not floor to 0 and become "retry now".
        assert_eq!(
            retry_after_of(PandayError::RateLimited {
                retry_after_ms: 400
            }),
            Some("1".into())
        );
    }

    /// One of every `PandayError` variant. A new variant will not compile here
    /// until it is added, which is the point.
    fn one_of_each() -> Vec<(&'static str, PandayError)> {
        vec![
            (
                "RateLimited",
                PandayError::RateLimited {
                    retry_after_ms: 1_000,
                },
            ),
            (
                "BudgetExceeded",
                PandayError::BudgetExceeded { balance_micros: -5 },
            ),
            (
                "EntitlementDenied",
                PandayError::EntitlementDenied {
                    plan: "free".into(),
                    needed: "frontier".into(),
                },
            ),
            (
                "ModelUnavailable",
                PandayError::ModelUnavailable {
                    tried: vec!["anthropic/claude-sonnet-5".into()],
                },
            ),
            (
                "PermissionDenied",
                PandayError::PermissionDenied("no".into()),
            ),
            (
                "ModelNotFound",
                PandayError::ModelNotFound {
                    considered: vec!["gemini/nope (no adapter configured)".into()],
                },
            ),
            ("Protocol", PandayError::Protocol("bad json".into())),
            (
                "Provider",
                PandayError::Provider {
                    upstream: "http".into(),
                    message: "500".into(),
                    retryable: true,
                },
            ),
            ("Other", PandayError::Other("boom".into())),
        ]
    }

    #[test]
    fn every_ingress_agrees_on_status() {
        // The three dialects render different envelopes on purpose. The status
        // is not part of the envelope — it is the protocol — and they had
        // drifted apart precisely because nothing asserted this (docs/11 M11.8):
        // an exhausted chain was 503 here and 404 on /v1/messages, and a
        // PermissionDenied that is 403 here fell through to 502 on both others.
        for (name, e) in one_of_each() {
            let openai = error_response(clone_of(&e)).status();
            let anthropic = crate::messages::anthropic_error(clone_of(&e)).status();
            let gemini = crate::gemini_api::gemini_error(clone_of(&e)).status();
            assert_eq!(
                openai, anthropic,
                "{name}: /v1/chat/completions says {openai}, /v1/messages says {anthropic}"
            );
            assert_eq!(
                openai, gemini,
                "{name}: /v1/chat/completions says {openai}, /v1beta says {gemini}"
            );
        }
    }

    #[test]
    fn the_shared_table_is_what_every_ingress_actually_uses() {
        // Guards against a dialect quietly reintroducing its own match arm:
        // agreement with each other is not enough if all three drift together.
        for (name, e) in one_of_each() {
            let expected = status_for(&e);
            assert_eq!(error_response(clone_of(&e)).status(), expected, "{name}");
            assert_eq!(
                crate::messages::anthropic_error(clone_of(&e)).status(),
                expected,
                "{name}"
            );
            assert_eq!(
                crate::gemini_api::gemini_error(clone_of(&e)).status(),
                expected,
                "{name}"
            );
        }
    }

    /// `PandayError` is not `Clone` (`Other` boxes a `dyn Error`), and the three
    /// mappers take it by value.
    fn clone_of(e: &PandayError) -> PandayError {
        match e {
            PandayError::RateLimited { retry_after_ms } => PandayError::RateLimited {
                retry_after_ms: *retry_after_ms,
            },
            PandayError::BudgetExceeded { balance_micros } => PandayError::BudgetExceeded {
                balance_micros: *balance_micros,
            },
            PandayError::EntitlementDenied { plan, needed } => PandayError::EntitlementDenied {
                plan: plan.clone(),
                needed: needed.clone(),
            },
            PandayError::ModelUnavailable { tried } => PandayError::ModelUnavailable {
                tried: tried.clone(),
            },
            PandayError::ModelNotFound { considered } => PandayError::ModelNotFound {
                considered: considered.clone(),
            },
            PandayError::PermissionDenied(m) => PandayError::PermissionDenied(m.clone()),
            PandayError::Protocol(m) => PandayError::Protocol(m.clone()),
            PandayError::Provider {
                upstream,
                message,
                retryable,
            } => PandayError::Provider {
                upstream: upstream.clone(),
                message: message.clone(),
                retryable: *retryable,
            },
            PandayError::Other(e) => PandayError::Other(e.to_string().into()),
        }
    }

    #[test]
    fn only_a_rate_limit_carries_the_header() {
        assert_eq!(retry_after_of(PandayError::Protocol("nope".into())), None);
        assert!(retry_after_headers(&PandayError::ModelUnavailable { tried: vec![] }).is_empty());
    }

    #[test]
    fn a_long_upstream_window_is_published_unclamped() {
        // MAX_HONOURED_RETRY_AFTER bounds how long *we* sleep, not what the
        // upstream said. Republishing a shortened number would schedule a
        // retry storm at the moment our own cap expired.
        assert_eq!(
            retry_after_of(PandayError::RateLimited {
                retry_after_ms: 3_600_000
            }),
            Some("3600".into())
        );
    }

    #[test]
    fn bare_claude_ids_become_anthropic() {
        assert_eq!(
            qualify_model("claude-sonnet-5", "anthropic"),
            "anthropic/claude-sonnet-5"
        );
    }

    #[test]
    fn already_qualified_ids_are_left_alone() {
        assert_eq!(qualify_model("xai/grok-4.6", "anthropic"), "xai/grok-4.6");
        assert_eq!(qualify_model("auto", "anthropic"), "auto");
    }

    #[test]
    fn gemini_models_prefix_is_stripped() {
        assert_eq!(
            qualify_model("models/gemini-2.5-flash", "gemini"),
            "gemini/gemini-2.5-flash"
        );
    }
}
