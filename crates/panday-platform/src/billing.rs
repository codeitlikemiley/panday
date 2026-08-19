//! Stripe, as a projection (docs/17 §Stripe integration, M17.4/M17.5).
//!
//! **Nothing here calls Stripe.** The transport is a trait with an in-memory implementation, and
//! the reason is not testing convenience: docs/17 is explicit that "Meters are *reporting*;
//! enforcement already happened at the edge — Stripe's async aggregation can't do real-time stops
//! (ADR-009)". A billing integration that Stripe can break is a billing integration that decides
//! whether customers can work. Everything that matters — plan state, grants, the ledger — lives in
//! our database, and Stripe is a system we *tell*, plus a system that tells us about payments.
//!
//! The three pieces:
//!
//! - **An inbox.** Webhooks are received into a table and applied from it, as separate steps. A
//!   handler that applies an effect and then returns 200 has three ways to be wrong — effect twice,
//!   200 lost, events out of order — and all three go away when receiving and applying are
//!   separate over a durable row keyed by Stripe's own event id.
//! - **A projection.** Applying an event moves plan state and writes grants as ledger entries.
//!   Idempotent by idempotency key, because "never trust webhook delivery" means assuming every
//!   event arrives more than once.
//! - **An export.** Hourly usage aggregates go to Billing Meters with our own cursor table, because
//!   Stripe's aggregation is asynchronous and cannot deduplicate for us.

use crate::pg::{LedgerEntry, PgError};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum BillingError {
    #[error(transparent)]
    Db(#[from] PgError),
    #[error("event {id}: {detail}")]
    Malformed { id: String, detail: String },
    #[error("no account matches stripe customer `{0}`")]
    UnknownCustomer(String),
    #[error("meter export: {0}")]
    Export(String),
}

/// The events docs/17 names, plus the shape we need from each.
///
/// A closed enum rather than a map of handlers: an event kind nobody wrote a handler for should be
/// visibly ignored, not silently dropped into a generic path that half-works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BillingEvent {
    /// A customer completed checkout: they have a plan now.
    #[serde(rename = "checkout.session.completed")]
    CheckoutCompleted { customer: String, plan_id: String },
    /// An invoice was paid: the plan's credits refresh.
    #[serde(rename = "invoice.paid")]
    InvoicePaid {
        customer: String,
        /// In micro-dollars, converted at the edge like every other money value here.
        credit_micros: i64,
    },
    /// The subscription changed plan, or ended.
    #[serde(rename = "customer.subscription.updated")]
    SubscriptionUpdated {
        customer: String,
        /// `None` = cancelled.
        plan_id: Option<String>,
    },
}

impl BillingEvent {
    pub fn customer(&self) -> &str {
        match self {
            BillingEvent::CheckoutCompleted { customer, .. }
            | BillingEvent::InvoicePaid { customer, .. }
            | BillingEvent::SubscriptionUpdated { customer, .. } => customer,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            BillingEvent::CheckoutCompleted { .. } => "checkout.session.completed",
            BillingEvent::InvoicePaid { .. } => "invoice.paid",
            BillingEvent::SubscriptionUpdated { .. } => "customer.subscription.updated",
        }
    }
}

/// Receive a webhook. Stores it and returns immediately — applying is a separate step.
///
/// A redelivery is a no-op, decided by the primary key rather than by any code path. Returns
/// whether this was the first time we saw it, because "we already had this" is useful to log and
/// meaningless to alarm on.
pub async fn receive(
    pool: &PgPool,
    event_id: &str,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<bool, BillingError> {
    let done = sqlx::query(
        "INSERT INTO billing_events (event_id, kind, payload) VALUES ($1, $2, $3)
         ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(event_id)
    .bind(kind)
    .bind(payload)
    .execute(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(done.rows_affected() == 1)
}

/// What one pass of the applier did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub applied: u32,
    pub failed: u32,
    /// Events whose customer we do not recognise. Their own number because it is usually a
    /// configuration problem — a test-mode webhook hitting a live database — rather than a bug.
    pub unknown_customer: u32,
}

/// Apply everything pending, oldest first.
///
/// Failures stay in the table with their reason and an attempt count. Nothing is deleted and
/// nothing is skipped forward: an event that cannot be applied is a thing a person has to look at,
/// and dropping it to keep the queue moving is how a customer ends up on the wrong plan for a
/// month.
pub async fn apply_pending(pool: &PgPool, limit: i64) -> Result<ApplyReport, BillingError> {
    let rows: Vec<(String, serde_json::Value, i32)> = sqlx::query_as(
        "-- tenant-scoping: cross-tenant — the webhook queue is not yet attributed to an account;
         -- attributing it is what applying does.
         SELECT event_id, payload, attempts FROM billing_events
         -- Fewest attempts first, then oldest. Ordering by age alone lets a wall of permanently
         -- broken events at the head of the queue starve every new one — the batch limit is 50, and
         -- 50 events nobody can apply would block a paying customer's checkout indefinitely.
         WHERE applied_at IS NULL ORDER BY attempts, received_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    let mut report = ApplyReport::default();
    for (event_id, payload, _attempts) in rows {
        match apply_one(pool, &event_id, &payload).await {
            Ok(account) => {
                sqlx::query(
                    "UPDATE billing_events SET applied_at = now(), error = NULL, account_id = $2
                     WHERE event_id = $1",
                )
                .bind(&event_id)
                .bind(account)
                .execute(pool)
                .await
                .map_err(|e| PgError::Query(e.to_string()))?;
                report.applied += 1;
            }
            Err(e) => {
                if matches!(e, BillingError::UnknownCustomer(_)) {
                    report.unknown_customer += 1;
                } else {
                    report.failed += 1;
                }
                sqlx::query(
                    "UPDATE billing_events SET error = $2, attempts = attempts + 1
                     WHERE event_id = $1",
                )
                .bind(&event_id)
                .bind(e.to_string())
                .execute(pool)
                .await
                .map_err(|e| PgError::Query(e.to_string()))?;
            }
        }
    }
    Ok(report)
}

async fn apply_one(
    pool: &PgPool,
    event_id: &str,
    payload: &serde_json::Value,
) -> Result<Uuid, BillingError> {
    let event: BillingEvent =
        serde_json::from_value(payload.clone()).map_err(|e| BillingError::Malformed {
            id: event_id.to_string(),
            detail: e.to_string(),
        })?;

    let account = account_for_customer(pool, event.customer()).await?;

    match &event {
        BillingEvent::CheckoutCompleted { plan_id, .. } => {
            set_plan(pool, account, Some(plan_id)).await?;
        }
        BillingEvent::SubscriptionUpdated { plan_id, .. } => {
            set_plan(pool, account, plan_id.as_deref()).await?;
        }
        BillingEvent::InvoicePaid { credit_micros, .. } => {
            // The grant's *effect* is a ledger entry, so the balance stays a sum over one table
            // (docs/17 §domain model). Keyed by the Stripe event id, so a redelivery that somehow
            // reached this point still cannot double-credit.
            let entry = LedgerEntry {
                id: Uuid::new_v4(),
                account_id: account,
                kind: "grant.plan".into(),
                amount_micros: *credit_micros,
                quantity: serde_json::json!({ "source": "stripe", "event_id": event_id }),
                source: serde_json::json!({ "stripe_event": event_id }),
                idempotency_key: format!("stripe:{event_id}"),
            };
            match crate::pg::append(pool, &entry).await {
                Ok(()) => {}
                Err(PgError::Duplicate(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(account)
}

/// Which account a Stripe customer belongs to.
///
/// The mapping lives on the account row as a name for now — the customer id is stored when checkout
/// is created, and until that flow exists this resolves by name so the projection is testable end
/// to end. The seam is here so the lookup changes in one place.
async fn account_for_customer(pool: &PgPool, customer: &str) -> Result<Uuid, BillingError> {
    let row: Option<(Uuid,)> = sqlx::query_as("SELECT account_id FROM accounts WHERE name = $1")
        .bind(customer)
        .fetch_optional(pool)
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;
    row.map(|(id,)| id)
        .ok_or_else(|| BillingError::UnknownCustomer(customer.to_string()))
}

/// Move an account onto a plan, or off one.
///
/// Ending the old subscription and starting the new one happen in one transaction, because the
/// partial unique index refuses two active rows — which is the database enforcing a state nobody
/// wrote a handler for, exactly as intended.
pub async fn set_plan(
    pool: &PgPool,
    account_id: Uuid,
    plan_id: Option<&str>,
) -> Result<(), BillingError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;

    sqlx::query(
        "UPDATE subscriptions SET ended_at = now()
         WHERE account_id = $1 AND ended_at IS NULL",
    )
    .bind(account_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    if let Some(plan_id) = plan_id {
        sqlx::query(
            "INSERT INTO subscriptions (subscription_id, account_id, plan_id)
             VALUES ($1, $2, $3)",
        )
        .bind(Uuid::new_v4())
        .bind(account_id)
        .bind(plan_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;
    }

    tx.commit()
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(())
}

/// The account's current plan, or `None` if it has no active subscription.
pub async fn current_plan(pool: &PgPool, account_id: Uuid) -> Result<Option<String>, BillingError> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT plan_id FROM subscriptions WHERE account_id = $1 AND ended_at IS NULL",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(row.map(|(p,)| p))
}

/// Events that could not be applied, for the nightly reconcile and for a human.
pub async fn stuck(pool: &PgPool, limit: i64) -> Result<Vec<(String, String, i32)>, BillingError> {
    let rows: Vec<(String, Option<String>, i32)> = sqlx::query_as(
        "-- tenant-scoping: cross-tenant — an unapplied event has no account yet, which is the
         -- reason it is stuck.
         SELECT event_id, error, attempts FROM billing_events
         -- Most-attempted first: the operator wants the things that keep failing, not the thing
         -- that arrived a second ago and has not been tried yet.
         WHERE applied_at IS NULL ORDER BY attempts DESC, received_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|(id, error, attempts)| {
            (
                id,
                error.unwrap_or_else(|| "not yet applied".into()),
                attempts,
            )
        })
        .collect())
}

// ── M17.5: meter export ───────────────────────────────────────────────────────

/// Where usage aggregates are sent.
///
/// A trait because the thing on the other side is Stripe, and a billing pipeline whose correctness
/// can only be checked by charging somebody is one nobody checks. `RecordingMeter` is what the
/// tests and the nightly dry-run use.
#[async_trait::async_trait]
pub trait MeterSink: Send + Sync {
    /// One meter event: `(customer, meter, value, hour)`. Stripe's `/v1/billing/meter_events`.
    async fn send(
        &self,
        customer: &str,
        meter: &str,
        value: u64,
        hour: time::OffsetDateTime,
    ) -> Result<(), String>;
}

/// Keeps what it was told. The dry-run sink, and the one the tests assert on.
#[derive(Default)]
pub struct RecordingMeter {
    sent: std::sync::Mutex<Vec<(String, String, u64)>>,
}

impl RecordingMeter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn sent(&self) -> Vec<(String, String, u64)> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl MeterSink for RecordingMeter {
    async fn send(
        &self,
        customer: &str,
        meter: &str,
        value: u64,
        _hour: time::OffsetDateTime,
    ) -> Result<(), String> {
        self.sent
            .lock()
            .unwrap()
            .push((customer.to_string(), meter.to_string(), value));
        Ok(())
    }
}

/// The meter we report against.
///
/// Per *million* tokens, deliberately: docs/17 flags per-token rounding as "a known Stripe footgun",
/// and it is — a fractional unit price rounded per event loses a percent of a bill in a way nobody
/// can reconstruct afterwards. We send whole tokens and price per million on Stripe's side, so the
/// rounding happens once, on a number both sides can see.
pub const TOKENS_METER: &str = "panday_tokens";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportReport {
    pub hours: u32,
    pub events_sent: u32,
    pub already_exported: u32,
    pub tokens: u64,
    /// Accounts whose send failed. Their cursor was released, so the next run retries exactly those.
    pub failed: u32,
}

/// Export one hour of usage per account (M17.5).
///
/// Idempotent through `meter_exports`: re-running an hour that was already sent is a primary-key
/// collision, not a second charge. Stripe aggregates asynchronously and cannot deduplicate for us
/// (ADR-009), so the cursor has to be ours — and it is written *before* the send is attempted, then
/// rolled back if the send fails, because a cursor that advances on a failed send silently drops an
/// hour of somebody's usage.
///
/// **One account's failure does not abandon the hour.** The first version returned on the first
/// error, which meant a single unreachable customer record stopped every other account's usage from
/// being reported — and the retry would then re-walk the whole hour to reach the same failure. Now
/// each account is independent: failures are counted, their cursors released, and the caller decides
/// what a non-zero `failed` means.
pub async fn export_hour(
    pool: &PgPool,
    sink: &dyn MeterSink,
    hour: time::OffsetDateTime,
) -> Result<ExportReport, BillingError> {
    let start = hour
        .replace_minute(0)
        .and_then(|t| t.replace_second(0))
        .and_then(|t| t.replace_nanosecond(0))
        .map_err(|e| BillingError::Export(e.to_string()))?;
    let end = start + time::Duration::hours(1);

    let rows: Vec<(Uuid, String, i64)> = sqlx::query_as(
        "-- tenant-scoping: cross-tenant — the export walks every account by design; each row is
         -- keyed by account_id and sent to that account's own meter.
         SELECT l.account_id, a.name,
                sum(coalesce((l.quantity->>'input_tokens')::bigint, 0)
                    + coalesce((l.quantity->>'output_tokens')::bigint, 0))::bigint
         FROM ledger_entries l JOIN accounts a ON a.account_id = l.account_id
         WHERE l.kind = 'usage.model' AND l.at >= $1 AND l.at < $2
         GROUP BY l.account_id, a.name",
    )
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    let mut report = ExportReport {
        hours: 1,
        ..Default::default()
    };

    for (account_id, customer, tokens) in rows {
        let tokens = tokens.max(0) as u64;
        if tokens == 0 {
            continue;
        }

        // Claim the hour first. If the claim collides, somebody already sent it.
        let claimed = sqlx::query(
            "INSERT INTO meter_exports (account_id, hour, meter, value) VALUES ($1, $2, $3, $4)
             ON CONFLICT (account_id, hour, meter) DO NOTHING",
        )
        .bind(account_id)
        .bind(start)
        .bind(TOKENS_METER)
        .bind(tokens as i64)
        .execute(pool)
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;

        if claimed.rows_affected() == 0 {
            report.already_exported += 1;
            continue;
        }

        match sink.send(&customer, TOKENS_METER, tokens, start).await {
            Ok(()) => {
                report.events_sent += 1;
                report.tokens += tokens;
            }
            Err(e) => {
                // Release the claim, or a transient Stripe failure silently drops an hour of
                // somebody's usage and nothing ever notices.
                sqlx::query(
                    "DELETE FROM meter_exports WHERE account_id = $1 AND hour = $2 AND meter = $3",
                )
                .bind(account_id)
                .bind(start)
                .bind(TOKENS_METER)
                .execute(pool)
                .await
                .map_err(|e| PgError::Query(e.to_string()))?;
                tracing::error!(%account_id, hour = %start, error = %e, "meter export failed");
                report.failed += 1;
            }
        }
    }
    Ok(report)
}

/// What we told Stripe about an account over a period, and what our ledger says.
///
/// docs/17 M17.5: "invoice sanity check vs ledger to the cent on a seeded month". The check is not
/// against the invoice PDF — it is against what we *reported*, because that is the number the
/// invoice is computed from, and a difference here is a bug we can fix before a customer sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceCheck {
    pub exported_tokens: u64,
    pub ledger_tokens: u64,
}

impl InvoiceCheck {
    pub fn agrees(&self) -> bool {
        self.exported_tokens == self.ledger_tokens
    }

    /// Signed: positive means we reported more than the ledger holds, which would over-bill.
    pub fn difference(&self) -> i64 {
        self.exported_tokens as i64 - self.ledger_tokens as i64
    }
}

pub async fn check_invoice(
    pool: &PgPool,
    account_id: Uuid,
    from: time::OffsetDateTime,
    to: time::OffsetDateTime,
) -> Result<InvoiceCheck, BillingError> {
    let (exported,): (Option<i64>,) = sqlx::query_as(
        "SELECT sum(value)::bigint FROM meter_exports
         WHERE account_id = $1 AND hour >= $2 AND hour < $3 AND meter = $4",
    )
    .bind(account_id)
    .bind(from)
    .bind(to)
    .bind(TOKENS_METER)
    .fetch_one(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    let (ledger,): (Option<i64>,) = sqlx::query_as(
        "SELECT sum(coalesce((quantity->>'input_tokens')::bigint, 0)
                    + coalesce((quantity->>'output_tokens')::bigint, 0))::bigint
         FROM ledger_entries
         WHERE account_id = $1 AND kind = 'usage.model' AND at >= $2 AND at < $3",
    )
    .bind(account_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    Ok(InvoiceCheck {
        exported_tokens: exported.unwrap_or(0).max(0) as u64,
        ledger_tokens: ledger.unwrap_or(0).max(0) as u64,
    })
}

// ── The webhook endpoint ─────────────────────────────────────────────────────

/// `POST /v1/billing/webhook` (M17.4).
///
/// Receives and returns 200. It does **not** apply: applying is the job of `apply_pending`, run by
/// the same nightly reconcile that catches the deliveries that never arrived. A handler that
/// applied inline would make Stripe's retry policy our transaction boundary.
pub mod http {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::Router;

    #[derive(Clone)]
    pub struct WebhookState {
        pub pool: PgPool,
        /// Shared secret the sender must present.
        ///
        /// Not Stripe's signature scheme yet — that needs the live account this repo does not have,
        /// and a half-implemented signature check is worse than an honest shared secret because it
        /// *looks* like the real thing. The seam is one function, and `verify` is where the real
        /// check lands.
        pub secret: String,
    }

    pub fn router(state: WebhookState) -> Router {
        Router::new()
            .route("/v1/billing/webhook", post(webhook))
            .with_state(state)
    }

    /// Whether this request may write to the inbox.
    ///
    /// Constant-time comparison, because a shared secret checked with `==` leaks its length and its
    /// prefix to anyone willing to measure.
    pub fn verify(secret: &str, presented: &str) -> bool {
        if secret.is_empty() || secret.len() != presented.len() {
            return false;
        }
        secret
            .bytes()
            .zip(presented.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }

    async fn webhook(
        State(state): State<WebhookState>,
        headers: HeaderMap,
        body: String,
    ) -> Response {
        let presented = headers
            .get("x-panday-billing-secret")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !verify(&state.secret, presented) {
            return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
        }

        let payload: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        };
        let (Some(id), Some(kind)) = (
            payload.get("id").and_then(|v| v.as_str()),
            payload.get("type").and_then(|v| v.as_str()),
        ) else {
            return (
                StatusCode::BAD_REQUEST,
                "an event needs an `id` and a `type`",
            )
                .into_response();
        };

        match receive(&state.pool, id, kind, &payload).await {
            // 200 either way: a redelivery is not an error, and telling Stripe otherwise makes it
            // retry something we already have.
            Ok(_) => (StatusCode::OK, "ok").into_response(),
            Err(e) => {
                // A 500 is correct here: we did not store it, so we *want* the retry.
                tracing::error!(error = %e, "billing webhook not stored");
                (StatusCode::INTERNAL_SERVER_ERROR, "not stored").into_response()
            }
        }
    }
}
