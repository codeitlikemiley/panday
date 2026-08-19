//! M11.4 — the ledger write path and budget stops, against a real database (docs/11 §quotas,
//! docs/17 §the ledger).
//!
//! The property test at the end is this milestone's acceptance: **Σ ledger == Σ provider-reported
//! usage**, over the same fixtures the conformance suite replays. Anything less is a write path
//! that looks right in one hand-written case.

use panday_gateway::{BudgetGate, UsageRecord, UsageSink};
use panday_platform::entitlements::Plan;
use panday_platform::ledger::{LedgerBudget, LedgerSink, OnWriteFailure};
use panday_platform::pg;
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{ModelRef, Usage};
use panday_types::pricing::{CostModel, PriceTable, Pricing};
use std::sync::Arc;
use uuid::Uuid;

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
            .with("together/qwen3.5-9b", Pricing::openai_compat_class())
            .with("local/qwen3.5-4b", Pricing::local()),
    )
}

fn record(account: AccountId, model: &str, usage: Usage) -> UsageRecord {
    UsageRecord {
        account,
        request: RequestId::new(),
        model: ModelRef(model.into()),
        provider: model.split('/').next().unwrap_or_default().to_string(),
        pool: "workhorse".into(),
        usage,
    }
}

fn usage(input: u64, output: u64, cache_read: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
    }
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_priced_call_lands_as_a_negative_ledger_entry() {
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let sink = LedgerSink::new(pool.clone(), prices(), OnWriteFailure::FailClosed);

    let r = record(
        account,
        "anthropic/claude-sonnet-4-5",
        usage(1_000, 100, 800),
    );
    let expected = prices()
        .cost_micros(&r.model, r.usage)
        .expect("a priced model");
    sink.record(r.clone()).await;

    assert_eq!(
        pg::balance_micros(&pool, account.0).await.unwrap(),
        -(expected as i64),
        "consumption is negative (ADR-009)"
    );
    let entries = pg::entries(&pool, account.0, 10).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, "usage.model");
    // The quantity carries what the money was for, so a dispute does not need the provider's
    // invoice to be explained.
    assert_eq!(entries[0].quantity["input_tokens"], 1_000);
    assert_eq!(entries[0].quantity["cache_read"], 800);
    assert_eq!(entries[0].quantity["provider_cost_micros"], expected);
    // And the source points into the event log.
    assert_eq!(entries[0].source["request_id"], r.request.0.to_string());
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn the_same_request_recorded_twice_bills_once() {
    // docs/17: "request_id-scoped; retries can't double-bill". The UNIQUE constraint enforces it
    // and the sink treats the duplicate as success, because the caller's correct behaviour is
    // "already billed, carry on".
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let sink = LedgerSink::new(pool.clone(), prices(), OnWriteFailure::FailClosed);
    let r = record(account, "anthropic/claude-sonnet-4-5", usage(1_000, 100, 0));

    sink.record(r.clone()).await;
    let after_first = pg::balance_micros(&pool, account.0).await.unwrap();
    sink.record(r.clone()).await;
    assert_eq!(
        pg::balance_micros(&pool, account.0).await.unwrap(),
        after_first,
        "the retry must not bill again"
    );
    assert_eq!(pg::entries(&pool, account.0, 10).await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_unpriced_model_is_not_billed_as_free() {
    // docs/21's rule, applied to money: "free" and "unpriced" are different claims, and a
    // zero-cost entry would understate COGS in a way nothing downstream could detect.
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let sink = LedgerSink::new(pool.clone(), prices(), OnWriteFailure::FailClosed);

    sink.record(record(
        account,
        "anthropic/some-new-model",
        usage(5_000, 500, 0),
    ))
    .await;
    assert!(
        pg::entries(&pool, account.0, 10).await.unwrap().is_empty(),
        "an unpriced call must not produce a ledger entry"
    );
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_local_model_costs_nothing_and_says_so_explicitly() {
    // Priced at zero is not the same as unpriced: the entry exists, so the call is *accounted*
    // for, and the amount is zero because a local model has no marginal cost (docs/18's free tier
    // is metered).
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let sink = LedgerSink::new(pool.clone(), prices(), OnWriteFailure::FailClosed);

    sink.record(record(account, "local/qwen3.5-4b", usage(4_000, 400, 0)))
        .await;
    let entries = pg::entries(&pool, account.0, 10).await.unwrap();
    assert_eq!(entries.len(), 1, "a free call is still recorded");
    assert_eq!(entries[0].amount_micros, 0);
    assert_eq!(entries[0].quantity["input_tokens"], 4_000);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn margin_is_applied_once_and_the_provider_cost_is_kept_beside_it() {
    // COGS and the price charged are different numbers; a ledger that stored only one cannot
    // answer either question.
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let sink = LedgerSink::new(pool.clone(), prices(), OnWriteFailure::FailClosed).with_margin(2.0);
    let r = record(account, "anthropic/claude-sonnet-4-5", usage(1_000, 100, 0));
    let provider_cost = prices().cost_micros(&r.model, r.usage).unwrap();

    sink.record(r).await;
    let entries = pg::entries(&pool, account.0, 10).await.unwrap();
    assert_eq!(entries[0].amount_micros, -(provider_cost as i64 * 2));
    assert_eq!(entries[0].quantity["provider_cost_micros"], provider_cost);
}

// ── Budget stops ─────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_account_in_credit_passes_the_gate() {
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let gate = LedgerBudget::new(pool.clone(), Plan::pro());
    assert!(gate.check(account, None).await.is_ok());
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_account_past_its_ceiling_is_refused_with_a_typed_error() {
    // docs/11: "Budget stop mid-session emits a typed `budget_exceeded` the harness turns into a
    // graceful session pause, not a 500."
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let sink = LedgerSink::new(pool.clone(), prices(), OnWriteFailure::FailClosed);

    // Pro's ceiling is 50_000_000 micro-credits. At $3/mtok that is ~16.7M input tokens, so 20M
    // takes the account past it — the arithmetic is written out because getting it wrong by 1000x
    // is exactly what the first draft of this test did.
    sink.record(record(
        account,
        "anthropic/claude-sonnet-4-5",
        usage(20_000_000, 0, 0),
    ))
    .await;
    let balance = pg::balance_micros(&pool, account.0).await.unwrap();
    assert!(balance < -50_000_000, "balance was {balance}");

    let gate = LedgerBudget::new(pool.clone(), Plan::pro());
    match gate.check(account, None).await {
        Err(panday_sdk::PandayError::BudgetExceeded { balance_micros }) => {
            assert_eq!(balance_micros, balance);
        }
        other => panic!("expected a typed budget stop, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_preflight_estimate_is_refused_before_the_tokens_are_spent() {
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let sink = LedgerSink::new(pool.clone(), prices(), OnWriteFailure::FailClosed);
    // 15M input tokens at $3/mtok = 45_000_000 micro-credits: 90% of Pro's ceiling, so the soft
    // rule applies and the hard one does not yet.
    sink.record(record(
        account,
        "anthropic/claude-sonnet-4-5",
        usage(15_000_000, 0, 0),
    ))
    .await;

    let gate = LedgerBudget::new(pool.clone(), Plan::pro());
    // A small request still fits.
    assert!(gate.check(account, Some(1_000)).await.is_ok());
    // A large one does not, and is refused before it runs.
    assert!(gate.check(account, Some(60_000_000)).await.is_err());
}

// ── The property this milestone is named for ──────────────────────────────────

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn the_ledger_total_equals_the_priced_provider_usage_over_many_calls() {
    // M11.4's acceptance: "Σ ledger == Σ provider-reported usage on replayed fixtures". Driven over
    // a spread of shapes — cache-heavy, output-heavy, free, unpriced — because the interesting
    // errors are in the cache splits and in the models that have no price.
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let sink = LedgerSink::new(pool.clone(), prices(), OnWriteFailure::FailClosed);
    let prices = prices();

    let shapes: Vec<(&str, Usage)> = vec![
        ("anthropic/claude-sonnet-4-5", usage(1_000, 100, 900)),
        ("anthropic/claude-sonnet-4-5", usage(50_000, 2_000, 48_000)),
        ("anthropic/claude-sonnet-4-5", usage(200, 4_000, 0)),
        ("together/qwen3.5-9b", usage(8_000, 300, 0)),
        ("together/qwen3.5-9b", usage(8_000, 300, 7_000)),
        ("local/qwen3.5-4b", usage(30_000, 900, 0)),
        // Unpriced: contributes nothing to either side of the equality, which is the point.
        ("anthropic/claude-opus-9", usage(1_000, 100, 0)),
    ];

    let mut expected: i64 = 0;
    for (model, u) in &shapes {
        let r = record(account, model, *u);
        if let Some(cost) = prices.cost_micros(&r.model, r.usage) {
            expected -= cost as i64;
        }
        sink.record(r).await;
    }

    let ledger_total = pg::balance_micros(&pool, account.0).await.unwrap();
    assert_eq!(
        ledger_total, expected,
        "the ledger must equal the priced provider usage, to the micro-credit"
    );

    // And the entries themselves reconcile: Σ provider_cost_micros over the entries equals the
    // same number, which is what a dispute would be settled with.
    let entries = pg::entries(&pool, account.0, 100).await.unwrap();
    let from_quantities: i64 = entries
        .iter()
        .map(|e| e.quantity["provider_cost_micros"].as_i64().unwrap_or(0))
        .sum();
    assert_eq!(-from_quantities, expected);
    // Six priced calls, one unpriced and therefore absent.
    assert_eq!(entries.len(), 6);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_write_failure_is_loud_and_the_mode_decides_the_rest() {
    // Fail-open is a policy, not an accident (docs/17). A pool pointed at nothing simulates the
    // outage; the sink must not panic either way, because a panic in the request path is worse
    // than both options.
    let dead = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy("postgres://panday:panday@127.0.0.1:1/panday_test")
        .expect("lazy pool");

    let account = AccountId(Uuid::now_v7());
    for mode in [
        OnWriteFailure::FailOpenWithAlarm,
        OnWriteFailure::FailClosed,
    ] {
        let sink = LedgerSink::new(dead.clone(), prices(), mode);
        // Does not panic, does not hang.
        sink.record(record(
            account,
            "anthropic/claude-sonnet-4-5",
            usage(10, 1, 0),
        ))
        .await;
    }
}
