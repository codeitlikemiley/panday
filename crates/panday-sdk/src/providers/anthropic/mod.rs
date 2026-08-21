//! `anthropic` — the Messages API dialect (docs/11, M11.2).
//!
//! Same sans-IO shape as `openai_compat`: [`super::sse`] turns bytes into
//! records, [`EventTranslator`] turns records into `StreamItem`s, and neither
//! knows what a socket is.

pub mod wire;

use super::models::{self, RemoteModel};
use super::sse;
use super::transport::{self, HttpStreamTransport, ReqwestTransport};
use crate::{ItemStream, ModelClient, PandayError};
use panday_types::id::CallId;
use panday_types::model::{ChatRequest, StopReason, StreamItem};
use std::collections::BTreeMap;
use std::sync::Arc;
use wire::WireEvent;

/// Turns decoded records into `StreamItem`s.
///
/// Stateful because tool calls arrive as `content_block_start` (id + name)
/// followed by `input_json_delta` fragments keyed only by block index, and
/// because usage is reported in two halves: input counts on `message_start`,
/// output counts on `message_delta`. Emitting either half alone would bill
/// wrongly, so they are accumulated and emitted once at the end.
#[derive(Debug, Default)]
pub struct EventTranslator {
    /// content-block index -> the `CallId` we minted for it.
    calls: BTreeMap<u32, CallId>,
    /// Accumulated usage across the two frames that carry it.
    usage: wire::WireUsage,
    saw_usage: bool,
    finished: bool,
}

impl EventTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, record: &str) -> Result<Vec<StreamItem>, PandayError> {
        let record = record.trim();
        if record.is_empty() {
            return Ok(Vec::new());
        }

        let event: WireEvent = serde_json::from_str(record).map_err(|e| {
            PandayError::Protocol(format!("anthropic: malformed stream event: {e}"))
        })?;

        let mut out = Vec::new();
        match event {
            WireEvent::MessageStart { message } => {
                if let Some(u) = message.usage {
                    // Input-side counts (including cache splits) land here.
                    self.usage.input_tokens = u.input_tokens;
                    self.usage.cache_read_input_tokens = u.cache_read_input_tokens;
                    self.usage.cache_creation_input_tokens = u.cache_creation_input_tokens;
                    self.usage.cache_creation = u.cache_creation;
                    self.saw_usage = true;
                }
            }

            WireEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                if let wire::WireContentBlock::ToolUse { id, name } = content_block {
                    let call = *self.calls.entry(index).or_default();
                    out.push(StreamItem::ToolCallStart {
                        id: call,
                        name,
                        // Anthropic's `toolu_…`; the harness must quote it
                        // back on the tool result.
                        provider_id: Some(id),
                    });
                }
            }

            WireEvent::ContentBlockDelta { index, delta } => match delta {
                wire::WireDelta::TextDelta { text } => {
                    if !text.is_empty() {
                        out.push(StreamItem::Delta { text });
                    }
                }
                wire::WireDelta::InputJsonDelta { partial_json } => {
                    if !partial_json.is_empty() {
                        // The block must have been opened by a tool_use start;
                        // if not, minting a id here keeps the fragments
                        // together rather than dropping them.
                        let call = *self.calls.entry(index).or_default();
                        out.push(StreamItem::ToolCallDelta {
                            id: call,
                            args_fragment: partial_json,
                        });
                    }
                }
                wire::WireDelta::Other => {}
            },

            WireEvent::MessageDelta { delta, usage } => {
                if let Some(u) = usage {
                    // Output count arrives here; input counts are already held.
                    self.usage.output_tokens = u.output_tokens;
                    self.saw_usage = true;
                }
                if let Some(raw) = &delta.stop_reason {
                    out.extend(self.flush_usage());
                    out.extend(self.finish_once(wire::stop_reason(raw)));
                }
            }

            WireEvent::MessageStop => {
                out.extend(self.flush_usage());
                out.extend(self.finish_once(StopReason::EndTurn));
            }

            // docs/11: a mid-stream failure is reported to the caller, and the
            // harness decides — the gateway never re-prompts on its own.
            WireEvent::Error { error } => {
                return Err(PandayError::Provider {
                    upstream: "anthropic".into(),
                    message: format!("{}: {}", error.r#type, error.message),
                    // `overloaded_error` and `api_error` are transient; the
                    // rest (invalid_request, authentication) are not.
                    retryable: matches!(
                        error.r#type.as_str(),
                        "overloaded_error" | "api_error" | "rate_limit_error"
                    ),
                });
            }

            WireEvent::Ping | WireEvent::ContentBlockStop { .. } | WireEvent::Unknown => {}
        }
        Ok(out)
    }

    /// End of byte stream: guarantee usage and exactly one `Done`.
    pub fn eof(&mut self) -> Vec<StreamItem> {
        let mut out: Vec<StreamItem> = self.flush_usage().into_iter().collect();
        out.extend(self.finish_once(StopReason::EndTurn));
        out
    }

    fn flush_usage(&mut self) -> Option<StreamItem> {
        if !self.saw_usage {
            return None;
        }
        self.saw_usage = false;
        Some(StreamItem::Usage {
            usage: self.usage.to_ir(),
        })
    }

    fn finish_once(&mut self, reason: StopReason) -> Option<StreamItem> {
        if self.finished {
            return None;
        }
        self.finished = true;
        Some(StreamItem::Done { reason })
    }
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

pub struct AnthropicClient {
    base_url: String,
    auth: AnthropicAuth,
    http: Arc<dyn HttpStreamTransport>,
}

enum AnthropicAuth {
    /// Console API key. `x-api-key`.
    ApiKey(String),
    /// Claude Code / Claude Pro-Max subscription OAuth. Bearer + oauth beta.
    OAuth(String),
}

impl AnthropicClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://api.anthropic.com", api_key)
    }

    /// Claude Code's subscription token, not a console API key.
    pub fn oauth(access_token: impl Into<String>) -> Self {
        Self {
            base_url: "https://api.anthropic.com".into(),
            auth: AnthropicAuth::OAuth(access_token.into()),
            http: Arc::new(ReqwestTransport::default()),
        }
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::with_transport(base_url, api_key, Arc::new(ReqwestTransport::default()))
    }

    pub fn with_transport(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        http: Arc<dyn HttpStreamTransport>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth: AnthropicAuth::ApiKey(api_key.into()),
            http,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }

    /// API keys use `x-api-key`. Subscription OAuth uses a bearer token and the
    /// oauth beta header Claude Code sends — a key-shaped header is rejected.
    fn headers(&self) -> Vec<(String, String)> {
        let mut h = vec![("anthropic-version".into(), wire::API_VERSION.into())];
        match &self.auth {
            AnthropicAuth::ApiKey(key) => h.push(("x-api-key".into(), key.clone())),
            AnthropicAuth::OAuth(token) => {
                h.push(("authorization".into(), format!("Bearer {token}")));
                h.push((
                    "anthropic-beta".into(),
                    "claude-code-20250219,oauth-2025-04-20".into(),
                ));
            }
        }
        h
    }

    /// What this credential can actually call (`GET /v1/models`), paginated.
    pub async fn list_models(&self) -> Result<Vec<RemoteModel>, PandayError> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..16 {
            let mut url = format!("{}/v1/models?limit=1000", self.base_url);
            if let Some(id) = &after {
                url.push_str("&after_id=");
                url.push_str(id);
            }
            let body = self.http.get_json(&url, &self.headers()).await?;
            let (page, next) = models::parse_anthropic_models(&body)?;
            out.extend(page);
            match next {
                Some(id) => after = Some(id),
                None => break,
            }
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl ModelClient for AnthropicClient {
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        let body = serde_json::to_vec(&wire::WireRequest::from_ir(&req))
            .map_err(|e| PandayError::Protocol(format!("anthropic: encode request: {e}")))?;

        let bytes = self
            .http
            .post_sse(&self.endpoint(), &self.headers(), body)
            .await?;
        Ok(Box::pin(into_items(bytes)))
    }
}

fn into_items(
    bytes: transport::ByteStream,
) -> impl futures_core::Stream<Item = Result<StreamItem, PandayError>> + Send {
    use futures_util::StreamExt;

    struct State {
        bytes: transport::ByteStream,
        decoder: sse::SseDecoder,
        translator: EventTranslator,
        pending: std::collections::VecDeque<Result<StreamItem, PandayError>>,
        done: bool,
    }

    let state = State {
        bytes,
        decoder: sse::SseDecoder::new(),
        translator: EventTranslator::new(),
        pending: Default::default(),
        done: false,
    };

    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(item) = st.pending.pop_front() {
                return Some((item, st));
            }
            if st.done {
                return None;
            }
            match st.bytes.next().await {
                Some(Ok(chunk)) => {
                    for record in st.decoder.push(&chunk) {
                        match st.translator.push(&record) {
                            Ok(items) => st.pending.extend(items.into_iter().map(Ok)),
                            Err(e) => {
                                st.pending.push_back(Err(e));
                                st.done = true;
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    st.pending.push_back(Err(e));
                    st.done = true;
                }
                None => {
                    if let Some(rest) = st.decoder.finish() {
                        if let Ok(items) = st.translator.push(&rest) {
                            st.pending.extend(items.into_iter().map(Ok));
                        }
                    }
                    st.pending.extend(st.translator.eof().into_iter().map(Ok));
                    st.done = true;
                }
            }
        }
    })
}
