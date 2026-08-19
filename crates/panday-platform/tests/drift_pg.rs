//! M21.4 — reconciling recorded COGS against a provider's usage report, over real rows.

use panday_platform::{drift, pg};
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

fn usage_entry(account: Uuid, model: &str, provider_cost_micros: i64) -> pg::LedgerEntry {
    pg::LedgerEntry {
        id: Uuid::new_v4(),
        account_id: account,
        kind: "usage.model".into(),
        // What the customer was charged. Deliberately different from the provider cost below, so
        // the test would catch a reconciliation that compared price to cost — the margin between
        // them looks exactly like drift.
        amount_micros: -(provider_cost_micros * 2),
        quantity: serde_json::json!({
            "model": model,
            "provider_cost_micros": provider_cost_micros,
            "input_tokens": 1000,
            "output_tokens": 100,
        }),
        source: serde_json::json!({ "request_id": Uuid::new_v4() }),
        idempotency_key: format!("drift-test:{}", Uuid::new_v4()),
    }
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn recorded_cogs_sums_provider_cost_not_what_the_customer_paid() {
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let model = format!("test/{}", Uuid::new_v4().simple());

    for cost in [1_000_000i64, 500_000, 250_000] {
        pg::append(&pool, &usage_entry(account, &model, cost))
            .await
            .unwrap();
    }

    let from = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    let to = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let cogs = drift::recorded_cogs(&pool, from, to).await.unwrap();

    assert_eq!(
        cogs.get(&model).copied(),
        Some(1_750_000),
        "COGS is what we paid the provider, not what we charged for it"
    );
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_period_outside_the_window_is_not_counted() {
    // A monthly reconciliation that quietly included last month would report drift every time.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let model = format!("test/{}", Uuid::new_v4().simple());
    pg::append(&pool, &usage_entry(account, &model, 1_000_000))
        .await
        .unwrap();

    let long_ago = time::OffsetDateTime::now_utc() - time::Duration::days(400);
    let cogs = drift::recorded_cogs(&pool, long_ago, long_ago + time::Duration::days(1))
        .await
        .unwrap();
    assert!(!cogs.contains_key(&model));
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_matching_invoice_reconciles_end_to_end() {
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let model = format!("test/{}", Uuid::new_v4().simple());
    pg::append(&pool, &usage_entry(account, &model, 3_000_000))
        .await
        .unwrap();

    let from = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    let to = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let ours = drift::recorded_cogs(&pool, from, to).await.unwrap();

    // Their invoice, in the normalised shape, one cent off — inside tolerance.
    let csv = format!("model,cost_usd,input_tokens,output_tokens\n{model},3.01,1000,100\n");
    let theirs = drift::parse_report(&csv).unwrap();

    let report = drift::compare(&ours, &theirs, 100, 10_000);
    let row = report
        .rows
        .iter()
        .find(|r| r.model == model)
        .expect("our model");
    assert_eq!(row.class, drift::Class::Ok, "{row:?}");
}
