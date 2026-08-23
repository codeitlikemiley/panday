//! M14.7 — sandbox-seconds reach the ledger (docs/14, docs/17 §the second metered good).

use panday_harness::{SandboxUsage, SandboxUsageSink};
use panday_platform::ledger::{OnWriteFailure, SandboxLedger, TierPrices};
use panday_platform::pg;
use panday_sandbox::SandboxTier;
use panday_types::{AccountId, CallId, SessionId};
use std::time::Duration;

async fn database() -> sqlx::PgPool {
    let url = pg::test_database_url()
        .expect("PANDAY_TEST_DATABASE_URL is unset — start deploy/integration-compose.yml first");
    let pool = pg::connect(&url).await.expect("connect");
    pg::migrate(&pool, &pg::migrations_dir())
        .await
        .expect("migrate");
    pool
}

fn usage(account: AccountId, tier: SandboxTier, millis: u64) -> SandboxUsage {
    SandboxUsage {
        account,
        session: SessionId::new(),
        call_id: CallId::new(),
        tool: "bash".into(),
        tier,
        duration: Duration::from_millis(millis),
    }
}

#[test]
fn a_tier_is_priced_per_second_and_billed_per_millisecond() {
    // A loop of forty 50ms calls rounded up to a second each would be charged forty seconds. That is
    // not a rounding error, it is a different price.
    //
    // Pure arithmetic, so no database: the first draft called a method that needed a pool and had to
    // build one to check a multiplication — which is how a unit test acquires a Docker dependency.
    let prices = TierPrices::default();
    assert_eq!(
        prices.amount_micros(SandboxTier::T2OsJail, Duration::from_millis(250)),
        25
    );
    assert_eq!(
        prices.amount_micros(SandboxTier::T2OsJail, Duration::from_millis(50)) * 40,
        200,
        "forty 50ms calls cost two seconds, not forty"
    );
    // Sub-millisecond work rounds to nothing rather than to a whole unit.
    assert_eq!(
        prices.amount_micros(SandboxTier::T1Wasm, Duration::from_micros(500)),
        0
    );
}

#[test]
fn the_tiers_are_priced_by_what_they_actually_cost() {
    // One price for "a sandbox second" would either overcharge for T0 or give T3 away.
    let prices = TierPrices::default();
    // Zero, deliberately: a T0 tool is a Rust function in our own process, and billing for it would
    // be billing for CPU already paid for in the request.
    assert_eq!(prices.for_tier(SandboxTier::T0InProcess), 0);
    assert!(prices.for_tier(SandboxTier::T1Wasm) > 0);
    assert!(prices.for_tier(SandboxTier::T2OsJail) > prices.for_tier(SandboxTier::T1Wasm));
    assert!(prices.for_tier(SandboxTier::T3MicroVm) > prices.for_tier(SandboxTier::T2OsJail));
    // Same isolation class, different host: T3-remote is our sandbox-seconds
    // record, not the operator's CodeSandbox credit bill.
    assert_eq!(
        prices.for_tier(SandboxTier::T3Remote),
        prices.for_tier(SandboxTier::T3MicroVm)
    );
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn sandbox_time_lands_in_the_ledger_beside_model_usage() {
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let ledger = SandboxLedger::new(pool.clone(), OnWriteFailure::FailClosed);

    ledger
        .record(usage(account, SandboxTier::T2OsJail, 3_000))
        .await;
    ledger
        .record(usage(account, SandboxTier::T3MicroVm, 1_000))
        .await;

    // 100 × 3s + 500 × 1s.
    assert_eq!(pg::balance_micros(&pool, account.0).await.unwrap(), -800);
    let entries = pg::entries(&pool, account.0, 10).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.kind == "usage.sandbox"));
    // The source points at the call, so a line on an invoice can be traced to the tool call that
    // produced it.
    assert!(entries.iter().all(|e| e.source["call_id"].is_string()));
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_free_tier_execution_is_still_recorded() {
    // A ledger that omitted free work could not answer "what did this session do", which is the
    // question a dispute starts from.
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let ledger = SandboxLedger::new(pool.clone(), OnWriteFailure::FailClosed);

    ledger
        .record(usage(account, SandboxTier::T0InProcess, 5_000))
        .await;
    let entries = pg::entries(&pool, account.0, 10).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].amount_micros, 0);
    assert_eq!(entries[0].quantity["sandbox_ms"], 5_000);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn the_same_call_is_billed_once_across_a_resume() {
    // docs/13's resume may re-run a replay-safe call. The work is the same work, so the bill is the
    // same bill — the call id is the idempotency unit for exactly this.
    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let ledger = SandboxLedger::new(pool.clone(), OnWriteFailure::FailClosed);
    let once = usage(account, SandboxTier::T2OsJail, 2_000);

    ledger.record(once.clone()).await;
    ledger.record(once.clone()).await;
    assert_eq!(pg::balance_micros(&pool, account.0).await.unwrap(), -200);
    assert_eq!(pg::entries(&pool, account.0, 10).await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn model_and_sandbox_usage_share_one_balance() {
    // docs/17: one ledger, several kinds. The balance is a sum over one table, so an invoice does not
    // have to add up two systems that can disagree.
    use panday_gateway::{UsageRecord, UsageSink};
    use panday_platform::ledger::LedgerSink;
    use panday_types::model::{ModelRef, Usage};
    use panday_types::pricing::{PriceTable, Pricing};
    use std::sync::Arc;

    let pool = database().await;
    let account = AccountId(pg::create_account(&pool, "acme").await.unwrap());
    let prices = Arc::new(PriceTable::new().with(
        "anthropic/claude-sonnet-4-5",
        Pricing::anthropic_sonnet_class(),
    ));
    let model_sink = LedgerSink::new(pool.clone(), prices.clone(), OnWriteFailure::FailClosed);
    let sandbox_sink = SandboxLedger::new(pool.clone(), OnWriteFailure::FailClosed);

    model_sink
        .record(UsageRecord {
            account,
            request: panday_types::id::RequestId::new(),
            model: ModelRef("anthropic/claude-sonnet-4-5".into()),
            provider: "anthropic".into(),
            pool: "workhorse".into(),
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 100,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cache_write_1h_tokens: 0,
            },
        })
        .await;
    sandbox_sink
        .record(usage(account, SandboxTier::T2OsJail, 1_000))
        .await;

    let entries = pg::entries(&pool, account.0, 10).await.unwrap();
    let kinds: std::collections::BTreeSet<&str> = entries.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["usage.model", "usage.sandbox"].into_iter().collect()
    );
    let total: i64 = entries.iter().map(|e| e.amount_micros).sum();
    assert_eq!(pg::balance_micros(&pool, account.0).await.unwrap(), total);
}
