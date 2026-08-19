//! The integration lane (docs/02 M2.3): a real Postgres, the real schema, real queries.
//!
//! Every test here is `#[ignore]`d, because docs/02's unit lane is "no network, no docker" and
//! must stay runnable on a laptop with neither. To run them:
//!
//! ```text
//! docker compose -f deploy/integration-compose.yml up -d
//! PANDAY_TEST_DATABASE_URL=postgres://panday:panday@127.0.0.1:5433/panday_test \
//!   cargo test -p panday-platform --test pg_integration -- --ignored
//! ```
//!
//! Each test owns its own account, so they can run concurrently against one database without a
//! reset step — which is also the tenant-scoping property under test: if a query leaked across
//! accounts, these tests would interfere with each other and say so.

use panday_platform::pg;
use uuid::Uuid;

/// Connect and migrate, or explain why not.
///
/// A missing URL is a hard failure *inside* an ignored test: someone who typed `--ignored` asked
/// for the integration lane, and silently passing would tell them the database is fine when
/// there is no database.
async fn database() -> sqlx::PgPool {
    let url = pg::test_database_url()
        .expect("PANDAY_TEST_DATABASE_URL is unset — start deploy/integration-compose.yml first");
    let pool = pg::connect(&url).await.expect("connect");
    let applied = pg::migrate(&pool, &pg::migrations_dir())
        .await
        .expect("migrate");
    assert!(!applied.is_empty(), "no migrations were found");
    pool
}

fn entry(account: Uuid, kind: &str, micros: i64, key: &str) -> pg::LedgerEntry {
    pg::LedgerEntry {
        id: Uuid::now_v7(),
        account_id: account,
        kind: kind.to_string(),
        amount_micros: micros,
        quantity: serde_json::json!({"input_tokens": 1000, "output_tokens": 50, "model": "anthropic/claude-sonnet-4-5"}),
        source: serde_json::json!({"session_id": Uuid::now_v7(), "seq": 7}),
        idempotency_key: key.to_string(),
    }
}

#[tokio::test]
#[ignore = "needs the integration lane (deploy/integration-compose.yml)"]
async fn the_schema_applies_and_a_scoped_query_runs() {
    // docs/02 M2.3's literal acceptance: "first sqlx query compiles against a real schema".
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.expect("account");

    // A unique key per run: the integration database outlives a single `cargo test`, so a fixed
    // idempotency key is a test that passes exactly once — which is what the first draft did.
    let key = format!("req-{}", Uuid::now_v7());
    pg::append(&pool, &entry(account, "usage.model", -2_340, &key))
        .await
        .expect("append");

    assert_eq!(pg::balance_micros(&pool, account).await.unwrap(), -2_340);
    let entries = pg::entries(&pool, account, 10).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, "usage.model");
    // JSONB survives the round trip, which is why the ledger can store per-kind shapes without
    // a column per kind.
    assert_eq!(entries[0].quantity["input_tokens"], 1000);
    assert_eq!(entries[0].source["seq"], 7);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn migrations_are_idempotent() {
    // Applied on every boot of every service; a second run must be a no-op rather than an error,
    // or a rolling deploy fails on the second pod.
    let pool = database().await;
    let again = pg::migrate(&pool, &pg::migrations_dir())
        .await
        .expect("re-apply");
    assert!(!again.is_empty());
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_retried_request_cannot_double_bill() {
    // docs/17: "idempotency_key TEXT UNIQUE NOT NULL -- request_id-scoped; retries can't
    // double-bill". The UNIQUE constraint is the enforcement and the code does not get a vote.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let key = format!("req-{}", Uuid::now_v7());

    pg::append(&pool, &entry(account, "usage.model", -1_000, &key))
        .await
        .expect("first append");
    let err = pg::append(&pool, &entry(account, "usage.model", -1_000, &key))
        .await
        .expect_err("the retry must not land");
    assert!(matches!(err, pg::PgError::Duplicate(_)), "{err:?}");

    // Billed once.
    assert_eq!(pg::balance_micros(&pool, account).await.unwrap(), -1_000);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn one_accounts_ledger_is_invisible_to_another() {
    // docs/20 T5, against a real database rather than a lint. Two accounts, one table.
    let pool = database().await;
    let mine = pg::create_account(&pool, "mine").await.unwrap();
    let theirs = pg::create_account(&pool, "theirs").await.unwrap();

    pg::append(
        &pool,
        &entry(
            mine,
            "usage.model",
            -500,
            &format!("mine-{}", Uuid::now_v7()),
        ),
    )
    .await
    .unwrap();
    pg::append(
        &pool,
        &entry(
            theirs,
            "usage.model",
            -9_000,
            &format!("theirs-{}", Uuid::now_v7()),
        ),
    )
    .await
    .unwrap();

    assert_eq!(pg::balance_micros(&pool, mine).await.unwrap(), -500);
    assert_eq!(pg::balance_micros(&pool, theirs).await.unwrap(), -9_000);
    assert_eq!(pg::entries(&pool, mine, 100).await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_grant_and_its_consumption_net_out() {
    // The ledger is double-entry-flavoured (ADR-009): a plan grant is positive, usage is
    // negative, and the balance is the sum. No separate balance to keep in step.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let tag = Uuid::now_v7();

    pg::append(
        &pool,
        &pg::LedgerEntry {
            kind: "grant.plan".into(),
            amount_micros: 20_000_000,
            quantity: serde_json::json!({"plan": "pro"}),
            idempotency_key: format!("grant-{tag}"),
            ..entry(account, "grant.plan", 20_000_000, &format!("grant-{tag}"))
        },
    )
    .await
    .unwrap();
    pg::append(
        &pool,
        &entry(account, "usage.model", -2_340, &format!("usage-{tag}")),
    )
    .await
    .unwrap();

    assert_eq!(
        pg::balance_micros(&pool, account).await.unwrap(),
        20_000_000 - 2_340
    );
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_entry_for_an_unknown_account_is_refused() {
    // The foreign key. A ledger entry with no account is money attributed to nobody, which is
    // exactly the row that turns up in a reconciliation and cannot be explained.
    let pool = database().await;
    let ghost = Uuid::now_v7();
    let err = pg::append(
        &pool,
        &entry(
            ghost,
            "usage.model",
            -1,
            &format!("ghost-{}", Uuid::now_v7()),
        ),
    )
    .await
    .expect_err("must be refused");
    assert!(matches!(err, pg::PgError::Query(_)), "{err:?}");
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_dead_database_fails_fast_rather_than_hanging() {
    // An operator debugging a hang looks at the wrong thing for an hour.
    let started = std::time::Instant::now();
    let err = pg::connect("postgres://panday:panday@127.0.0.1:1/panday_test")
        .await
        .expect_err("nothing is listening on port 1");
    assert!(matches!(err, pg::PgError::Connect(_)), "{err:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
}
