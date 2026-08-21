//! Credential inner-loop for one provider name (docs/25 M25.4, this experiment).
//!
//! The registry still sees one `"xai"` adapter. Members are tried in order on
//! the same request. `RateLimited` / retryable provider errors walk to the
//! next key; a non-retryable 400 does not. Every member 429s → `RateLimited`
//! so the gateway's model chain can still fail over to Claude.

use super::openai_compat::OpenAiCompat;
use crate::{AdapterCaps, ProviderAdapter};
use panday_sdk::providers::RemoteModel;
use panday_sdk::{ItemStream, PandayError};
use panday_types::model::ChatRequest;
use std::sync::Arc;

/// Several adapters behind one registry name.
pub struct PooledAdapter {
    members: Vec<Arc<dyn ProviderAdapter>>,
}

impl PooledAdapter {
    pub fn new(members: Vec<Arc<dyn ProviderAdapter>>) -> Self {
        Self { members }
    }
}

/// 0 tokens → no adapter; 1 → today's single client; 2+ → this pool.
pub fn from_xai_tokens(tokens: Vec<String>) -> Option<Arc<dyn ProviderAdapter>> {
    let tokens: Vec<String> = tokens
        .into_iter()
        .filter(|t| !t.trim().is_empty())
        .collect();
    let base = panday_sdk::oauth::xai_api_base();
    let mut iter = tokens.into_iter();
    match (iter.next(), iter.next()) {
        (None, _) => None,
        (Some(only), None) => Some(Arc::new(OpenAiCompat::new(base, Some(only))) as _),
        (Some(first), Some(second)) => {
            let mut members: Vec<Arc<dyn ProviderAdapter>> = vec![
                Arc::new(OpenAiCompat::new(base, Some(first))) as _,
                Arc::new(OpenAiCompat::new(base, Some(second))) as _,
            ];
            members.extend(
                iter.map(|t| {
                    Arc::new(OpenAiCompat::new(base, Some(t))) as Arc<dyn ProviderAdapter>
                }),
            );
            Some(Arc::new(PooledAdapter::new(members)) as _)
        }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for PooledAdapter {
    fn name(&self) -> &'static str {
        // Inner dialect the registry already expects from a lone OpenAiCompat.
        self.members
            .first()
            .map(|m| m.name())
            .unwrap_or("openai_compat")
    }

    fn capabilities(&self, model: &str) -> AdapterCaps {
        match self.members.first() {
            Some(m) => m.capabilities(model),
            None => AdapterCaps::default(),
        }
    }

    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        let mut last_retryable: Option<PandayError> = None;
        let mut all_rate_limited = true;
        for member in &self.members {
            match member.chat(req.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) if e.is_retryable() => {
                    all_rate_limited &= matches!(e, PandayError::RateLimited { .. });
                    last_retryable = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        if all_rate_limited && last_retryable.is_some() {
            return Err(PandayError::RateLimited { retry_after_ms: 0 });
        }
        Err(last_retryable
            .unwrap_or_else(|| PandayError::Protocol("pooled adapter has no members".into())))
    }

    async fn list_models(&self) -> Result<Vec<RemoteModel>, PandayError> {
        let mut last_retryable: Option<PandayError> = None;
        for member in &self.members {
            match member.list_models().await {
                Ok(models) => return Ok(models),
                Err(e) if e.is_retryable() => last_retryable = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last_retryable
            .unwrap_or_else(|| PandayError::Protocol("pooled adapter has no members".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CacheStyle;
    use futures_util::StreamExt;
    use panday_sdk::providers::transport::{ByteStream, HttpStreamTransport};
    use panday_types::id::{AccountId, RequestId};
    use panday_types::model::{
        CallMeta, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct Spy {
        name: &'static str,
        calls: AtomicUsize,
        script: Script,
    }

    enum Script {
        RateLimited,
        BadRequest,
        Retryable,
        Ok(&'static str),
    }

    impl Spy {
        fn new(name: &'static str, script: Script) -> Arc<Self> {
            Arc::new(Self {
                name,
                calls: AtomicUsize::new(0),
                script,
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for Spy {
        fn name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self, _model: &str) -> AdapterCaps {
            AdapterCaps {
                cache_style: CacheStyle::AutomaticPrefix,
                tools: true,
                ..Default::default()
            }
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.script {
                Script::RateLimited => Err(PandayError::RateLimited { retry_after_ms: 0 }),
                Script::BadRequest => Err(PandayError::Provider {
                    upstream: self.name.into(),
                    message: "HTTP 400: bad request".into(),
                    retryable: false,
                }),
                Script::Retryable => Err(PandayError::Provider {
                    upstream: self.name.into(),
                    message: "HTTP 503: overloaded".into(),
                    retryable: true,
                }),
                Script::Ok(text) => {
                    let items: Vec<Result<StreamItem, PandayError>> = vec![
                        Ok(StreamItem::Delta { text: text.into() }),
                        Ok(StreamItem::Done {
                            reason: StopReason::EndTurn,
                        }),
                    ];
                    Ok(Box::pin(futures_util::stream::iter(items)))
                }
            }
        }
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: ModelRef("xai/grok-4.6".into()),
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

    async fn drain_text(adapter: &dyn ProviderAdapter) -> Result<String, PandayError> {
        let mut stream = adapter.chat(req()).await?;
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let StreamItem::Delta { text: t } = item? {
                text.push_str(&t);
            }
        }
        Ok(text)
    }

    fn pool(members: Vec<Arc<Spy>>) -> PooledAdapter {
        PooledAdapter::new(
            members
                .into_iter()
                .map(|m| m as Arc<dyn ProviderAdapter>)
                .collect(),
        )
    }

    #[tokio::test]
    async fn rate_limited_member_walks_to_the_next_key() {
        let a = Spy::new("openai_compat", Script::RateLimited);
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        assert_eq!(adapter.name(), "openai_compat");
        let text = drain_text(&adapter).await.expect("B serves the call");
        assert_eq!(text, "from-b");
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
    }

    #[tokio::test]
    async fn every_member_429_returns_rate_limited() {
        let a = Spy::new("openai_compat", Script::RateLimited);
        let b = Spy::new("openai_compat", Script::RateLimited);
        let adapter = pool(vec![a.clone(), b.clone()]);
        let err = drain_text(&adapter).await.expect_err("pool exhausted");
        assert!(matches!(err, PandayError::RateLimited { .. }));
        assert!(err.is_retryable());
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
    }

    #[tokio::test]
    async fn a_400_does_not_walk_keys() {
        let a = Spy::new("openai_compat", Script::BadRequest);
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let err = drain_text(&adapter).await.expect_err("caller error");
        assert!(!err.is_retryable());
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 0, "a 400 must not spend the next credential");
    }

    #[tokio::test]
    async fn retryable_provider_error_walks_like_429() {
        let a = Spy::new("openai_compat", Script::Retryable);
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let text = drain_text(&adapter).await.expect("B serves");
        assert_eq!(text, "from-b");
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
    }

    #[test]
    fn from_xai_tokens_matches_boot_shape() {
        assert!(from_xai_tokens(vec![]).is_none());
        assert!(from_xai_tokens(vec![String::new()]).is_none());
        let one = from_xai_tokens(vec!["sk-test-aaaa".into()]).expect("one token");
        assert_eq!(one.name(), "openai_compat");
        let two = from_xai_tokens(vec!["sk-test-aaaa".into(), "sk-test-bbbb".into()])
            .expect("two tokens");
        assert_eq!(two.name(), "openai_compat");
    }

    /// Minimal scripted HTTP: fail with an error, or replay one SSE body.
    struct MockTransport {
        fail: Mutex<Option<fn() -> PandayError>>,
        body: Vec<u8>,
        calls: AtomicUsize,
    }

    impl MockTransport {
        fn failing(err: fn() -> PandayError) -> Arc<Self> {
            Arc::new(Self {
                fail: Mutex::new(Some(err)),
                body: Vec::new(),
                calls: AtomicUsize::new(0),
            })
        }
        fn streaming(body: &[u8]) -> Arc<Self> {
            Arc::new(Self {
                fail: Mutex::new(None),
                body: body.to_vec(),
                calls: AtomicUsize::new(0),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl HttpStreamTransport for MockTransport {
        async fn post_sse(
            &self,
            _url: &str,
            _headers: &[(String, String)],
            _body: Vec<u8>,
        ) -> Result<ByteStream, PandayError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(err) = *self.fail.lock().unwrap() {
                return Err(err());
            }
            Ok(Box::pin(futures_util::stream::iter(vec![Ok(self
                .body
                .clone())])))
        }
    }

    const FROM_B: &[u8] =
        br#"data: {"choices":[{"index":0,"delta":{"content":"from-b"},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#;

    #[tokio::test]
    async fn mock_transport_429_then_sse_from_the_second_key() {
        let a_http = MockTransport::failing(|| PandayError::RateLimited { retry_after_ms: 0 });
        let b_http = MockTransport::streaming(FROM_B);
        let a = OpenAiCompat::with_transport(
            "https://api.x.ai",
            Some("sk-test-aaaa".into()),
            a_http.clone(),
        );
        let b = OpenAiCompat::with_transport(
            "https://api.x.ai",
            Some("sk-test-bbbb".into()),
            b_http.clone(),
        );
        let adapter = PooledAdapter::new(vec![Arc::new(a) as _, Arc::new(b) as _]);
        let text = drain_text(&adapter).await.expect("B's SSE");
        assert_eq!(text, "from-b");
        assert_eq!(a_http.calls(), 1);
        assert_eq!(b_http.calls(), 1);
    }
}
