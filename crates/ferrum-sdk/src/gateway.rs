//! The gateway transport (docs/10 Layer 2, M10.2).
//!
//! Everything our own services send to a model goes through `ferrum-gateway`
//! (architecture sentence 3: "Every model call in the company goes through
//! ferrum-gateway ... *because* it is the only door"). This is the client side
//! of that door.
//!
//! The gateway speaks an OpenAI-compatible dialect on `/v1/chat/completions`
//! (docs/03 §Platform REST, docs/11 §ingress), so the transport reuses the
//! same wire code the gateway's own adapters use — docs/10's "write once, use
//! both sides". The only differences from talking to a provider directly are
//! the base URL and that the bearer token is a ferrum API key.

use crate::middleware::{ModelClientExt, RetryPolicy};
use crate::providers::openai_compat::OpenAiCompatClient;
use crate::providers::transport::HttpStreamTransport;
use crate::{FerrumError, ItemStream, ModelClient};
use ferrum_types::model::ChatRequest;
use std::sync::Arc;
use std::time::Duration;

/// A client for a `ferrum-gateway` instance.
pub struct GatewayTransport {
    inner: OpenAiCompatClient,
}

impl GatewayTransport {
    /// `base_url` is the gateway root, e.g. `https://gateway.internal`.
    /// `api_key` is a ferrum key (`frm_live_…`), not a provider key — the
    /// gateway holds provider credentials and never hands them out.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            inner: OpenAiCompatClient::new(base_url, api_key),
        }
    }

    /// Inject a transport — the seam tests use to avoid a network.
    pub fn with_transport(
        base_url: impl Into<String>,
        api_key: Option<String>,
        http: Arc<dyn HttpStreamTransport>,
    ) -> Self {
        Self {
            inner: OpenAiCompatClient::with_transport(base_url, api_key, http),
        }
    }
}

#[async_trait::async_trait]
impl ModelClient for GatewayTransport {
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, FerrumError> {
        self.inner.chat(req).await
    }
}

/// The stack docs/10 shows in its builder sketch, assembled with defaults:
/// a per-attempt deadline with retry outside it.
///
/// Returned boxed because the layered type is an implementation detail;
/// callers want "a `ModelClient` to the gateway", not a three-deep generic.
pub fn connect(base_url: impl Into<String>, api_key: Option<String>) -> Box<dyn ModelClient> {
    Box::new(
        GatewayTransport::new(base_url, api_key)
            .with_timeout(Duration::from_secs(30))
            .with_retry(RetryPolicy::default()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::transport::ByteStream;
    use ferrum_types::id::{AccountId, RequestId};
    use ferrum_types::model::{
        CallMeta, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    };
    use futures_util::StreamExt;
    use std::sync::Mutex;

    struct MockTransport {
        body: Vec<u8>,
        seen: Mutex<Option<(String, Option<String>)>>,
    }

    #[async_trait::async_trait]
    impl HttpStreamTransport for MockTransport {
        async fn post_sse(
            &self,
            url: &str,
            headers: &[(String, String)],
            _body: Vec<u8>,
        ) -> Result<ByteStream, FerrumError> {
            let key = headers
                .iter()
                .find(|(k, _)| k == "authorization")
                .map(|(_, v)| v.trim_start_matches("Bearer ").to_string());
            *self.seen.lock().unwrap() = Some((url.to_string(), key));
            let chunks: Vec<Result<Vec<u8>, FerrumError>> = vec![Ok(self.body.clone())];
            Ok(Box::pin(futures_util::stream::iter(chunks)))
        }
    }

    const STREAM: &[u8] = br#"data: {"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1}}

data: [DONE]

"#;

    fn a_request() -> ChatRequest {
        ChatRequest {
            model: ModelRef("anthropic/claude-sonnet-4-5".into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
                call_id: None,
                provider_call_id: None,
            }],
            tools: vec![],
            sampling: Sampling::default(),
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

    #[tokio::test]
    async fn streams_a_completion_end_to_end_through_the_gateway() {
        let http = Arc::new(MockTransport {
            body: STREAM.to_vec(),
            seen: Mutex::new(None),
        });
        let client = GatewayTransport::with_transport(
            "https://gateway.internal",
            Some("frm_live_abc".into()),
            http.clone(),
        );

        let items: Vec<StreamItem> = client
            .chat(a_request())
            .await
            .expect("call")
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("clean stream");

        let text: String = items
            .iter()
            .filter_map(|i| match i {
                StreamItem::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "ok");

        let usage = items
            .iter()
            .find_map(|i| match i {
                StreamItem::Usage { usage } => Some(*usage),
                _ => None,
            })
            .expect("usage must reach the caller for the ledger");
        assert_eq!(usage.input_tokens, 5);

        assert!(matches!(
            items.last(),
            Some(StreamItem::Done {
                reason: StopReason::EndTurn
            })
        ));

        let (url, key) = http.seen.lock().unwrap().clone().unwrap();
        assert_eq!(url, "https://gateway.internal/v1/chat/completions");
        assert_eq!(key.as_deref(), Some("frm_live_abc"));
    }

    #[tokio::test]
    async fn the_full_stack_composes_and_still_streams() {
        // connect()'s layering must not break the stream it wraps.
        let http = Arc::new(MockTransport {
            body: STREAM.to_vec(),
            seen: Mutex::new(None),
        });
        let client = GatewayTransport::with_transport("https://gw", None, http)
            .with_timeout(Duration::from_secs(30))
            .with_retry(RetryPolicy::default());

        let items = client.chat(a_request()).await.expect("call").count().await;
        assert!(items > 0, "middleware must not swallow the stream");
    }
}
