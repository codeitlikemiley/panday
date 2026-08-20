//! `openai_compat` — the Chat Completions dialect (docs/11).
//!
//! One adapter, many bases: Together, Fireworks, Groq, vLLM, llama-server,
//! mistral.rs. M11.1 targets a local llama-server.
//!
//! ## Shape: sans-IO
//!
//! Everything that can be a pure function is one. Bytes arrive from somewhere
//! (`SseDecoder`), records become `StreamItem`s (`ChunkTranslator`), and
//! neither knows what a socket is. docs/10 takes this from rig's sans-IO
//! design, and it is what lets the whole dialect be tested without a model
//! running — the suite here never opens a connection.
//!
//! ```text
//! bytes ──▶ SseDecoder ──▶ "data:" records ──▶ ChunkTranslator ──▶ StreamItem
//! ```

pub mod wire;

use crate::{ItemStream, ModelClient, PandayError};
use panday_types::id::CallId;
use panday_types::model::{ChatRequest, StopReason, StreamItem};
use std::collections::BTreeMap;
use std::sync::Arc;

use super::sse;
use super::transport::{self, HttpStreamTransport, ReqwestTransport};
use wire::WireChunk;

/// The sentinel that ends an OpenAI-compatible stream.
const DONE: &str = "[DONE]";

/// Turns decoded SSE records into `StreamItem`s.
///
/// Stateful for two reasons the wire forces on us:
///  - tool calls stream as fragments keyed by `index`, and `id`/`name` arrive
///    only on the first fragment, so the mapping must be remembered;
///  - `Done` must be emitted exactly once, and some servers send both a
///    `finish_reason` and a `[DONE]` sentinel.
#[derive(Debug, Default)]
pub struct ChunkTranslator {
    /// provider tool-call index -> the `CallId` we minted for it.
    calls: BTreeMap<u32, CallId>,
    /// provider tool-call index -> the provider's opaque id.
    ///
    /// KNOWN GAP (M11.1): the IR's `CallId` is a UUID (docs/03 §Identifiers)
    /// but this dialect's ids are arbitrary strings like `call_abc123`, and
    /// `StreamItem::ToolCallStart` has nowhere to carry the original. We mint
    /// a UUID and keep the provider's string here so a caller can recover it;
    /// a multi-turn tool loop needs it to echo `tool_call_id` back. Closing
    /// this properly means an IR field — deferred to M11.2/M11.3, when the
    /// Anthropic adapter and the failover chain make the requirement concrete.
    provider_ids: BTreeMap<u32, String>,
    /// Set once `Done` has been emitted.
    finished: bool,
}

impl ChunkTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    /// The provider's opaque id for a tool call we reported, if any.
    pub fn provider_call_id(&self, call: CallId) -> Option<&str> {
        let index = self
            .calls
            .iter()
            .find(|(_, v)| **v == call)
            .map(|(k, _)| *k)?;
        self.provider_ids.get(&index).map(String::as_str)
    }

    /// Feed one decoded `data:` record; get the items it produced.
    pub fn push(&mut self, record: &str) -> Result<Vec<StreamItem>, PandayError> {
        let record = record.trim();
        if record.is_empty() {
            return Ok(Vec::new());
        }
        if record == DONE {
            return Ok(self.finish_once(StopReason::EndTurn).into_iter().collect());
        }

        let chunk: WireChunk = serde_json::from_str(record).map_err(|e| {
            PandayError::Protocol(format!("openai_compat: malformed stream chunk: {e}"))
        })?;

        let mut out = Vec::new();

        for choice in &chunk.choices {
            if let Some(text) = &choice.delta.content {
                // Empty deltas are common padding; forwarding them would make
                // the harness render nothing repeatedly.
                if !text.is_empty() {
                    out.push(StreamItem::Delta { text: text.clone() });
                }
            }

            for frag in &choice.delta.tool_calls {
                // `CallId::default()` is `CallId::new()` — a fresh UUIDv7.
                let call = *self.calls.entry(frag.index).or_default();
                if let Some(id) = &frag.id {
                    self.provider_ids.insert(frag.index, id.clone());
                }
                if let Some(func) = &frag.function {
                    // `name` marks the start of a call; `arguments` extends it.
                    if let Some(name) = &func.name {
                        out.push(StreamItem::ToolCallStart {
                            id: call,
                            name: name.clone(),
                            provider_id: self.provider_ids.get(&frag.index).cloned(),
                        });
                    }
                    if let Some(args) = &func.arguments {
                        if !args.is_empty() {
                            out.push(StreamItem::ToolCallDelta {
                                id: call,
                                args_fragment: args.clone(),
                            });
                        }
                    }
                }
            }
        }

        // Usage rides the final chunk (we always request it, see wire.rs).
        // Emit it before `Done` so a consumer that stops at `Done` still bills.
        if let Some(usage) = chunk.usage {
            out.push(StreamItem::Usage {
                usage: usage.to_ir(),
            });
        }

        if let Some(raw) = chunk.choices.iter().find_map(|c| c.finish_reason.as_ref()) {
            out.extend(self.finish_once(wire::stop_reason(raw)));
        }

        Ok(out)
    }

    /// Called when the byte stream ends. A server that drops the connection
    /// without `[DONE]` or a `finish_reason` still ended the turn; the harness
    /// must not wait forever for a terminator that is not coming.
    pub fn eof(&mut self) -> Option<StreamItem> {
        self.finish_once(StopReason::EndTurn)
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
// The adapter
// ---------------------------------------------------------------------------

/// `openai_compat` — one adapter, many bases.
pub struct OpenAiCompatClient {
    /// Base URL of the server, e.g. `http://127.0.0.1:8080`.
    base_url: String,
    /// `None` for the `local` tier: loopback llama-server takes no auth.
    api_key: Option<String>,
    http: Arc<dyn HttpStreamTransport>,
    /// When true, `provider/model` is sent intact — this client is talking to
    /// a panday gateway, which routes on the prefix. Upstream adapters leave
    /// this false so Together/xAI/llama-server see a bare model name.
    keep_model_ref: bool,
}

impl OpenAiCompatClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self::with_transport(base_url, api_key, Arc::new(ReqwestTransport::default()))
    }

    /// Point at a local llama-server (docs/11: the `local` adapter is
    /// `openai_compat` pinned to loopback with no auth).
    pub fn local(base_url: impl Into<String>) -> Self {
        Self::new(base_url, None)
    }

    /// Inject a transport — the seam the tests use.
    pub fn with_transport(
        base_url: impl Into<String>,
        api_key: Option<String>,
        http: Arc<dyn HttpStreamTransport>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            http,
            keep_model_ref: false,
        }
    }

    /// Client for *our* OpenAI-compat ingress: the model field is a route.
    pub fn for_gateway(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        let mut c = Self::new(base_url, api_key);
        c.keep_model_ref = true;
        c
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }

    /// The `local` tier is this adapter pinned to loopback with no auth
    /// (docs/11), so an absent key means no header at all rather than an
    /// empty one — llama-server rejects a malformed `Authorization`.
    fn headers(&self) -> Vec<(String, String)> {
        match &self.api_key {
            Some(key) => vec![("authorization".into(), format!("Bearer {key}"))],
            None => Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl ModelClient for OpenAiCompatClient {
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        let wire = if self.keep_model_ref {
            wire::WireRequest::from_ir_keep_ref(&req)
        } else {
            wire::WireRequest::from_ir(&req)
        };
        let body = serde_json::to_vec(&wire)
            .map_err(|e| PandayError::Protocol(format!("openai_compat: encode request: {e}")))?;

        let bytes = self
            .http
            .post_sse(&self.endpoint(), &self.headers(), body)
            .await?;

        Ok(Box::pin(into_items(bytes)))
    }
}

/// Bytes → `StreamItem`s. The async glue over the pure core; all the decisions
/// live in `SseDecoder` and `ChunkTranslator`, which is why this is short.
fn into_items(
    bytes: transport::ByteStream,
) -> impl futures_core::Stream<Item = Result<StreamItem, PandayError>> + Send {
    use futures_util::StreamExt;

    // State threaded through the unfold: the byte source, both machines, and
    // items decoded but not yet yielded (one chunk can produce several).
    struct State {
        bytes: transport::ByteStream,
        decoder: sse::SseDecoder,
        translator: ChunkTranslator,
        pending: std::collections::VecDeque<Result<StreamItem, PandayError>>,
        done: bool,
    }

    let state = State {
        bytes,
        decoder: sse::SseDecoder::new(),
        translator: ChunkTranslator::new(),
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
                            // A malformed chunk ends the stream: continuing
                            // would silently drop content the model produced.
                            Err(e) => {
                                st.pending.push_back(Err(e));
                                st.done = true;
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    // Mid-stream transport failure. docs/11: emit the error and
                    // let the harness decide; the gateway does not re-prompt.
                    st.pending.push_back(Err(e));
                    st.done = true;
                }
                None => {
                    // Clean EOF: flush anything unterminated, then guarantee
                    // exactly one Done.
                    if let Some(rest) = st.decoder.finish() {
                        if let Ok(items) = st.translator.push(&rest) {
                            st.pending.extend(items.into_iter().map(Ok));
                        }
                    }
                    st.pending.extend(st.translator.eof().map(Ok));
                    st.done = true;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a whole recorded stream through both stages, exactly as the
    /// async path will. No network, no model — docs/11's "recorded
    /// request/response pairs replayed in CI".
    fn replay(bytes: &[u8]) -> Vec<StreamItem> {
        let mut decoder = sse::SseDecoder::new();
        let mut translator = ChunkTranslator::new();
        let mut items = Vec::new();
        for record in decoder.push(bytes) {
            items.extend(translator.push(&record).expect("chunk must translate"));
        }
        if let Some(rest) = decoder.finish() {
            items.extend(translator.push(&rest).expect("trailing chunk"));
        }
        items.extend(translator.eof());
        items
    }

    /// A llama-server text completion, verbatim in shape.
    const TEXT_STREAM: &[u8] = br#"data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"qwen","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"qwen","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"qwen","choices":[{"index":0,"delta":{"content":", world"},"finish_reason":null}]}

data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"qwen","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":8}}}

data: [DONE]

"#;

    #[test]
    fn streams_a_local_text_completion() {
        let items = replay(TEXT_STREAM);

        let text: String = items
            .iter()
            .filter_map(|i| match i {
                StreamItem::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello, world");

        let usage = items
            .iter()
            .find_map(|i| match i {
                StreamItem::Usage { usage } => Some(*usage),
                _ => None,
            })
            .expect("usage must be captured or the call bills as free");
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.cache_read_tokens, 8);

        assert!(matches!(
            items.last(),
            Some(StreamItem::Done {
                reason: StopReason::EndTurn
            })
        ));
    }

    #[test]
    fn emits_done_exactly_once_despite_finish_reason_and_sentinel() {
        // The stream above carries BOTH a finish_reason and [DONE]; a second
        // Done would make the harness think a turn ended twice.
        let dones = replay(TEXT_STREAM)
            .iter()
            .filter(|i| matches!(i, StreamItem::Done { .. }))
            .count();
        assert_eq!(dones, 1);
    }

    #[test]
    fn usage_is_emitted_before_done() {
        let items = replay(TEXT_STREAM);
        let usage_at = items
            .iter()
            .position(|i| matches!(i, StreamItem::Usage { .. }))
            .unwrap();
        let done_at = items
            .iter()
            .position(|i| matches!(i, StreamItem::Done { .. }))
            .unwrap();
        assert!(
            usage_at < done_at,
            "a consumer stopping at Done must still bill"
        );
    }

    #[test]
    fn assembles_a_fragmented_tool_call() {
        let stream = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"run_tests","arguments":""}}]},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pkg\":"}}]},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"types\"}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#;
        let items = replay(stream);

        let starts: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                StreamItem::ToolCallStart { id, name, .. } => Some((*id, name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 1, "name arrives once; one start per call");
        assert_eq!(starts[0].1, "run_tests");

        let args: String = items
            .iter()
            .filter_map(|i| match i {
                StreamItem::ToolCallDelta { id, args_fragment } => {
                    assert_eq!(*id, starts[0].0, "fragments must share the call id");
                    Some(args_fragment.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(args, r#"{"pkg":"types"}"#);

        assert!(matches!(
            items.last(),
            Some(StreamItem::Done {
                reason: StopReason::ToolUse
            })
        ));
    }

    #[test]
    fn keeps_parallel_tool_calls_apart() {
        // One physical line: SSE only honours `data:`-prefixed lines, so a
        // record must never be wrapped across lines.
        let stream = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read","arguments":"{\"a\":1}"}},{"index":1,"id":"call_b","function":{"name":"write","arguments":"{\"b\":2}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#;
        let items = replay(stream);
        let starts: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                StreamItem::ToolCallStart { id, name, .. } => Some((*id, name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 2);
        assert_ne!(starts[0].0, starts[1].0, "distinct calls need distinct ids");
        assert_eq!(starts[0].1, "read");
        assert_eq!(starts[1].1, "write");
    }

    #[test]
    fn recovers_the_provider_call_id() {
        let mut t = ChunkTranslator::new();
        let items = t
            .push(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"read","arguments":""}}]}}]}"#)
            .unwrap();
        let StreamItem::ToolCallStart { id, .. } = &items[0] else {
            panic!("expected a tool call start");
        };
        // The UUID is ours; the provider's opaque id must remain recoverable.
        assert_eq!(t.provider_call_id(*id), Some("call_abc"));
    }

    #[test]
    fn a_truncated_stream_still_terminates() {
        // Connection dropped after one delta: no finish_reason, no [DONE].
        let items = replay(
            br#"data: {"choices":[{"index":0,"delta":{"content":"partial"}}]}

"#,
        );
        assert!(matches!(items.first(), Some(StreamItem::Delta { .. })));
        assert!(
            matches!(items.last(), Some(StreamItem::Done { .. })),
            "the harness must not wait forever for a terminator that never comes"
        );
    }

    #[test]
    fn malformed_json_is_a_protocol_error_not_a_panic() {
        let mut t = ChunkTranslator::new();
        let err = t.push("{not json").unwrap_err();
        assert!(matches!(err, PandayError::Protocol(_)));
        // And it is not retryable — replaying it would fail identically.
        assert!(!err.is_retryable());
    }

    #[test]
    fn empty_and_padding_records_produce_nothing() {
        let mut t = ChunkTranslator::new();
        assert!(t.push("").unwrap().is_empty());
        assert!(t
            .push(r#"{"choices":[{"index":0,"delta":{"content":""}}]}"#)
            .unwrap()
            .is_empty());
    }

    // -----------------------------------------------------------------------
    // End-to-end through the adapter, with the HTTP layer mocked.
    //
    // The user requirement and docs/11's conformance-fixture rule are the
    // same thing: the suite must pass with no model running.
    // -----------------------------------------------------------------------

    use futures_util::StreamExt;
    use std::sync::Mutex;
    use transport::ByteStream;

    /// Replays recorded bytes and records what was sent, so tests can assert
    /// on the request as well as the response.
    struct MockTransport {
        /// Body chunks to hand back, in order — exercising split boundaries.
        chunks: Vec<Vec<u8>>,
        /// Fail the call outright instead of streaming.
        fail: Option<PandayError>,
        seen: Mutex<Option<SeenRequest>>,
    }

    #[derive(Clone)]
    struct SeenRequest {
        url: String,
        api_key: Option<String>,
        body: serde_json::Value,
    }

    impl MockTransport {
        fn streaming(chunks: Vec<&[u8]>) -> Arc<Self> {
            Arc::new(Self {
                chunks: chunks.into_iter().map(<[u8]>::to_vec).collect(),
                fail: None,
                seen: Mutex::new(None),
            })
        }
        fn failing(err: PandayError) -> Arc<Self> {
            Arc::new(Self {
                chunks: Vec::new(),
                fail: Some(err),
                seen: Mutex::new(None),
            })
        }
        fn seen(&self) -> SeenRequest {
            self.seen
                .lock()
                .unwrap()
                .clone()
                .expect("no request issued")
        }
    }

    #[async_trait::async_trait]
    impl HttpStreamTransport for MockTransport {
        async fn post_sse(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: Vec<u8>,
        ) -> Result<ByteStream, PandayError> {
            *self.seen.lock().unwrap() = Some(SeenRequest {
                url: url.to_string(),
                api_key: headers
                    .iter()
                    .find(|(k, _)| k == "authorization")
                    .map(|(_, v)| v.trim_start_matches("Bearer ").to_string()),
                body: serde_json::from_slice(&body).expect("body must be valid JSON"),
            });
            if let Some(e) = &self.fail {
                // PandayError is not Clone; rebuild the shape under test.
                return Err(match e {
                    PandayError::RateLimited { retry_after_ms } => PandayError::RateLimited {
                        retry_after_ms: *retry_after_ms,
                    },
                    other => PandayError::Protocol(other.to_string()),
                });
            }
            let chunks: Vec<Result<Vec<u8>, PandayError>> =
                self.chunks.iter().cloned().map(Ok).collect();
            Ok(Box::pin(futures_util::stream::iter(chunks)))
        }
    }

    fn a_request() -> ChatRequest {
        use panday_types::id::{AccountId, RequestId};
        use panday_types::model::{CallMeta, ContentBlock, Message, ModelRef, Role, Sampling};
        ChatRequest {
            model: ModelRef("local/qwen3.5-4b".into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "say hello".into(),
                }],
                call_id: None,
                provider_call_id: None,
            }],
            tools: vec![],
            sampling: Sampling {
                max_tokens: Some(64),
                ..Default::default()
            },
            cache: Default::default(),
            stream: true,
            metadata: CallMeta {
                account: AccountId::new(),
                request: RequestId::new(),
                session: None,
                turn: None,
                task: None,
            },
        }
    }

    async fn collect(adapter: &OpenAiCompatClient) -> Vec<Result<StreamItem, PandayError>> {
        adapter
            .chat(a_request())
            .await
            .expect("call")
            .collect()
            .await
    }

    #[tokio::test]
    async fn streams_a_completion_from_a_local_llama_server() {
        let http = MockTransport::streaming(vec![TEXT_STREAM]);
        let adapter =
            OpenAiCompatClient::with_transport("http://127.0.0.1:8080", None, http.clone());

        let items: Vec<StreamItem> = collect(&adapter)
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("no errors in a clean stream");

        let text: String = items
            .iter()
            .filter_map(|i| match i {
                StreamItem::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello, world");
        assert!(matches!(
            items.last(),
            Some(StreamItem::Done {
                reason: StopReason::EndTurn
            })
        ));
    }

    #[tokio::test]
    async fn for_gateway_sends_the_full_model_ref() {
        // Ingress routes on `provider/model`. Stripping would send `grok-4.6`
        // and the pool would honestly refuse it.
        let http = MockTransport::streaming(vec![TEXT_STREAM]);
        let mut adapter =
            OpenAiCompatClient::with_transport("http://127.0.0.1:8088", None, http.clone());
        adapter.keep_model_ref = true;
        let req = {
            let mut r = a_request();
            r.model = panday_types::model::ModelRef("xai/grok-4.6".into());
            r
        };
        let _ = adapter.chat(req).await;
        assert_eq!(http.seen().body["model"], "xai/grok-4.6");
    }

    #[tokio::test]
    async fn posts_to_the_chat_completions_endpoint_without_auth_on_loopback() {
        let http = MockTransport::streaming(vec![TEXT_STREAM]);
        // Trailing slash must not produce a double slash in the path.
        let adapter =
            OpenAiCompatClient::with_transport("http://127.0.0.1:8080/", None, http.clone());
        let _ = collect(&adapter).await;

        let seen = http.seen();
        assert_eq!(seen.url, "http://127.0.0.1:8080/v1/chat/completions");
        assert_eq!(seen.api_key, None, "the local tier takes no auth");
        assert_eq!(seen.body["model"], "qwen3.5-4b");
        assert_eq!(seen.body["stream"], true);
        assert_eq!(seen.body["stream_options"]["include_usage"], true);
        assert_eq!(seen.body["max_tokens"], 64);
    }

    #[tokio::test]
    async fn sends_a_bearer_token_when_one_is_configured() {
        let http = MockTransport::streaming(vec![TEXT_STREAM]);
        let adapter = OpenAiCompatClient::with_transport(
            "https://api.together.xyz",
            Some("sk-test".into()),
            http.clone(),
        );
        let _ = collect(&adapter).await;
        assert_eq!(http.seen().api_key.as_deref(), Some("sk-test"));
    }

    #[tokio::test]
    async fn survives_chunk_boundaries_that_split_records() {
        // The network does not respect record boundaries; deliver the stream
        // in three arbitrary slices.
        let mid = TEXT_STREAM.len() / 2;
        let http = MockTransport::streaming(vec![
            &TEXT_STREAM[..7],
            &TEXT_STREAM[7..mid],
            &TEXT_STREAM[mid..],
        ]);
        let adapter = OpenAiCompatClient::with_transport("http://127.0.0.1:8080", None, http);

        let items: Vec<StreamItem> = collect(&adapter)
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();
        let text: String = items
            .iter()
            .filter_map(|i| match i {
                StreamItem::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello, world");
    }

    #[tokio::test]
    async fn a_failed_call_surfaces_as_an_error_not_an_empty_stream() {
        let http = MockTransport::failing(PandayError::RateLimited {
            retry_after_ms: 250,
        });
        let adapter = OpenAiCompatClient::with_transport("http://127.0.0.1:8080", None, http);

        let err = adapter.chat(a_request()).await.err().expect("must fail");
        assert!(matches!(err, PandayError::RateLimited { .. }));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn a_malformed_chunk_ends_the_stream_with_an_error() {
        let http = MockTransport::streaming(vec![b"data: {not json\n\n"]);
        let adapter = OpenAiCompatClient::with_transport("http://127.0.0.1:8080", None, http);

        let items = collect(&adapter).await;
        assert!(
            items.iter().any(|i| i.is_err()),
            "a malformed chunk must not be silently dropped"
        );
    }
}
