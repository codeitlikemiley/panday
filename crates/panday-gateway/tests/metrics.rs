//! M21.2 — the Prometheus endpoint and what the gateway puts on it (docs/21).
//!
//! The metric registry is process-global (a scrape must see one number per
//! series, not one per handler), so these tests assert on *series presence* and
//! on *deltas*, never on absolute values — an absolute assertion here would
//! pass or fail depending on which other test in this binary ran first.

use panday_gateway::{AdapterCaps, Gateway, IngressState, ProviderAdapter};
use panday_router::PolicyRouter;
use panday_sdk::{ItemStream, PandayError};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    TaskClass, Usage,
};
use panday_types::pricing::{PriceTable, Pricing};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DEV_POLICY: &str = include_str!("../../panday-router/policy/dev.yaml");

struct Fake {
    fail: Option<fn() -> PandayError>,
}

#[async_trait::async_trait]
impl ProviderAdapter for Fake {
    fn name(&self) -> &'static str {
        "openai_compat"
    }
    fn capabilities(&self, _m: &str) -> AdapterCaps {
        AdapterCaps::default()
    }
    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        if let Some(f) = self.fail {
            return Err(f());
        }
        let items: Vec<Result<StreamItem, PandayError>> = vec![
            Ok(StreamItem::Delta { text: "hi".into() }),
            Ok(StreamItem::Usage {
                usage: Usage {
                    input_tokens: 1_000,
                    output_tokens: 100,
                    cache_read_tokens: 800,
                    cache_write_tokens: 0,
                    cache_write_1h_tokens: 0,
                },
            }),
            Ok(StreamItem::Done {
                reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

fn request(model: &str, task: Option<TaskClass>) -> ChatRequest {
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
            task,
        },
    }
}

/// Drain a stream so the `Usage` item is actually seen — metrics come off the
/// same event the ledger does, and neither fires for a stream nobody polls.
async fn drain(g: &Gateway, req: ChatRequest) -> Result<(), PandayError> {
    use futures_util::StreamExt;
    use panday_sdk::ModelClient;
    let mut s = g.chat(req).await?;
    while let Some(item) = s.next().await {
        item?;
    }
    Ok(())
}

fn series(text: &str, name_and_labels: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.starts_with(name_and_labels))
        .and_then(|l| l.rsplit_once(' '))
        .and_then(|(_, v)| v.parse().ok())
}

#[tokio::test]
async fn a_successful_call_lands_tokens_latency_and_a_route_decision() {
    let g = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("local", Arc::new(Fake { fail: None }))
        .build();

    let before = panday_sdk::metrics::render();
    let tokens_before = series(
        &before,
        "panday_tokens_total{provider=\"local\",model=\"local/qwen3.5-4b\",kind=\"input\"}",
    )
    .unwrap_or(0.0);

    drain(&g, request("local/qwen3.5-4b", Some(TaskClass::Route)))
        .await
        .unwrap();

    let after = panday_sdk::metrics::render();
    assert_eq!(
        series(
            &after,
            "panday_tokens_total{provider=\"local\",model=\"local/qwen3.5-4b\",kind=\"input\"}"
        )
        .unwrap(),
        tokens_before + 1000.0
    );
    assert!(after.contains("kind=\"cache_read\""));
    assert!(after.contains("panday_model_latency_seconds_count{provider=\"local\""));
    // The rule that matched is on the series, not just the pool: docs/21 asks
    // "is the router earning its keep", which is a per-rule question. A pinned
    // model bypasses rule selection, so the rule is `pinned` and there is no
    // pool — labelled as such rather than as an empty string, which on a
    // dashboard reads like a bug.
    assert!(
        after.contains(
            "panday_route_decisions_total{rule=\"pinned\",pool=\"pinned\",task=\"route\"}"
        ),
        "{after}"
    );
}

#[tokio::test]
async fn cogs_appears_only_when_the_model_has_a_configured_price() {
    // An invented price on a money dashboard is worse than a gap in one.
    let unpriced = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("local", Arc::new(Fake { fail: None }))
        .build();
    drain(&unpriced, request("local/qwen3.5-4b", None))
        .await
        .unwrap();
    let text = panday_sdk::metrics::render();
    assert!(
        text.contains("panday_unpriced_calls_total{provider=\"local\",model=\"local/qwen3.5-4b\"}"),
        "{text}"
    );

    let priced = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("local", Arc::new(Fake { fail: None }))
        .costs(Arc::new(
            PriceTable::new().with("local/qwen3.5-4b", Pricing::anthropic_sonnet_class()),
        ))
        .build();
    drain(&priced, request("local/qwen3.5-4b", None))
        .await
        .unwrap();

    let text = panday_sdk::metrics::render();
    // 200 fresh input @ $3/mtok + 800 cache-read @ 10% + 100 output @ $15/mtok
    // = $0.0006 + $0.00024 + $0.0015 = $0.00234
    let cost = series(
        &text,
        "panday_cost_usd_total{pool=\"pinned\",provider=\"local\",model=\"local/qwen3.5-4b\"}",
    )
    .unwrap_or_else(|| panic!("{text}"));
    assert!(
        (cost - 0.00234).abs() < 1e-9,
        "cost was {cost}, expected 0.00234"
    );
}

#[tokio::test]
async fn a_failing_provider_is_counted_by_error_kind_not_by_message() {
    // The error message carries provider text and ids. As a label that is
    // unbounded cardinality — the thing that takes a Prometheus down.
    let g = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter(
            "local",
            Arc::new(Fake {
                fail: Some(|| PandayError::RateLimited {
                    retry_after_ms: 1234,
                }),
            }),
        )
        .build();

    let _ = drain(&g, request("local/qwen3.5-4b", None)).await;
    let text = panday_sdk::metrics::render();
    assert!(
        text.contains("code=\"rate_limited\""),
        "the error kind should be the label: {text}"
    );
    assert!(
        !text.contains("1234"),
        "the retry hint must not become a label value: {text}"
    );
    assert!(text.contains("outcome=\"error\""), "{text}");
}

#[tokio::test]
async fn the_cache_read_ratio_is_a_distribution_not_a_series_per_session() {
    // docs/21's table says "per session"; sessions are unbounded, so what
    // Prometheus gets is the histogram and what answers a question about one
    // session is the log (`panday replay`).
    let g = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("local", Arc::new(Fake { fail: None }))
        .build();
    for _ in 0..3 {
        drain(&g, request("local/qwen3.5-4b", None)).await.unwrap();
    }
    let text = panday_sdk::metrics::render();
    assert!(text.contains("panday_cache_read_ratio_bucket{pool=\"pinned\""));
    assert!(
        !text.contains("session"),
        "no session id may appear in a metric: {text}"
    );
    assert!(
        !text.contains("account"),
        "no account id may appear in a metric: {text}"
    );
}

#[tokio::test]
async fn the_endpoint_serves_prometheus_text() {
    let g = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("local", Arc::new(Fake { fail: None }))
        .build();
    let app = panday_gateway::ingress::router(IngressState::open(Arc::new(g), AccountId::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let mut stream = TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(
            format!("GET /metrics HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);

    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(
        text.to_lowercase()
            .contains("content-type: text/plain; version=0.0.4"),
        "a scraper checks the content type: {text}"
    );
    assert!(
        text.contains("panday_metrics_series_dropped_total"),
        "{text}"
    );
    // No auth header was sent. A metrics endpoint that 401s is one nobody
    // scrapes; it carries counts only, never content or ids.
    assert!(!text.contains("401"));
}
