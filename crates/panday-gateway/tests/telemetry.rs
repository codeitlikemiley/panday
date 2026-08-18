//! M21.1 — the gateway's span carries the id scheme (docs/21).
//!
//! Runs on the multi-thread runtime on purpose: a `span.enter()` guard is
//! thread-local, and a future that moves threads between polls loses it
//! silently. A `current_thread` test would pass against exactly the bug this
//! guards.

use panday_gateway::{AdapterCaps, Gateway, ProviderAdapter};
use panday_router::PolicyRouter;
use panday_sdk::telemetry::content_is_scrubbed;
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    Usage,
};
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }
}

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct Dead;

#[async_trait::async_trait]
impl ProviderAdapter for Dead {
    fn name(&self) -> &'static str {
        "openai_compat"
    }
    fn capabilities(&self, _m: &str) -> AdapterCaps {
        AdapterCaps::default()
    }
    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        // Yield so the future genuinely suspends and can be resumed on another
        // worker thread — which is what breaks a thread-local span guard.
        tokio::task::yield_now().await;
        Err(PandayError::Provider {
            upstream: "x".into(),
            message: "529 overloaded".into(),
            retryable: true,
        })
    }
}

struct Live;

#[async_trait::async_trait]
impl ProviderAdapter for Live {
    fn name(&self) -> &'static str {
        "openai_compat"
    }
    fn capabilities(&self, _m: &str) -> AdapterCaps {
        AdapterCaps::default()
    }
    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        tokio::task::yield_now().await;
        let items: Vec<Result<StreamItem, PandayError>> = vec![
            Ok(StreamItem::Delta {
                text: "SECRET_REPLY".into(),
            }),
            Ok(StreamItem::Usage {
                usage: Usage {
                    input_tokens: 7,
                    output_tokens: 2,
                    ..Default::default()
                },
            }),
            Ok(StreamItem::Done {
                reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

const DEV_POLICY: &str = include_str!("../../panday-router/policy/dev.yaml");

fn request(account: AccountId, request: RequestId) -> ChatRequest {
    ChatRequest {
        model: ModelRef("local/qwen3.5-4b".into()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "SECRET_PROMPT".into(),
            }],
            call_id: None,
            provider_call_id: None,
        }],
        tools: vec![],
        sampling: Sampling::default(),
        cache: Default::default(),
        stream: true,
        metadata: CallMeta {
            account,
            request,
            session: None,
            turn: None,
            task: None,
        },
    }
}

async fn capture_call(adapter: Arc<dyn ProviderAdapter>) -> (String, AccountId, RequestId) {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .with_writer(capture.clone()),
    );

    let gateway = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("local", adapter)
        .build();

    let account = AccountId::new();
    let req_id = RequestId::new();

    // `with_subscriber`, not `with_default`: `with_default` is scoped to a
    // closure AND thread-local, so a future created inside it and awaited
    // outside runs with no subscriber at all — which produced a completely
    // empty capture and looked like "the span is missing". Attaching the
    // subscriber to the FUTURE follows it across worker threads.
    use tracing::instrument::WithSubscriber;
    let _ = gateway
        .chat(request(account, req_id))
        .with_subscriber(tracing::Dispatch::new(subscriber))
        .await;

    (capture.text(), account, req_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_gateway_span_survives_moving_between_worker_threads() {
    let (output, account, req_id) = capture_call(Arc::new(Dead)).await;

    assert!(
        output.contains("gateway.chat"),
        "the gateway span never appeared:\n{output}"
    );
    // The ids that make a trace joinable — raw uuids, per docs/03.
    assert!(
        output.contains(&account.0.to_string()),
        "account_id missing from the span:\n{output}"
    );
    assert!(
        output.contains(&req_id.0.to_string()),
        "request_id missing from the span:\n{output}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failover_warning_is_attached_to_the_gateway_span() {
    // The symptom that exposed the thread-local bug: the warn was emitted with
    // no span at all, so an operator could not tell which request it belonged
    // to.
    let (output, _a, req_id) = capture_call(Arc::new(Dead)).await;

    let warn_line = output
        .lines()
        .find(|l| l.contains("provider leg failed"))
        .unwrap_or_else(|| panic!("no failover warning:\n{output}"));

    assert!(
        warn_line.contains("gateway.chat"),
        "the warning is not attached to the request's span: {warn_line}"
    );
    assert!(
        warn_line.contains(&req_id.0.to_string()),
        "the warning cannot be traced back to a request: {warn_line}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_resolved_provider_is_recorded_once_the_chain_resolves() {
    let (output, _a, _r) = capture_call(Arc::new(Live)).await;
    assert!(
        output.contains("\"provider\":\"local\""),
        "the chosen provider was never recorded:\n{output}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_prompt_or_reply_content_reaches_the_gateway_trace() {
    let (output, _a, _r) = capture_call(Arc::new(Live)).await;

    for secret in ["SECRET_PROMPT", "SECRET_REPLY"] {
        assert!(
            !output.contains(secret),
            "content leaked into the gateway trace: {secret}\n{output}"
        );
    }
    for line in output.lines().filter(|l| l.starts_with('{')) {
        content_is_scrubbed(line).unwrap_or_else(|e| panic!("{e}"));
    }
}
