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

// ── M17.1: the rest of the account model ─────────────────────────────────────

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn the_account_model_migration_applies_and_enforces_its_invariants() {
    let pool = database().await;

    // One active subscription per account, enforced by a partial unique index rather than by
    // application code: "two active plans" is a state nobody wrote a handler for.
    let account = pg::create_account(&pool, "acme").await.unwrap();
    sqlx::query("INSERT INTO plans (plan_id, name, entitlements) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind("pro")
        .bind("Pro")
        .bind(serde_json::json!([{"kind": "requests_per_min", "limit": 300}]))
        .execute(&pool)
        .await
        .expect("plan");

    let first = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO subscriptions (subscription_id, account_id, plan_id) VALUES ($1, $2, $3)",
    )
    .bind(first)
    .bind(account)
    .bind("pro")
    .execute(&pool)
    .await
    .expect("first subscription");

    let second = sqlx::query(
        "INSERT INTO subscriptions (subscription_id, account_id, plan_id) VALUES ($1, $2, $3)",
    )
    .bind(Uuid::now_v7())
    .bind(account)
    .bind("pro")
    .execute(&pool)
    .await;
    assert!(
        second.is_err(),
        "a second active subscription must be refused by the database"
    );

    // Ending the first frees the slot: a cancellation is an end date, so history survives.
    sqlx::query(
        "UPDATE subscriptions SET ended_at = now() WHERE subscription_id = $1 AND account_id = $2",
    )
    .bind(first)
    .bind(account)
    .execute(&pool)
    .await
    .expect("cancel");
    sqlx::query(
        "INSERT INTO subscriptions (subscription_id, account_id, plan_id) VALUES ($1, $2, $3)",
    )
    .bind(Uuid::now_v7())
    .bind(account)
    .bind("pro")
    .execute(&pool)
    .await
    .expect("a new subscription after cancellation");
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_api_key_is_stored_as_a_hash_and_revoked_by_timestamp() {
    // A stolen database must not be a stolen key: the plaintext exists once, in the response to
    // the create call. And revocation is a timestamp, because an audit that cannot show a key
    // *was* revoked cannot show when.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let plaintext = format!("pnd_live_{}", Uuid::now_v7().simple());
    let hash = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(plaintext.as_bytes());
        format!("{:x}", h.finalize())
    };

    let key_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO api_keys (key_id, account_id, key_hash, prefix, name)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(key_id)
    .bind(account)
    .bind(&hash)
    .bind("pnd_live_")
    .bind("ci")
    .execute(&pool)
    .await
    .expect("insert key");

    // The lookup is by hash and scoped by account, one index hit — the auth path runs on every
    // request and cannot afford a scan.
    let found: (Uuid,) = sqlx::query_as(
        "SELECT key_id FROM api_keys WHERE key_hash = $1 AND account_id = $2 AND revoked_at IS NULL",
    )
    .bind(&hash)
    .bind(account)
    .fetch_one(&pool)
    .await
    .expect("find key");
    assert_eq!(found.0, key_id);

    // The plaintext is nowhere in the row.
    let row: (String,) =
        sqlx::query_as("SELECT key_hash FROM api_keys WHERE key_id = $1 AND account_id = $2")
            .bind(key_id)
            .bind(account)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(row.0, plaintext);
    assert_eq!(row.0, hash);

    sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE key_id = $1 AND account_id = $2")
        .bind(key_id)
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();
    let after: Option<(Uuid,)> = sqlx::query_as(
        "SELECT key_id FROM api_keys WHERE key_hash = $1 AND account_id = $2 AND revoked_at IS NULL",
    )
    .bind(&hash)
    .bind(account)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(after.is_none(), "a revoked key must not resolve");
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_credit_grant_points_at_the_ledger_entry_it_produced() {
    // Grants live beside the ledger rather than inside it because a grant can expire while a
    // ledger entry never changes — but the *effect* is a ledger entry, so the balance stays a sum
    // over one table.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let tag = Uuid::now_v7();
    let ledger_id = Uuid::now_v7();

    pg::append(
        &pool,
        &pg::LedgerEntry {
            id: ledger_id,
            kind: "grant.purchase".into(),
            amount_micros: 5_000_000,
            quantity: serde_json::json!({"credits": 5}),
            idempotency_key: format!("purchase-{tag}"),
            ..entry(
                account,
                "grant.purchase",
                5_000_000,
                &format!("purchase-{tag}"),
            )
        },
    )
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO credit_grants (grant_id, account_id, reason, amount_micros, ledger_entry_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(account)
    .bind("purchase")
    .bind(5_000_000i64)
    .bind(ledger_id)
    .execute(&pool)
    .await
    .expect("grant");

    assert_eq!(pg::balance_micros(&pool, account).await.unwrap(), 5_000_000);

    // A grant of zero or less is refused by the CHECK: a "grant" that takes credit away is an
    // adjustment, and calling it a grant would make the two indistinguishable in a report.
    let bad = sqlx::query(
        "INSERT INTO credit_grants (grant_id, account_id, reason, amount_micros)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(account)
    .bind("promo")
    .bind(0i64)
    .execute(&pool)
    .await;
    assert!(bad.is_err(), "a non-positive grant must be refused");
}

// ── M17.2: the balance view ──────────────────────────────────────────────────

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn the_balance_view_equals_the_entries_it_summarises() {
    // The invariant. A cached total that can drift from its source is worse than a `SUM`, because it
    // is wrong quietly — so the suite checks both numbers and the whole-database drift query.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();

    let mut expected = 0i64;
    for i in 1..=25 {
        let amount = if i % 3 == 0 { 1_000 * i } else { -700 * i };
        expected += amount;
        pg::append(
            &pool,
            &pg::LedgerEntry {
                amount_micros: amount,
                idempotency_key: format!("view-{}-{}", Uuid::now_v7(), i),
                ..entry(account, "usage.model", amount, "unused")
            },
        )
        .await
        .expect("append");
    }

    assert_eq!(pg::balance_micros(&pool, account).await.unwrap(), expected);
    assert_eq!(
        pg::balance_from_entries(&pool, account).await.unwrap(),
        expected,
        "the view and the entries must agree"
    );
    // Scoped to this account, not the whole database: the lane's database is shared and long-lived,
    // another test deliberately tampers with its own balance, and a global assertion here would make
    // this test fail for someone else's reasons.
    let drift = pg::balance_drift(&pool).await.unwrap();
    assert!(
        !drift.iter().any(|(id, _, _)| *id == account),
        "this account drifted: {drift:?}"
    );
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_duplicate_write_moves_neither_the_entries_nor_the_balance() {
    // The transaction is what makes this free: the insert fails, everything rolls back, and the
    // balance never moved. A write path that updated the balance outside the transaction would
    // double-count exactly the retries idempotency exists to absorb.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let key = format!("dup-{}", Uuid::now_v7());

    pg::append(
        &pool,
        &pg::LedgerEntry {
            amount_micros: -5_000,
            idempotency_key: key.clone(),
            ..entry(account, "usage.model", -5_000, &key)
        },
    )
    .await
    .unwrap();

    for _ in 0..3 {
        assert!(matches!(
            pg::append(
                &pool,
                &pg::LedgerEntry {
                    amount_micros: -5_000,
                    idempotency_key: key.clone(),
                    ..entry(account, "usage.model", -5_000, &key)
                },
            )
            .await,
            Err(pg::PgError::Duplicate(_))
        ));
    }

    assert_eq!(pg::balance_micros(&pool, account).await.unwrap(), -5_000);
    assert_eq!(
        pg::balance_from_entries(&pool, account).await.unwrap(),
        -5_000
    );
    assert!(!pg::balance_drift(&pool)
        .await
        .unwrap()
        .iter()
        .any(|(id, _, _)| *id == account));
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_account_with_no_entries_reads_as_zero_rather_than_erroring() {
    // There is no balance row until the first entry, and "no row" has to read as zero — an
    // entitlement check that failed on a new account would refuse every first request.
    let pool = database().await;
    let account = pg::create_account(&pool, "fresh").await.unwrap();
    assert_eq!(pg::balance_micros(&pool, account).await.unwrap(), 0);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn the_drift_query_finds_a_balance_that_was_tampered_with() {
    // Without this, "no drift" and "the drift query is broken" look identical — and this check spends
    // its life in the first state, so the second would go unnoticed.
    let pool = database().await;
    let account = pg::create_account(&pool, "tampered").await.unwrap();
    let key = format!("tamper-{}", Uuid::now_v7());
    pg::append(
        &pool,
        &pg::LedgerEntry {
            amount_micros: -1_000,
            idempotency_key: key.clone(),
            ..entry(account, "usage.model", -1_000, &key)
        },
    )
    .await
    .unwrap();

    // Move the cached balance behind the ledger's back, the way a broken write path would.
    sqlx::query("UPDATE balances SET balance_micros = balance_micros - 999 WHERE account_id = $1")
        .bind(account)
        .execute(&pool)
        .await
        .unwrap();

    let drift = pg::balance_drift(&pool).await.unwrap();
    let found = drift.iter().find(|(id, _, _)| *id == account);
    assert!(found.is_some(), "the drift query missed a tampered balance");
    let (_, cached, computed) = found.unwrap();
    assert_eq!(*cached, -1_999);
    assert_eq!(*computed, -1_000);

    // And repair it, which is the other half of a drift check: a monitor that only reports leaves an
    // operator hand-writing UPDATEs against a money table at 3am.
    let repaired = pg::repair_balance(&pool, account).await.unwrap();
    assert_eq!(repaired, -1_000);
    assert_eq!(pg::balance_micros(&pool, account).await.unwrap(), -1_000);
    assert!(
        !pg::balance_drift(&pool)
            .await
            .unwrap()
            .iter()
            .any(|(id, _, _)| *id == account),
        "the repair did not clear the drift"
    );
}
