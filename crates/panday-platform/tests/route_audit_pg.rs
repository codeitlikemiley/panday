//! M12.2 — routing decisions in Postgres: the row, the operator's aggregate, and retention.

use panday_gateway::{RouteAudit, RouteRecord};
use panday_platform::{pg, routes};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::TaskClass;

async fn database() -> sqlx::PgPool {
    let url = pg::test_database_url()
        .expect("PANDAY_TEST_DATABASE_URL is unset — start deploy/integration-compose.yml first");
    let pool = pg::connect(&url).await.expect("connect");
    pg::migrate(&pool, &pg::migrations_dir())
        .await
        .expect("migrate");
    pool
}

fn record(account: uuid::Uuid, rule: &str, chosen: Option<&str>, attempts: u32) -> RouteRecord {
    RouteRecord {
        account: AccountId(account),
        request: RequestId::new(),
        requested: "auto".into(),
        task: TaskClass::Code,
        matched_rule: rule.into(),
        pool: "frontier".into(),
        chain: vec![
            "anthropic/claude-opus-4-1".into(),
            "anthropic/claude-sonnet-4-5".into(),
        ],
        chosen: chosen.map(str::to_string),
        attempts,
        confidence: 0.82,
        trusted: true,
    }
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_decision_round_trips() {
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let row = record(account, "rules[3]", Some("anthropic/claude-opus-4-1"), 1);
    routes::insert(&pool, &row).await.unwrap();

    let back = routes::recent(&pool, account, 10).await.unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].request_id, row.request.0);
    assert_eq!(back[0].matched_rule, "rules[3]");
    assert_eq!(back[0].task, "code");
    // The chain survives as a chain, in order. Storing it as text would make "which models does
    // this rule actually reach" a `LIKE` query.
    assert_eq!(back[0].chain, row.chain);
    assert_eq!(back[0].chosen.as_deref(), Some("anthropic/claude-opus-4-1"));
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_replayed_request_id_does_not_error() {
    // Same request written twice — a retry, a redelivery, a backfill overlapping live traffic. The
    // audit path must never be the thing that fails a request, so this is a no-op, not a conflict.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let row = record(account, "rules[0]", Some("local/qwen3.5-4b"), 1);
    routes::insert(&pool, &row).await.unwrap();
    routes::insert(&pool, &row).await.unwrap();
    assert_eq!(routes::recent(&pool, account, 10).await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn one_account_never_sees_another_accounts_decisions() {
    let pool = database().await;
    let mine = pg::create_account(&pool, "mine").await.unwrap();
    let theirs = pg::create_account(&pool, "theirs").await.unwrap();
    routes::insert(&pool, &record(theirs, "rules[0]", None, 2))
        .await
        .unwrap();
    assert!(routes::recent(&pool, mine, 10).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn rule_health_counts_failovers_and_dead_chains_separately() {
    // The two failures are different problems: one costs latency, the other costs the request.
    // Collapsing them into "errors" is how a slow provider hides behind a working one.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let rule = format!("rules[{}]", uuid::Uuid::new_v4().simple());

    routes::insert(&pool, &record(account, &rule, Some("a/b"), 1))
        .await
        .unwrap();
    routes::insert(&pool, &record(account, &rule, Some("a/c"), 2))
        .await
        .unwrap();
    routes::insert(&pool, &record(account, &rule, None, 2))
        .await
        .unwrap();

    let since = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    let health = routes::rule_health(&pool, since).await.unwrap();
    let mine = health.iter().find(|h| h.rule == rule).expect("the rule");
    assert_eq!(mine.total, 3);
    assert_eq!(mine.failed_over, 2, "two requests took more than one leg");
    assert_eq!(mine.unserved, 1, "one was never served at all");
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn retention_deletes_only_what_is_old() {
    // An audit table with no retention is a disk-full incident with a scheduled date.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    routes::insert(&pool, &record(account, "rules[0]", Some("a/b"), 1))
        .await
        .unwrap();

    // Nothing here is 400 days old, so a 400-day retention must not touch it — including rows other
    // tests just wrote.
    routes::prune(&pool, 400).await.unwrap();
    assert_eq!(routes::recent(&pool, account, 10).await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn the_gateway_sink_writes_without_making_the_caller_wait() {
    // `PgRouteAudit::record` returns before the row exists — that is the point, and the reason the
    // test polls instead of asserting immediately. What must hold is that the row lands.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let sink = routes::PgRouteAudit::new(pool.clone());
    let row = record(account, "rules[1]", Some("a/b"), 1);
    sink.record(row.clone()).await;

    for _ in 0..50 {
        if !routes::recent(&pool, account, 10).await.unwrap().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the audit row never landed");
}

#[tokio::test]
#[ignore = "needs the integration lane (deploy/integration-compose.yml)"]
async fn what_the_classifier_said_survives_the_round_trip() {
    // The point of 0011. `task` alone conflates "the heuristic was certain" with "it guessed
    // something at 0.2 and the gate fell back", and a learned router (M19.3) is trained on exactly
    // that distinction. Both were discarded before this, so the evidence a future model needs was
    // being thrown away one request at a time.
    let pool = database().await;
    let account = pg::create_account(&pool, &format!("acme-{}", uuid::Uuid::now_v7()))
        .await
        .expect("account");

    let mut untrusted = record(account, "rules[0]", Some("anthropic/claude-sonnet-4-5"), 1);
    untrusted.confidence = 0.21;
    untrusted.trusted = false;
    let id = untrusted.request.0;

    // `routes::insert`, not `PgRouteAudit::record`: the trait impl detaches the write onto a
    // background task so inference never waits on the database, so a test that read straight after
    // it would race the spawn — and did, once, with `RowNotFound`. What is under test here is the
    // column round trip, not the detachment.
    routes::insert(&pool, &untrusted).await.expect("insert");

    let (confidence, trusted): (Option<f32>, Option<bool>) =
        sqlx::query_as("SELECT confidence, trusted FROM route_decisions WHERE request_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("the row");

    assert_eq!(
        trusted,
        Some(false),
        "the gate fired and the row must say so"
    );
    let confidence = confidence.expect("a confidence was recorded");
    assert!(
        (confidence - 0.21).abs() < 1e-6,
        "confidence must survive as written, not rounded to a bucket: {confidence}"
    );
}
