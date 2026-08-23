//! M11.6 — exact cache and circuit breakers (docs/11).

use panday_gateway::cache::{is_cacheable, CacheKey, ExactCache, MemoryExactCache};
use panday_gateway::circuit::{BreakerConfig, Breakers, State};
use panday_gateway::{AdapterCaps, Gateway, ProviderAdapter};
use panday_router::PolicyRouter;
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    ToolDef, Usage,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const DEV_POLICY: &str = include_str!("../../panday-router/policy/dev.yaml");

/// Counts how many times the provider was actually called.
struct Counting {
    calls: AtomicUsize,
    fail_until: usize,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            fail_until: 0,
        })
    }
    fn failing(fail_until: usize) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            fail_until,
        })
    }
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for Counting {
    fn name(&self) -> &'static str {
        "openai_compat"
    }
    fn capabilities(&self, _m: &str) -> AdapterCaps {
        AdapterCaps::default()
    }
    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n <= self.fail_until {
            return Err(PandayError::Provider {
                upstream: "local".into(),
                message: "overloaded".into(),
                retryable: true,
            });
        }
        let items: Vec<Result<StreamItem, PandayError>> = vec![
            Ok(StreamItem::Delta {
                text: format!("answer {n}"),
            }),
            Ok(StreamItem::Usage {
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 10,
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

fn request(account: AccountId, prompt: &str) -> ChatRequest {
    ChatRequest {
        model: ModelRef("local/qwen3.5-4b".into()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: prompt.into(),
            }],
            call_id: None,
            provider_call_id: None,
        }],
        tools: vec![],
        sampling: Sampling {
            // docs/11: the exact cache is for `temperature=0` traffic.
            temperature: Some(0.0),
            ..Default::default()
        },
        cache: Default::default(),
        stream: true,
        metadata: CallMeta {
            account,
            request: RequestId::new(),
            session: None,
            turn: None,
            task: None,
        },
    }
}

async fn drain(g: &Gateway, req: ChatRequest) -> Result<Vec<StreamItem>, PandayError> {
    use futures_util::StreamExt;
    let mut s = g.chat(req).await?;
    let mut out = Vec::new();
    while let Some(item) = s.next().await {
        out.push(item?);
    }
    Ok(out)
}

fn cached_gateway(adapter: Arc<dyn ProviderAdapter>) -> Gateway {
    Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("local", adapter)
        .exact_cache(Arc::new(MemoryExactCache::new(16)), Duration::from_secs(60))
        .build()
}

// ── Exact cache ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_identical_request_is_served_without_calling_the_provider() {
    let adapter = Counting::new();
    let g = cached_gateway(adapter.clone());
    let account = AccountId::new();

    let first = drain(&g, request(account, "2 + 2?")).await.unwrap();
    let second = drain(&g, request(account, "2 + 2?")).await.unwrap();

    assert_eq!(adapter.count(), 1, "the second call should have been a hit");
    assert_eq!(
        text_of(&first),
        text_of(&second),
        "a hit must be the same answer"
    );
}

#[tokio::test]
async fn a_cache_hit_reports_no_output_tokens_and_no_fresh_input() {
    // The ledger prices what it is told. Replaying the original usage would bill
    // twice for one purchase; dropping the frame would make the request vanish
    // from a client's own accounting.
    let adapter = Counting::new();
    let g = cached_gateway(adapter.clone());
    let account = AccountId::new();

    drain(&g, request(account, "hi")).await.unwrap();
    let hit = drain(&g, request(account, "hi")).await.unwrap();

    let usage = hit
        .iter()
        .find_map(|i| match i {
            StreamItem::Usage { usage } => Some(*usage),
            _ => None,
        })
        .expect("a hit still reports usage");
    assert_eq!(usage.output_tokens, 0);
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(
        usage.cache_read_tokens, 100,
        "all of it came from cache, not from the provider"
    );
}

#[tokio::test]
async fn one_tenants_prompt_never_serves_another_tenants_response() {
    // The finding docs/20's cache-key audit exists to make. Identical prompts
    // across tenants are what a shared eval harness produces, so this collision
    // is likely rather than theoretical — and the leak would look like a cache
    // working well.
    let adapter = Counting::new();
    let g = cached_gateway(adapter.clone());

    drain(&g, request(AccountId::new(), "same prompt"))
        .await
        .unwrap();
    drain(&g, request(AccountId::new(), "same prompt"))
        .await
        .unwrap();

    assert_eq!(adapter.count(), 2, "a different account must miss");
}

#[tokio::test]
async fn a_different_prompt_or_a_different_model_misses() {
    let adapter = Counting::new();
    let g = cached_gateway(adapter.clone());
    let account = AccountId::new();

    drain(&g, request(account, "a")).await.unwrap();
    drain(&g, request(account, "b")).await.unwrap();
    // Same prompt, different model: a pinned model bypasses pool selection
    // (docs/12) so this reaches the same adapter, and it must not be served the
    // other model's answer.
    let mut other_model = request(account, "a");
    other_model.model = ModelRef("local/other".into());
    drain(&g, other_model).await.unwrap();

    assert_eq!(adapter.count(), 3, "three distinct requests, three calls");
    // And the first one is still cached.
    drain(&g, request(account, "a")).await.unwrap();
    assert_eq!(adapter.count(), 3);
}

#[test]
fn the_key_ignores_ids_and_notices_everything_that_changes_the_answer() {
    let account = AccountId::new();
    let a = request(account, "hello");
    let mut b = request(account, "hello");
    // A fresh `request_id` every time; if it were in the key the hit rate would
    // be zero and the cache a slow memory leak.
    assert_ne!(a.metadata.request, b.metadata.request);
    assert_eq!(CacheKey::of(&a), CacheKey::of(&b));

    // Sampling changes the answer.
    b.sampling.max_tokens = Some(10);
    assert_ne!(CacheKey::of(&a), CacheKey::of(&b));

    // So do the cache hints: they change what the provider is asked for.
    let mut c = request(account, "hello");
    c.cache.extended_ttl = true;
    assert_ne!(CacheKey::of(&a), CacheKey::of(&c));

    // Streaming is the same completion delivered differently.
    let mut d = request(account, "hello");
    d.stream = !a.stream;
    assert_eq!(CacheKey::of(&a), CacheKey::of(&d));
}

#[test]
fn only_deterministic_toolless_requests_are_cacheable() {
    let account = AccountId::new();
    assert!(is_cacheable(&request(account, "x")));

    // Absent temperature is NOT zero — providers default to ~1. Caching one
    // sample of a distribution and serving it forever would look like a model
    // that stopped thinking.
    let mut unset = request(account, "x");
    unset.sampling.temperature = None;
    assert!(!is_cacheable(&unset));

    let mut hot = request(account, "x");
    hot.sampling.temperature = Some(0.7);
    assert!(!is_cacheable(&hot));

    // A request with tools is part of an agent loop whose next step depends on
    // real execution. Replaying a cached tool call would have the harness act on
    // a decision made about a different workspace.
    let mut agentic = request(account, "x");
    agentic.tools = vec![ToolDef {
        name: "bash".into(),
        description: "run".into(),
        parameters: serde_json::json!({}),
    }];
    assert!(!is_cacheable(&agentic));
}

#[tokio::test]
async fn caching_is_off_unless_a_ttl_is_configured() {
    let adapter = Counting::new();
    let g = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("local", adapter.clone())
        .build();
    let account = AccountId::new();
    drain(&g, request(account, "hi")).await.unwrap();
    drain(&g, request(account, "hi")).await.unwrap();
    assert_eq!(adapter.count(), 2, "no TTL means no caching");
}

#[tokio::test]
async fn memory_exact_cache_satisfies_the_conformance_suite() {
    // Expiry, tenant isolation, overwrite and round-tripping moved into the shared suite, which
    // `PgExactCache` runs too. What stays below is what is genuinely `MemoryExactCache`'s: a PG
    // unlogged table is bounded by disk and a reaper, not by an entry count, and `len()` is an
    // inherent method rather than part of the trait.
    panday_gateway::cache::conformance::run(
        "MemoryExactCache",
        std::sync::Arc::new(|| {
            Box::pin(async {
                std::sync::Arc::new(MemoryExactCache::new(64))
                    as std::sync::Arc<dyn panday_gateway::cache::ExactCache>
            })
        }),
    )
    .await;
}

#[tokio::test]
async fn the_memory_cache_stays_bounded_and_keeps_the_newest() {
    let cache = MemoryExactCache::new(2);
    let account = AccountId::new();
    let key = |n: u32| CacheKey {
        account,
        digest: format!("{n:064x}"),
    };
    let response = panday_gateway::CachedResponse { items: vec![] };

    for n in 2..=5 {
        cache
            .put(key(n), response.clone(), Duration::from_secs(60))
            .await;
    }
    assert!(cache.len() <= 2, "capacity was {}", cache.len());
    assert!(cache.get(&key(5)).await.is_some(), "the newest survives");
}

// ── Circuit breakers ─────────────────────────────────────────────────────────

fn quick() -> BreakerConfig {
    BreakerConfig {
        window: 10,
        min_samples: 4,
        error_rate: 0.5,
        cooldown: Duration::from_millis(80),
    }
}

#[test]
fn one_failure_on_a_cold_route_does_not_open_the_breaker() {
    // Without a minimum sample size the first failure is a 100% error rate, and
    // the breaker opens on one bad request.
    let b = Breakers::new(quick());
    b.record_failure("local", "m");
    assert_eq!(b.state("local", "m"), State::Closed);
    assert!(b.allow("local", "m"));
}

#[test]
fn a_route_failing_most_calls_opens_and_stops_being_attempted() {
    let b = Breakers::new(quick());
    for _ in 0..4 {
        b.record_failure("local", "m");
    }
    assert_eq!(b.state("local", "m"), State::Open);
    assert!(!b.allow("local", "m"), "an open breaker refuses the call");
    // Another (provider, model) is unaffected — one overloaded model must not
    // take a healthy pool down with it.
    assert!(b.allow("local", "other"));
    assert!(b.allow("anthropic", "m"));
}

#[test]
fn a_busy_route_with_a_low_error_rate_stays_closed() {
    // A failure *count* would open this route; a rate does not.
    let b = Breakers::new(quick());
    for _ in 0..9 {
        b.record_success("local", "m");
    }
    b.record_failure("local", "m");
    assert_eq!(b.state("local", "m"), State::Closed);
}

#[test]
fn exactly_one_probe_goes_through_when_the_cooldown_expires() {
    let b = Breakers::new(quick());
    for _ in 0..4 {
        b.record_failure("local", "m");
    }
    std::thread::sleep(Duration::from_millis(120));

    assert_eq!(b.state("local", "m"), State::HalfOpen);
    assert!(b.allow("local", "m"), "the probe is allowed");
    assert!(
        !b.allow("local", "m"),
        "a burst must send one probe, not all of them"
    );
}

#[test]
fn a_successful_probe_closes_the_breaker_despite_a_window_full_of_failures() {
    // The window still holds the failures that opened it, so a rate test would
    // keep the breaker open forever however healthy the route now is.
    let b = Breakers::new(quick());
    for _ in 0..4 {
        b.record_failure("local", "m");
    }
    std::thread::sleep(Duration::from_millis(120));
    assert!(b.allow("local", "m"));
    b.record_success("local", "m");

    assert_eq!(b.state("local", "m"), State::Closed);
    assert!(b.allow("local", "m"));
}

#[test]
fn a_failed_probe_reopens_the_breaker_for_another_cooldown() {
    let b = Breakers::new(quick());
    for _ in 0..4 {
        b.record_failure("local", "m");
    }
    std::thread::sleep(Duration::from_millis(120));
    assert!(b.allow("local", "m"));
    b.record_failure("local", "m");

    assert_eq!(b.state("local", "m"), State::Open);
    assert!(!b.allow("local", "m"));
}

#[tokio::test]
async fn a_tripped_breaker_makes_the_gateway_stop_calling_the_provider() {
    // 4 failures at 50% of a 4-sample minimum opens it; the 5th request must not
    // reach the adapter at all.
    let adapter = Counting::failing(100);
    let g = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("local", adapter.clone())
        .breakers(Arc::new(Breakers::new(quick())))
        .build();
    let account = AccountId::new();

    for _ in 0..4 {
        let _ = drain(&g, request(account, "hi")).await;
    }
    let calls_before = adapter.count();
    let err = drain(&g, request(account, "hi")).await.unwrap_err();

    assert_eq!(
        adapter.count(),
        calls_before,
        "the provider must not be called through an open breaker"
    );
    assert_eq!(g.circuit_state("local", "local/qwen3.5-4b"), State::Open);
    // The caller gets an outage error naming the attempt, not a hang.
    assert!(
        matches!(err, PandayError::ModelUnavailable { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_bad_request_does_not_open_a_breaker_for_everyone_else() {
    // An invalid request fails identically on every provider. Counting it against
    // the route would let one broken client take a healthy model offline.
    struct Rejecting;
    #[async_trait::async_trait]
    impl ProviderAdapter for Rejecting {
        fn name(&self) -> &'static str {
            "openai_compat"
        }
        fn capabilities(&self, _m: &str) -> AdapterCaps {
            AdapterCaps::default()
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
            Err(PandayError::Protocol("unsupported field".into()))
        }
    }

    let breakers = Arc::new(Breakers::new(quick()));
    let g = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("local", Arc::new(Rejecting))
        .breakers(breakers.clone())
        .build();
    let account = AccountId::new();

    for _ in 0..6 {
        let _ = drain(&g, request(account, "hi")).await;
    }
    assert_eq!(breakers.state("local", "local/qwen3.5-4b"), State::Closed);
}

fn text_of(items: &[StreamItem]) -> String {
    items
        .iter()
        .filter_map(|i| match i {
            StreamItem::Delta { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}
