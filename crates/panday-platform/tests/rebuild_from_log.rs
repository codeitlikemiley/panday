//! M3.5 — replaying a session reproduces the ledger the live path wrote (docs/03, docs/17).
//!
//! ## What makes this a property test without a property-testing crate
//!
//! `proptest` is not in docs/02's dependency table, and the shrinking it buys is worth less here
//! than the shapes: what breaks a ledger rebuild is cache splits, failover mid-session, unpriced
//! models and zero-cost calls, and those are enumerable rather than discoverable. So the generator
//! below is a small deterministic LCG over those shapes, run across 200 sessions — deterministic so
//! a failure is reproducible from its seed, which is most of what shrinking would have given us.
//!
//! Every session runs through the **real** path: a fake provider, the real gateway, the real
//! `LedgerSink` writing to Postgres. Then the log is replayed and the two numbers are compared. A
//! test that priced the log twice and compared it with itself would prove nothing.

use panday_gateway::{Gateway, ProviderAdapter, UsageSink};
use panday_platform::ledger::{LedgerSink, OnWriteFailure};
use panday_platform::{pg, rebuild};
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::event::{Envelope, Event};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    Usage,
};
use panday_types::pricing::{CostModel, PriceTable, Pricing};
use panday_types::SessionId;
use std::sync::Arc;

async fn database() -> sqlx::PgPool {
    let url = pg::test_database_url()
        .expect("PANDAY_TEST_DATABASE_URL is unset — start deploy/integration-compose.yml first");
    let pool = pg::connect(&url).await.expect("connect");
    pg::migrate(&pool, &pg::migrations_dir())
        .await
        .expect("migrate");
    pool
}

fn prices() -> Arc<dyn CostModel> {
    Arc::new(
        PriceTable::new()
            .with(
                "anthropic/claude-sonnet-4-5",
                Pricing::anthropic_sonnet_class(),
            )
            .with(
                "anthropic/claude-opus-4-1",
                Pricing::anthropic_sonnet_class(),
            )
            .with("together/qwen3.5-9b", Pricing::openai_compat_class())
            .with("local/qwen3.5-4b", Pricing::local()),
    )
}

/// Deterministic, so a failing case is reproducible from its seed.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        // Numerical Recipes' constants; adequate for choosing test shapes and not pretending to be
        // anything more.
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 16
    }

    fn pick<'a, T>(&mut self, options: &'a [T]) -> &'a T {
        &options[(self.next() % options.len() as u64) as usize]
    }

    fn range(&mut self, low: u64, high: u64) -> u64 {
        low + self.next() % (high - low + 1)
    }
}

/// One scripted turn's usage.
#[derive(Clone, Copy, Debug)]
struct Turn {
    usage: Usage,
}

/// A provider that replays a script of turns, reporting usage per turn.
struct Scripted {
    turns: std::sync::Mutex<std::collections::VecDeque<Turn>>,
}

#[async_trait::async_trait]
impl ProviderAdapter for Scripted {
    fn name(&self) -> &'static str {
        "openai_compat"
    }
    fn capabilities(&self, _m: &str) -> panday_gateway::AdapterCaps {
        panday_gateway::AdapterCaps::default()
    }
    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| PandayError::Protocol("script exhausted".into()))?;
        let items: Vec<Result<StreamItem, PandayError>> = vec![
            Ok(StreamItem::Delta { text: "ok".into() }),
            Ok(StreamItem::Usage { usage: turn.usage }),
            Ok(StreamItem::Done {
                reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

fn request(account: AccountId, session: SessionId, model: &str) -> ChatRequest {
    ChatRequest {
        model: ModelRef(model.into()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "go".into() }],
            call_id: None,
            provider_call_id: None,
        }],
        tools: vec![],
        sampling: Sampling::default(),
        cache: Default::default(),
        stream: true,
        metadata: CallMeta {
            account,
            request: RequestId::new(),
            session: Some(session),
            turn: None,
            task: None,
        },
    }
}

/// Build the log the harness would have written for one turn, from what the gateway saw.
///
/// The harness's own `AssistantMessage` carries exactly this usage (its loop folds the stream), so
/// constructing it here keeps the test to one moving part — the *pricing* — rather than dragging in
/// tool execution. The shape is asserted against the harness's real output in
/// `panday-harness`'s own suites.
fn log_for(session: SessionId, turns: &[(ModelRef, Usage)]) -> Vec<Envelope> {
    let mut events = Vec::new();
    let mut seq = 0;
    let mut push = |event: Event, seq: &mut u64| {
        *seq += 1;
        events.push(Envelope {
            v: 1,
            session_id: session,
            seq: *seq,
            at: time::OffsetDateTime::UNIX_EPOCH,
            turn_id: None,
            event,
        });
    };
    for (model, usage) in turns {
        push(
            Event::TurnStarted {
                model: model.clone(),
                parent: None,
            },
            &mut seq,
        );
        push(
            Event::AssistantMessage {
                content: vec![ContentBlock::Text { text: "ok".into() }],
                usage: *usage,
            },
            &mut seq,
        );
        push(
            Event::TurnFinished {
                reason: StopReason::EndTurn,
                // The redundant summary docs/03 warns about: present in the log, and a rebuild that
                // counted it would double every turn.
                usage: *usage,
                cost_micros: 0,
            },
            &mut seq,
        );
    }
    events
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn two_hundred_generated_sessions_rebuild_to_the_ledger_exactly() {
    let pool = database().await;
    let prices = prices();
    let models = [
        "anthropic/claude-sonnet-4-5",
        "anthropic/claude-opus-4-1",
        "together/qwen3.5-9b",
        "local/qwen3.5-4b",
        // Unpriced on purpose: a rebuild must account for it the same way the live path does.
        "anthropic/claude-unreleased-7",
    ];

    let mut rng = Lcg(0xdead_beef);
    let mut checked = 0;
    let mut with_failover = 0;
    let mut with_unpriced = 0;

    for case in 0..200u32 {
        let account = AccountId(pg::create_account(&pool, "rebuild").await.unwrap());
        let session = SessionId::new();
        let turn_count = rng.range(1, 4);

        // Each turn gets its own model, so roughly every case exercises a model change mid-session —
        // which is where a rebuild that priced everything at the first model would disagree.
        let mut script = Vec::new();
        for _ in 0..turn_count {
            let model = ModelRef((*rng.pick(&models)).to_string());
            let input = rng.range(100, 200_000);
            // Cache reads are a subset of input (the `Usage` convention), and getting that wrong is
            // the single most likely pricing bug — so the generator respects it.
            let cache_read = rng.range(0, input);
            let usage = Usage {
                input_tokens: input,
                output_tokens: rng.range(0, 8_000),
                cache_read_tokens: cache_read,
                cache_write_tokens: 0,
                cache_write_1h_tokens: 0,
            };
            script.push((model, usage));
        }
        if script
            .iter()
            .map(|(m, _)| &m.0)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 1
        {
            with_failover += 1;
        }
        if script
            .iter()
            .any(|(m, u)| prices.cost_micros(m, *u).is_none())
        {
            with_unpriced += 1;
        }

        // The live path: gateway → LedgerSink → Postgres, one call per turn.
        let sink = Arc::new(LedgerSink::new(
            pool.clone(),
            prices.clone(),
            OnWriteFailure::FailClosed,
        ));
        for (model, usage) in &script {
            let adapter = Arc::new(Scripted {
                turns: std::sync::Mutex::new([Turn { usage: *usage }].into()),
            });
            let router = panday_router::PolicyRouter::from_yaml(include_str!(
                "../../panday-router/policy/dev.yaml"
            ))
            .expect("policy");
            let gateway = Gateway::builder(Arc::new(router))
                .adapter("anthropic", adapter.clone() as Arc<dyn ProviderAdapter>)
                .adapter("together", adapter.clone() as Arc<dyn ProviderAdapter>)
                .adapter("local", adapter as Arc<dyn ProviderAdapter>)
                .usage_sink(sink.clone() as Arc<dyn UsageSink>)
                .build();

            use futures_util::StreamExt;
            let mut stream = gateway
                .chat(request(account, session, &model.0))
                .await
                .expect("establish");
            while let Some(item) = stream.next().await {
                item.expect("stream item");
            }
        }

        // The rebuild: the same numbers, recomputed from the log alone.
        let events = log_for(session, &script);
        let rebuilt = rebuild::from_log(&events, prices.as_ref());
        let ledger_total = pg::balance_micros(&pool, account.0).await.unwrap();

        assert_eq!(
            rebuild::discrepancy(&rebuilt, ledger_total),
            0,
            "case {case} (seed 0xdeadbeef): ledger {ledger_total} vs rebuild {} for script {script:?}",
            rebuilt.total_micros
        );
        checked += 1;
    }

    assert_eq!(checked, 200);
    // The generator has to have produced the interesting shapes, or this test is 200 copies of the
    // easy case.
    assert!(
        with_failover > 40,
        "only {with_failover} sessions changed model"
    );
    assert!(
        with_unpriced > 20,
        "only {with_unpriced} sessions had an unpriced model"
    );
}

#[test]
fn the_rebuild_counts_usage_once_not_twice() {
    // `TurnFinished.usage` is a redundant summary (docs/03). `log_for` writes it, so a rebuild that
    // counted both would report double — and would have "agreed" with a live path that made the
    // same mistake.
    let session = SessionId::new();
    let usage = Usage {
        input_tokens: 1_000,
        output_tokens: 100,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
    };
    let model = ModelRef("anthropic/claude-sonnet-4-5".into());
    let events = log_for(session, &[(model.clone(), usage)]);
    let rebuilt = rebuild::from_log(&events, prices().as_ref());

    let once = prices().cost_micros(&model, usage).unwrap() as i64;
    assert_eq!(rebuilt.total_micros, -once);
    assert_eq!(rebuilt.usage.input_tokens, 1_000, "usage folded once");
    assert_eq!(rebuilt.turns, 1);
}

#[test]
fn each_turn_is_priced_at_the_model_that_turn_used() {
    // A session that failed over from opus to a cheap pool must not be priced as if it stayed.
    let session = SessionId::new();
    let usage = Usage {
        input_tokens: 100_000,
        output_tokens: 1_000,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
    };
    let expensive = ModelRef("anthropic/claude-sonnet-4-5".into());
    let cheap = ModelRef("together/qwen3.5-9b".into());

    let mixed = rebuild::from_log(
        &log_for(
            session,
            &[(expensive.clone(), usage), (cheap.clone(), usage)],
        ),
        prices().as_ref(),
    );
    let all_expensive = rebuild::from_log(
        &log_for(
            session,
            &[(expensive.clone(), usage), (expensive.clone(), usage)],
        ),
        prices().as_ref(),
    );
    assert!(
        mixed.total_micros > all_expensive.total_micros,
        "a cheaper second turn must cost less: {mixed:?} vs {all_expensive:?}"
    );
}

#[test]
fn an_unpriced_turn_is_reported_rather_than_skipped() {
    // A rebuild that silently ignored an unpriced model would agree with a live path that had also
    // ignored it, and neither would be right.
    let session = SessionId::new();
    let usage = Usage {
        input_tokens: 1_000,
        output_tokens: 10,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
    };
    let rebuilt = rebuild::from_log(
        &log_for(session, &[(ModelRef("anthropic/unknown-9".into()), usage)]),
        prices().as_ref(),
    );
    assert_eq!(rebuilt.total_micros, 0);
    assert_eq!(rebuilt.unpriced_turns, 1);
    // The tokens are still counted, so the gap is visible rather than invisible.
    assert_eq!(rebuilt.usage.input_tokens, 1_000);
}

#[test]
fn a_discrepancy_keeps_its_sign() {
    // Over-billing and under-billing are different incidents with different responses.
    let rebuilt = rebuild::Rebuilt {
        total_micros: -1_000,
        turns: 1,
        unpriced_turns: 0,
        usage: Usage::default(),
    };
    assert_eq!(rebuild::discrepancy(&rebuilt, -1_000), 0);
    // The ledger charged more than the log justifies.
    assert_eq!(rebuild::discrepancy(&rebuilt, -1_500), -500);
    // The ledger charged less.
    assert_eq!(rebuild::discrepancy(&rebuilt, -500), 500);
}
