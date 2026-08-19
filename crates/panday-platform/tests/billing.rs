//! M17.4/M17.5 — the webhook inbox, the plan projection, and the meter export.
//!
//! No Stripe. Everything asserted here is a property of *our* side, which is the point: docs/17
//! makes Stripe a projection, and a billing pipeline whose correctness can only be checked by
//! charging somebody is one nobody checks.

use panday_platform::billing::{self, BillingEvent, RecordingMeter, TOKENS_METER};
use panday_platform::pg;
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

/// A plan row to point subscriptions at.
async fn plan(pool: &sqlx::PgPool, id: &str) {
    sqlx::query(
        "INSERT INTO plans (plan_id, name, entitlements) VALUES ($1, $2, '{}'::jsonb)
         ON CONFLICT (plan_id) DO NOTHING",
    )
    .bind(id)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

fn event(e: &BillingEvent) -> serde_json::Value {
    serde_json::to_value(e).unwrap()
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_redelivered_webhook_is_stored_once() {
    // "Never trust webhook delivery" means assuming every event arrives more than once. The
    // primary key decides, not a code path.
    let pool = database().await;
    let id = format!("evt_{}", Uuid::new_v4().simple());
    let payload = serde_json::json!({"type": "invoice.paid", "customer": "x", "credit_micros": 1});

    assert!(billing::receive(&pool, &id, "invoice.paid", &payload)
        .await
        .unwrap());
    assert!(
        !billing::receive(&pool, &id, "invoice.paid", &payload)
            .await
            .unwrap(),
        "the second delivery is not new"
    );
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn checkout_puts_an_account_on_a_plan() {
    let pool = database().await;
    let name = format!("acme-{}", Uuid::new_v4().simple());
    let account = pg::create_account(&pool, &name).await.unwrap();
    plan(&pool, "pro").await;

    let id = format!("evt_{}", Uuid::new_v4().simple());
    billing::receive(
        &pool,
        &id,
        "checkout.session.completed",
        &event(&BillingEvent::CheckoutCompleted {
            customer: name.clone(),
            plan_id: "pro".into(),
        }),
    )
    .await
    .unwrap();

    let report = billing::apply_pending(&pool, 50).await.unwrap();
    assert!(report.applied >= 1);
    assert_eq!(
        billing::current_plan(&pool, account)
            .await
            .unwrap()
            .as_deref(),
        Some("pro")
    );
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_plan_change_leaves_exactly_one_active_subscription() {
    // "Two active plans" is a state nobody wrote a handler for, and the partial unique index simply
    // refuses it — so the transition has to end the old row in the same transaction.
    let pool = database().await;
    let name = format!("acme-{}", Uuid::new_v4().simple());
    let account = pg::create_account(&pool, &name).await.unwrap();
    plan(&pool, "pro").await;
    plan(&pool, "max").await;

    billing::set_plan(&pool, account, Some("pro"))
        .await
        .unwrap();
    billing::set_plan(&pool, account, Some("max"))
        .await
        .unwrap();
    assert_eq!(
        billing::current_plan(&pool, account)
            .await
            .unwrap()
            .as_deref(),
        Some("max")
    );

    // Cancelling leaves history rather than deleting it.
    billing::set_plan(&pool, account, None).await.unwrap();
    assert_eq!(billing::current_plan(&pool, account).await.unwrap(), None);
    let (rows,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM subscriptions WHERE account_id = $1")
            .bind(account)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 2, "both subscriptions survive as history");
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_paid_invoice_credits_once_even_if_applied_twice() {
    // The grant's effect is a ledger entry keyed by the Stripe event id, so a replay of the whole
    // inbox — which is a thing an operator will do — cannot double-credit.
    let pool = database().await;
    let name = format!("acme-{}", Uuid::new_v4().simple());
    let account = pg::create_account(&pool, &name).await.unwrap();
    let before = pg::balance_micros(&pool, account).await.unwrap();

    let id = format!("evt_{}", Uuid::new_v4().simple());
    billing::receive(
        &pool,
        &id,
        "invoice.paid",
        &event(&BillingEvent::InvoicePaid {
            customer: name.clone(),
            credit_micros: 20_000_000,
        }),
    )
    .await
    .unwrap();
    billing::apply_pending(&pool, 50).await.unwrap();

    // Force a second application of the same event, as a replay would.
    sqlx::query("UPDATE billing_events SET applied_at = NULL WHERE event_id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    billing::apply_pending(&pool, 50).await.unwrap();

    assert_eq!(
        pg::balance_micros(&pool, account).await.unwrap(),
        before + 20_000_000,
        "credited exactly once"
    );
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_event_for_an_unknown_customer_stays_in_the_queue_with_its_reason() {
    // Usually a test-mode webhook hitting a live database. Dropping it to keep the queue moving is
    // how a customer ends up on the wrong plan for a month.
    let pool = database().await;
    let id = format!("evt_{}", Uuid::new_v4().simple());
    billing::receive(
        &pool,
        &id,
        "invoice.paid",
        &event(&BillingEvent::InvoicePaid {
            customer: "nobody-by-that-name".into(),
            credit_micros: 1_000,
        }),
    )
    .await
    .unwrap();

    // Counted, not asserted exactly: the integration database is shared, so other tests' stuck
    // events are in this queue too — which is itself the condition the ordering change protects.
    let report = billing::apply_pending(&pool, 100).await.unwrap();
    assert!(report.unknown_customer >= 1);

    let stuck = billing::stuck(&pool, 100).await.unwrap();
    let mine = stuck
        .iter()
        .find(|(e, _, _)| *e == id)
        .expect("still queued");
    assert!(mine.1.contains("nobody-by-that-name"), "{:?}", mine);
    assert!(mine.2 >= 1, "the attempt is counted");
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_malformed_event_does_not_stop_the_queue() {
    let pool = database().await;
    let name = format!("acme-{}", Uuid::new_v4().simple());
    let account = pg::create_account(&pool, &name).await.unwrap();
    plan(&pool, "pro").await;

    let bad = format!("evt_{}", Uuid::new_v4().simple());
    billing::receive(
        &pool,
        &bad,
        "nonsense",
        &serde_json::json!({"type": "who.knows"}),
    )
    .await
    .unwrap();
    let good = format!("evt_{}", Uuid::new_v4().simple());
    billing::receive(
        &pool,
        &good,
        "checkout.session.completed",
        &event(&BillingEvent::CheckoutCompleted {
            customer: name.clone(),
            plan_id: "pro".into(),
        }),
    )
    .await
    .unwrap();

    let report = billing::apply_pending(&pool, 50).await.unwrap();
    assert!(report.failed >= 1);
    assert!(report.applied >= 1, "the good event still went through");
    assert_eq!(
        billing::current_plan(&pool, account)
            .await
            .unwrap()
            .as_deref(),
        Some("pro")
    );
}

// ── M17.5 ─────────────────────────────────────────────────────────────────────

async fn usage(pool: &sqlx::PgPool, account: Uuid, at: time::OffsetDateTime, tokens: i64) {
    sqlx::query(
        "INSERT INTO ledger_entries (id, account_id, at, kind, amount_micros, quantity, source, idempotency_key)
         VALUES ($1, $2, $3, 'usage.model', -1000, $4, '{}'::jsonb, $5)",
    )
    .bind(Uuid::new_v4())
    .bind(account)
    .bind(at)
    .bind(serde_json::json!({"input_tokens": tokens, "output_tokens": 0, "model": "m"}))
    .bind(format!("meter-test:{}", Uuid::new_v4()))
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_hour_exports_once_and_only_once() {
    // Stripe aggregates asynchronously and cannot deduplicate for us (ADR-009), so the cursor is
    // ours and re-running an hour must be a collision rather than a second charge.
    let pool = database().await;
    let name = format!("acme-{}", Uuid::new_v4().simple());
    let account = pg::create_account(&pool, &name).await.unwrap();
    let hour = time::OffsetDateTime::now_utc() - time::Duration::hours(3);
    usage(&pool, account, hour, 1_500).await;

    // Asserted on *this* account's meter events: the export walks every account in the hour, and
    // the integration database is shared with every other test in this file.
    let mine = |meter: &RecordingMeter| -> Vec<(String, String, u64)> {
        meter
            .sent()
            .into_iter()
            .filter(|(c, _, _)| *c == name)
            .collect()
    };

    let meter = RecordingMeter::new();
    billing::export_hour(&pool, &meter, hour).await.unwrap();
    assert_eq!(
        mine(&meter),
        vec![(name.clone(), TOKENS_METER.to_string(), 1_500)]
    );

    billing::export_hour(&pool, &meter, hour).await.unwrap();
    assert_eq!(mine(&meter).len(), 1, "nothing was sent twice");
}

/// Fails every send.
struct BrokenMeter;

#[async_trait::async_trait]
impl billing::MeterSink for BrokenMeter {
    async fn send(
        &self,
        _customer: &str,
        _meter: &str,
        _value: u64,
        _hour: time::OffsetDateTime,
    ) -> Result<(), String> {
        Err("stripe is down".into())
    }
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_failed_send_does_not_advance_the_cursor() {
    // A cursor that advances on a failed send silently drops an hour of somebody's usage, and
    // nothing downstream ever notices — the invoice is simply smaller.
    let pool = database().await;
    let name = format!("acme-{}", Uuid::new_v4().simple());
    let account = pg::create_account(&pool, &name).await.unwrap();
    let hour = time::OffsetDateTime::now_utc() - time::Duration::hours(4);
    usage(&pool, account, hour, 900).await;

    let broken = billing::export_hour(&pool, &BrokenMeter, hour)
        .await
        .unwrap();
    assert!(broken.failed >= 1, "the failure is counted, not swallowed");
    assert_eq!(broken.events_sent, 0);

    // The retry sends the hour that was never sent — the cursor did not advance past it.
    let meter = RecordingMeter::new();
    billing::export_hour(&pool, &meter, hour).await.unwrap();
    assert!(
        meter.sent().iter().any(|(c, _, v)| *c == name && *v == 900),
        "the released hour was retried: {:?}",
        meter.sent()
    );
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn what_we_reported_matches_the_ledger_to_the_token() {
    // docs/17 M17.5's "invoice sanity check vs ledger to the cent", on a seeded period. Checked
    // against what we *reported*, because that is the number the invoice is computed from.
    let pool = database().await;
    let name = format!("acme-{}", Uuid::new_v4().simple());
    let account = pg::create_account(&pool, &name).await.unwrap();

    let base = time::OffsetDateTime::now_utc() - time::Duration::hours(10);
    for h in 0..3 {
        usage(
            &pool,
            account,
            base + time::Duration::hours(h),
            1_000 * (h + 1),
        )
        .await;
    }

    let meter = RecordingMeter::new();
    for h in 0..3 {
        billing::export_hour(&pool, &meter, base + time::Duration::hours(h))
            .await
            .unwrap();
    }

    let check = billing::check_invoice(
        &pool,
        account,
        base - time::Duration::hours(1),
        base + time::Duration::hours(4),
    )
    .await
    .unwrap();
    assert!(check.agrees(), "{check:?}");
    assert_eq!(check.ledger_tokens, 6_000);
    assert_eq!(check.difference(), 0);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_unexported_hour_shows_up_as_a_difference() {
    // The check has to be able to fail, or it is decoration.
    let pool = database().await;
    let name = format!("acme-{}", Uuid::new_v4().simple());
    let account = pg::create_account(&pool, &name).await.unwrap();
    let base = time::OffsetDateTime::now_utc() - time::Duration::hours(20);
    usage(&pool, account, base, 5_000).await;

    let check = billing::check_invoice(
        &pool,
        account,
        base - time::Duration::hours(1),
        base + time::Duration::hours(2),
    )
    .await
    .unwrap();
    assert!(!check.agrees());
    assert_eq!(check.difference(), -5_000, "we under-reported, not over");
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_wall_of_broken_events_does_not_starve_a_new_one() {
    // The batch limit is finite and the queue is ordered. Ordering by age alone means 50 events
    // nobody can apply block a paying customer's checkout indefinitely — so the order is fewest
    // attempts first.
    let pool = database().await;
    let name = format!("acme-{}", Uuid::new_v4().simple());
    let account = pg::create_account(&pool, &name).await.unwrap();
    plan(&pool, "pro").await;

    // Twenty events that can never apply, all older than the good one.
    for _ in 0..20 {
        let id = format!("evt_{}", Uuid::new_v4().simple());
        billing::receive(
            &pool,
            &id,
            "invoice.paid",
            &event(&BillingEvent::InvoicePaid {
                customer: format!("ghost-{}", Uuid::new_v4().simple()),
                credit_micros: 1,
            }),
        )
        .await
        .unwrap();
    }
    // Give them all a failed attempt.
    billing::apply_pending(&pool, 200).await.unwrap();

    let good = format!("evt_{}", Uuid::new_v4().simple());
    billing::receive(
        &pool,
        &good,
        "checkout.session.completed",
        &event(&BillingEvent::CheckoutCompleted {
            customer: name.clone(),
            plan_id: "pro".into(),
        }),
    )
    .await
    .unwrap();

    // A small batch — smaller than the backlog — still reaches the new event.
    billing::apply_pending(&pool, 5).await.unwrap();
    assert_eq!(
        billing::current_plan(&pool, account)
            .await
            .unwrap()
            .as_deref(),
        Some("pro"),
        "the newest event was starved behind the backlog"
    );
}

#[test]
fn the_webhook_secret_is_compared_in_constant_time_and_rejects_the_obvious() {
    use panday_platform::billing::http::verify;

    assert!(verify("s3cret", "s3cret"));
    assert!(!verify("s3cret", "s3cre"), "a prefix is not the secret");
    assert!(!verify("s3cret", "s3crett"));
    assert!(!verify("s3cret", ""));
    // An unset secret must never accept anything, or a misconfigured deployment is an open write
    // into the billing inbox.
    assert!(!verify("", ""));
    assert!(!verify("", "anything"));
}
