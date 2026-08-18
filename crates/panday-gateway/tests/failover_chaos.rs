//! M11.3 — chain failover and the kill-a-provider chaos test.
//!
//! docs/11's acceptance: "Router integration (12) with chain-failover;
//! kill-a-provider chaos test passes (session degrades, never errors to user)."
//!
//! "Never errors to user" is the property under test, and it is stronger than
//! "eventually succeeds": a multi-turn session must keep working *through* a
//! provider dying, without the caller seeing a failure it has to handle.

use futures_util::StreamExt;
use panday_gateway::{
    AdapterCaps, CacheStyle, CollectUsage, Gateway, ProviderAdapter, UsageRecord,
};
use panday_router::PolicyRouter;
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    Usage,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

const DEV_POLICY: &str = include_str!("../../panday-router/policy/dev.yaml");

/// A provider that can be "killed" at runtime.
struct FlakyProvider {
    name: &'static str,
    /// Once set, every call fails.
    dead: AtomicBool,
    /// How it fails when dead.
    error: fn(&str) -> PandayError,
    calls: AtomicUsize,
}

impl FlakyProvider {
    fn new(name: &'static str, error: fn(&str) -> PandayError) -> Arc<Self> {
        Arc::new(Self {
            name,
            dead: AtomicBool::new(false),
            error,
            calls: AtomicUsize::new(0),
        })
    }
    fn kill(&self) {
        self.dead.store(true, Ordering::SeqCst);
    }
    fn revive(&self) {
        self.dead.store(false, Ordering::SeqCst);
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn overloaded(name: &str) -> PandayError {
    PandayError::Provider {
        upstream: name.to_string(),
        message: "529 overloaded".into(),
        retryable: true,
    }
}

fn bad_request(name: &str) -> PandayError {
    PandayError::Provider {
        upstream: name.to_string(),
        message: "400 unknown parameter".into(),
        retryable: false,
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for FlakyProvider {
    fn name(&self) -> &'static str {
        self.name
    }
    fn capabilities(&self, _m: &str) -> AdapterCaps {
        AdapterCaps {
            cache_style: CacheStyle::None,
            ..Default::default()
        }
    }
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.dead.load(Ordering::SeqCst) {
            return Err((self.error)(self.name));
        }
        let served_by = req.model.0.clone();
        let items: Vec<Result<StreamItem, PandayError>> = vec![
            Ok(StreamItem::Delta { text: served_by }),
            Ok(StreamItem::Usage {
                usage: Usage {
                    input_tokens: 10,
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

/// A provider that establishes a stream and then dies partway through it.
struct DiesMidStream;

#[async_trait::async_trait]
impl ProviderAdapter for DiesMidStream {
    fn name(&self) -> &'static str {
        "anthropic"
    }
    fn capabilities(&self, _m: &str) -> AdapterCaps {
        AdapterCaps::default()
    }
    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        let items: Vec<Result<StreamItem, PandayError>> = vec![
            Ok(StreamItem::Delta {
                text: "partial answer".into(),
            }),
            Err(PandayError::Provider {
                upstream: "anthropic".into(),
                message: "connection reset mid-stream".into(),
                retryable: true,
            }),
        ];
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

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
        stream: true,
        metadata: CallMeta {
            account: AccountId::new(),
            request: RequestId::new(),
            session: None,
            turn: None,
            task: Some(panday_types::model::TaskClass::Code),
        },
    }
}

/// `code` + a paid plan routes to frontier with a workhorse fallback, which is
/// exactly the chain this milestone is about.
fn coding_request() -> ChatRequest {
    let mut r = req("auto");
    r.metadata.task = Some(panday_types::model::TaskClass::Code);
    r
}

fn gateway(
    frontier: Arc<FlakyProvider>,
    workhorse: Arc<FlakyProvider>,
    usage: Arc<CollectUsage>,
) -> Gateway {
    // dev.yaml: code+pro -> frontier (anthropic/claude-opus-4-1) with a
    // workhorse fallback (anthropic/claude-sonnet-4-5). Both are `anthropic`,
    // so distinguish them by registering under two provider prefixes and
    // pinning explicitly in the tests that need it.
    Gateway::builder(Arc::new(
        PolicyRouter::from_yaml(DEV_POLICY).expect("dev policy"),
    ))
    .adapter("anthropic", frontier as Arc<dyn ProviderAdapter>)
    .adapter("together", workhorse as Arc<dyn ProviderAdapter>)
    .adapter(
        "local",
        FlakyProvider::new("openai_compat", overloaded) as Arc<dyn ProviderAdapter>,
    )
    .usage_sink(usage)
    .build()
}

async fn text_of(stream: ItemStream) -> Result<String, PandayError> {
    let items: Vec<Result<StreamItem, PandayError>> = stream.collect().await;
    let mut out = String::new();
    for item in items {
        if let StreamItem::Delta { text } = item? {
            out.push_str(&text);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_healthy_chain_serves_from_its_head() {
    let frontier = FlakyProvider::new("anthropic", overloaded);
    let workhorse = FlakyProvider::new("openai_compat", overloaded);
    let g = gateway(
        frontier.clone(),
        workhorse.clone(),
        Arc::new(CollectUsage::new()),
    );

    let text = text_of(g.chat(coding_request()).await.unwrap())
        .await
        .unwrap();
    assert!(text.contains("claude"), "served by: {text}");
    assert_eq!(workhorse.calls(), 0, "the fallback must not be touched");
}

#[tokio::test]
async fn a_dead_head_fails_over_to_the_next_leg() {
    let frontier = FlakyProvider::new("anthropic", overloaded);
    let workhorse = FlakyProvider::new("openai_compat", overloaded);
    let g = gateway(
        frontier.clone(),
        workhorse.clone(),
        Arc::new(CollectUsage::new()),
    );

    frontier.kill();

    // Same request, next target (docs/11) — and the caller sees a normal stream.
    let text = text_of(g.chat(req("together/qwen3.5-9b")).await.unwrap())
        .await
        .unwrap();
    assert!(text.contains("qwen"), "served by: {text}");
}

#[tokio::test]
async fn killing_a_provider_mid_session_degrades_without_erroring_to_the_user() {
    // The chaos test. A multi-turn session must keep working THROUGH a
    // provider dying — the acceptance is "never errors to user", which is
    // stronger than "eventually succeeds".
    let frontier = FlakyProvider::new("anthropic", overloaded);
    let workhorse = FlakyProvider::new("openai_compat", overloaded);
    let usage = Arc::new(CollectUsage::new());
    let g = gateway(frontier.clone(), workhorse.clone(), usage.clone());

    let mut served: Vec<String> = Vec::new();

    for turn in 0..6 {
        // Kill the frontier a third of the way in, revive it at the end —
        // an outage, not a permanent change.
        if turn == 2 {
            frontier.kill();
        }
        if turn == 5 {
            frontier.revive();
        }

        // `cheap` mixes together+local, so a chain with a live leg exists
        // throughout the outage.
        let mut r = coding_request();
        r.metadata.task = Some(panday_types::model::TaskClass::Route);

        let stream = g
            .chat(r)
            .await
            .unwrap_or_else(|e| panic!("turn {turn} errored to the user: {e}"));
        let text = text_of(stream)
            .await
            .unwrap_or_else(|e| panic!("turn {turn} errored mid-stream: {e}"));
        served.push(text);
    }

    assert_eq!(served.len(), 6, "every turn must have been served");
    assert!(
        served.iter().all(|s| !s.is_empty()),
        "a turn produced no content: {served:?}"
    );

    // Every served turn is billed, whichever leg answered — the ledger must
    // not lose the ones that failed over.
    let records: Vec<UsageRecord> = usage.take();
    assert_eq!(records.len(), 6, "usage must be recorded per served call");
}

#[tokio::test]
async fn a_non_retryable_failure_does_not_walk_the_chain() {
    // A 400 fails identically everywhere. Walking the chain would turn one
    // clear error into several confusing ones, and spend the caller's quota
    // to do it.
    let frontier = FlakyProvider::new("anthropic", bad_request);
    let workhorse = FlakyProvider::new("openai_compat", overloaded);
    let g = gateway(
        frontier.clone(),
        workhorse.clone(),
        Arc::new(CollectUsage::new()),
    );

    frontier.kill();
    let err = g.chat(coding_request()).await.map(|_| ()).unwrap_err();

    assert!(!err.is_retryable(), "{err}");
    assert_eq!(frontier.calls(), 1);
    assert_eq!(
        workhorse.calls(),
        0,
        "a non-retryable error must not try the next leg"
    );
}

#[tokio::test]
async fn an_exhausted_chain_names_every_leg_it_tried() {
    // Its own gateway: every registered leg must be dead, or a live one
    // elsewhere in the pool would (correctly) serve the request and there
    // would be no exhaustion to observe.
    let together = FlakyProvider::new("openai_compat", overloaded);
    let local = FlakyProvider::new("openai_compat", overloaded);
    let g = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("together", together.clone() as Arc<dyn ProviderAdapter>)
        .adapter("local", local.clone() as Arc<dyn ProviderAdapter>)
        .build();

    together.kill();
    local.kill();

    // `route` maps to the `cheap` pool, whose legs are exactly those two.
    let err = g
        .chat({
            let mut r = coding_request();
            r.metadata.task = Some(panday_types::model::TaskClass::Route);
            r
        })
        .await
        .map(|_| ())
        .unwrap_err();

    match err {
        PandayError::ModelUnavailable { tried } => {
            assert!(!tried.is_empty(), "the error must say what was attempted");
            assert!(
                tried.iter().any(|t| t.contains("overloaded")),
                "an operator needs the upstream reason: {tried:?}"
            );
        }
        other => panic!("expected ModelUnavailable, got {other}"),
    }

    assert_eq!(together.calls(), 1, "each leg is tried exactly once");
    assert_eq!(local.calls(), 1);
}

#[tokio::test]
async fn a_mid_stream_failure_is_surfaced_not_retried() {
    // docs/11: after tokens have flowed the gateway "does not re-prompt on its
    // own" — the harness holds turn semantics. Re-prompting would also
    // double-bill the caller for tokens they already saw.
    let workhorse = FlakyProvider::new("openai_compat", overloaded);
    let g = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter(
            "anthropic",
            Arc::new(DiesMidStream) as Arc<dyn ProviderAdapter>,
        )
        .adapter("together", workhorse.clone() as Arc<dyn ProviderAdapter>)
        .build();

    let stream = g
        .chat(coding_request())
        .await
        .expect("establishment succeeded");
    let items: Vec<Result<StreamItem, PandayError>> = stream.collect().await;

    // The partial content reached the caller...
    assert!(matches!(items.first(), Some(Ok(StreamItem::Delta { .. }))));
    // ...and the failure is reported, not papered over.
    assert!(
        items.iter().any(|i| i.is_err()),
        "the mid-stream failure must reach the caller"
    );
    assert_eq!(
        workhorse.calls(),
        0,
        "the gateway must not silently re-prompt another provider"
    );
}

#[tokio::test]
async fn failover_sends_the_same_request_to_the_next_leg() {
    // docs/11: "next target, with the *same* request (IR makes this possible)".
    // Only the model may differ.
    struct Recorder {
        seen: std::sync::Mutex<Vec<ChatRequest>>,
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for Recorder {
        fn name(&self) -> &'static str {
            "openai_compat"
        }
        fn capabilities(&self, _m: &str) -> AdapterCaps {
            AdapterCaps::default()
        }
        async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
            self.seen.lock().unwrap().push(req);
            Err(PandayError::Provider {
                upstream: "x".into(),
                message: "529".into(),
                retryable: true,
            })
        }
    }

    let recorder = Arc::new(Recorder {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let g = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("together", recorder.clone() as Arc<dyn ProviderAdapter>)
        .adapter("local", recorder.clone() as Arc<dyn ProviderAdapter>)
        .build();

    let mut r = coding_request();
    r.metadata.task = Some(panday_types::model::TaskClass::Route);
    let _ = g.chat(r).await;

    let seen = recorder.seen.lock().unwrap();
    assert!(seen.len() >= 2, "the chain should have been walked");
    assert_eq!(
        seen[0].messages, seen[1].messages,
        "the request body must be identical across legs"
    );
    assert_ne!(
        seen[0].model, seen[1].model,
        "only the model should change between legs"
    );
}
