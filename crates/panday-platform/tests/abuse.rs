//! M17.7/M20.4 — the kill switch bites, and the audit trail says who did it.

use panday_platform::keys::{Environment, KeyError, Scope};
use panday_platform::{abuse, keys, pg};

async fn database() -> sqlx::PgPool {
    let url = pg::test_database_url()
        .expect("PANDAY_TEST_DATABASE_URL is unset — start deploy/integration-compose.yml first");
    let pool = pg::connect(&url).await.expect("connect");
    pg::migrate(&pool, &pg::migrations_dir())
        .await
        .expect("migrate");
    pool
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn suspending_an_account_refuses_its_keys_on_the_next_request() {
    // Immediate and with nothing to invalidate: the suspension is read in the same query as the
    // key, so there is no window where a killed account still works.
    let pool = database().await;
    let account = pg::create_account(&pool, "abuser").await.unwrap();
    let issued = keys::issue(
        &pool,
        account,
        "theirs",
        Environment::Live,
        &[Scope::Models],
    )
    .await
    .unwrap();
    assert!(keys::authenticate(&pool, &issued.plaintext).await.is_ok());

    abuse::suspend(&pool, account, "admin-key-1", "card testing")
        .await
        .unwrap();
    assert!(matches!(
        keys::authenticate(&pool, &issued.plaintext).await,
        Err(KeyError::Suspended)
    ));

    // Reversible: the key that was refused a moment ago works again, without re-issuing.
    abuse::unsuspend(&pool, account, "admin-key-1")
        .await
        .unwrap();
    assert!(keys::authenticate(&pool, &issued.plaintext).await.is_ok());
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_suspension_records_who_and_why_and_survives_the_reinstatement() {
    // "Who turned it off" and "who turned it back on" are the first two questions an incident
    // review asks, and an admin action that leaves no trace is indistinguishable from an intrusion.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();

    abuse::suspend(&pool, account, "admin-key-7", "chargeback")
        .await
        .unwrap();
    assert_eq!(
        abuse::suspension(&pool, account).await.unwrap().as_deref(),
        Some("chargeback")
    );

    abuse::unsuspend(&pool, account, "admin-key-9")
        .await
        .unwrap();
    assert_eq!(abuse::suspension(&pool, account).await.unwrap(), None);

    let history = abuse::history(&pool, account, 10).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].0, "unsuspend");
    assert_eq!(history[0].1, "admin-key-9");
    assert_eq!(history[1].0, "suspend");
    assert_eq!(history[1].2.as_deref(), Some("chargeback"));
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn suspension_is_not_deletion() {
    // A deleted account cannot be investigated and cannot be reinstated. The keys, the ledger and
    // the history all survive.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    keys::issue(&pool, account, "k", Environment::Live, &[Scope::Models])
        .await
        .unwrap();

    abuse::suspend(&pool, account, "admin", "fraud review")
        .await
        .unwrap();
    assert_eq!(keys::list(&pool, account).await.unwrap().len(), 1);
    assert!(pg::balance_micros(&pool, account).await.is_ok());
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn the_velocity_counters_read_what_actually_happened() {
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    for i in 0..3 {
        keys::issue(
            &pool,
            account,
            &format!("k{i}"),
            Environment::Live,
            &[Scope::Models],
        )
        .await
        .unwrap();
    }

    assert_eq!(abuse::keys_issued(&pool, account, 1).await.unwrap(), 3);
    // A window that ended before any of it happened sees none of it.
    assert!(abuse::accounts_created(&pool, 1).await.unwrap() >= 1);

    // Spend is positive-for-consumption, so a report reads the way a person expects.
    pg::append(
        &pool,
        &pg::LedgerEntry {
            id: uuid::Uuid::new_v4(),
            account_id: account,
            kind: "usage.model".into(),
            amount_micros: -2_500_000,
            quantity: serde_json::json!({}),
            source: serde_json::json!({}),
            idempotency_key: format!("abuse-test:{}", uuid::Uuid::new_v4()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        abuse::spend_micros(&pool, account, 24).await.unwrap(),
        2_500_000
    );
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn suspending_an_account_that_does_not_exist_is_an_error_not_a_silent_success() {
    let pool = database().await;
    assert!(abuse::suspend(&pool, uuid::Uuid::new_v4(), "admin", "typo")
        .await
        .is_err());
}
