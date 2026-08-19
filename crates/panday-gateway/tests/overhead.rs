//! M11.6's p99 overhead budget (docs/11): "<3ms non-streaming, <1ms per stream
//! frame at 100 rps on one core".
//!
//! `#[ignore]`d, and deliberately so: a wall-clock assertion on shared CI
//! hardware fails for reasons that have nothing to do with the code, and a test
//! that flakes gets muted, which is worse than one that must be run on purpose.
//! Run it as:
//!
//! ```text
//! cargo test -p panday-gateway --test overhead --release -- --ignored --nocapture
//! ```
//!
//! It prints the distribution either way, so a regression is visible even when
//! the bar is met. Debug builds miss the bar by an order of magnitude — the
//! numbers recorded in docs/11 are release numbers.
//!
//! Two things this measures that a naive benchmark would not:
//!
//! - **Only the gateway's own work.** The adapter returns a ready-made stream, so
//!   what is left is routing, classification, cache keying, the breaker check and
//!   the metric writes. A benchmark that included a provider's latency would be
//!   measuring the internet.
//! - **The per-frame cost separately.** A stream frame passes through
//!   `capture_usage`, and the budget for that is three times tighter than for
//!   establishment because it is paid once per token chunk rather than once per
//!   request.

use panday_gateway::{AdapterCaps, Gateway, MemoryExactCache, ProviderAdapter};
use panday_router::PolicyRouter;
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    Usage,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEV_POLICY: &str = include_str!("../../panday-router/policy/dev.yaml");
const FRAMES: usize = 64;

struct Ready;

#[async_trait::async_trait]
impl ProviderAdapter for Ready {
    fn name(&self) -> &'static str {
        "openai_compat"
    }
    fn capabilities(&self, _m: &str) -> AdapterCaps {
        AdapterCaps::default()
    }
    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        let mut items: Vec<Result<StreamItem, PandayError>> = (0..FRAMES)
            .map(|i| {
                Ok(StreamItem::Delta {
                    text: format!("chunk {i} of a plausible completion"),
                })
            })
            .collect();
        items.push(Ok(StreamItem::Usage {
            usage: Usage {
                input_tokens: 4_000,
                output_tokens: 400,
                cache_read_tokens: 3_600,
                ..Default::default()
            },
        }));
        items.push(Ok(StreamItem::Done {
            reason: StopReason::EndTurn,
        }));
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

/// A realistic agent request: a long-ish transcript, tools declared (so the
/// classifier and the cache-eligibility check both do real work).
fn request() -> ChatRequest {
    let messages = (0..20)
        .map(|i| Message {
            role: if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: vec![ContentBlock::Text {
                text: format!("turn {i}: {}", "some conversation text ".repeat(20)),
            }],
            call_id: None,
            provider_call_id: None,
        })
        .collect();
    ChatRequest {
        model: ModelRef::auto(),
        messages,
        tools: vec![panday_types::model::ToolDef {
            name: "bash".into(),
            description: "run a command".into(),
            parameters: serde_json::json!({"type": "object"}),
        }],
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

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

fn report(label: &str, mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    let p50 = percentile(&samples, 0.50);
    let p99 = percentile(&samples, 0.99);
    let max = *samples.last().unwrap();
    println!(
        "{label}: n={} p50={:?} p99={:?} max={:?}",
        samples.len(),
        p50,
        p99,
        max
    );
    p99
}

#[test]
#[ignore = "wall-clock measurement; run explicitly in release (see module docs)"]
fn establishment_overhead_stays_under_the_budget() {
    // Single-threaded on purpose: docs/11's budget is "on one core", and a
    // multi-thread runtime would hide contention behind spare cores.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();

    let gateway = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("anthropic", Arc::new(Ready))
        .adapter("local", Arc::new(Ready))
        .adapter("together", Arc::new(Ready))
        .exact_cache(
            Arc::new(MemoryExactCache::new(1024)),
            Duration::from_secs(60),
        )
        .build();

    rt.block_on(async {
        // Warm up: the first calls pay for lazily-initialized metric series and
        // the policy's first classification, which no steady-state p99 includes.
        for _ in 0..50 {
            let _ = gateway.chat(request()).await;
        }

        // 1000 back-to-back requests is strictly harder than the specified
        // 100 rps: there is no idle time between them for anything to catch up.
        let mut samples = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let req = request();
            let t = Instant::now();
            let stream = gateway.chat(req).await.expect("established");
            samples.push(t.elapsed());
            drop(stream);
        }
        let p99 = report("establishment", samples);
        assert!(
            p99 < Duration::from_millis(3),
            "p99 establishment overhead {p99:?} exceeds the 3ms budget (docs/11 M11.6)"
        );
    });
}

#[test]
#[ignore = "wall-clock measurement; run explicitly in release (see module docs)"]
fn per_frame_overhead_stays_under_the_budget() {
    use futures_util::StreamExt;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();

    let gateway = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("anthropic", Arc::new(Ready))
        .adapter("local", Arc::new(Ready))
        .adapter("together", Arc::new(Ready))
        .build();

    rt.block_on(async {
        for _ in 0..10 {
            let mut s = gateway.chat(request()).await.unwrap();
            while s.next().await.is_some() {}
        }

        let mut samples = Vec::with_capacity(100 * (FRAMES + 2));
        for _ in 0..100 {
            let mut stream = gateway.chat(request()).await.unwrap();
            loop {
                let t = Instant::now();
                let item = stream.next().await;
                let elapsed = t.elapsed();
                if item.is_none() {
                    break;
                }
                samples.push(elapsed);
            }
        }
        let p99 = report("per frame", samples);
        assert!(
            p99 < Duration::from_millis(1),
            "p99 per-frame overhead {p99:?} exceeds the 1ms budget (docs/11 M11.6)"
        );
    });
}
