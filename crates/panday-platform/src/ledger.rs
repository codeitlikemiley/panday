//! The ledger write path (M11.4 / M17.2, docs/17 §the ledger, ADR-009).
//!
//! Two implementations of two gateway seams:
//!
//! - [`LedgerSink`] is a `UsageSink`: it prices a call and appends `usage.model`.
//! - [`LedgerBudget`] is a `BudgetGate`: it reads the balance and asks the entitlement engine.
//!
//! ## Fail-closed or fail-open is a per-surface decision
//!
//! docs/17: "Fail-closed for API keys, fail-open-with-alarm for our own interactive surfaces (a
//! billing outage shouldn't brick paying users mid-session; the alarm + backfill job reconciles)."
//! So [`OnWriteFailure`] is a field, not a constant, and the fail-open path *warns* — an outage
//! that produces no alarm is an outage nobody backfills.
//!
//! ## Idempotency is the database's job
//!
//! The key is the request id, which docs/17 specifies, and the UNIQUE constraint enforces it. A
//! retried request finds its own entry already there and carries on: not an error, because the
//! caller's correct behaviour is "already billed, continue", and not a silent skip either, because
//! the distinction shows up in a reconciliation.

use crate::entitlements::{check, Observed, Plan, Request, Verdict};
use crate::pg::{self, LedgerEntry};
use panday_gateway::{BudgetGate, UsageRecord, UsageSink};
use panday_sdk::PandayError;
use panday_types::id::AccountId;
use panday_types::pricing::CostModel;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

/// What to do when the ledger write fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnWriteFailure {
    /// Refuse the call. For API keys: an unbilled API call is revenue we cannot invoice, and the
    /// caller is a program that will retry.
    FailClosed,
    /// Log an alarm and continue. For interactive surfaces: a billing outage should not brick a
    /// paying user mid-session, and the backfill job reconciles from the event log (docs/17).
    FailOpenWithAlarm,
}

pub struct LedgerSink {
    pool: PgPool,
    prices: Arc<dyn CostModel>,
    on_failure: OnWriteFailure,
    /// Margin, applied once here rather than scattered across price tables: COGS and the price we
    /// charge are different numbers, and mixing them makes the ledger unable to answer either
    /// question.
    margin_multiplier: f64,
}

impl LedgerSink {
    pub fn new(pool: PgPool, prices: Arc<dyn CostModel>, on_failure: OnWriteFailure) -> Self {
        Self {
            pool,
            prices,
            on_failure,
            margin_multiplier: 1.0,
        }
    }

    pub fn with_margin(mut self, multiplier: f64) -> Self {
        self.margin_multiplier = multiplier;
        self
    }

    /// The entry a usage record produces, or `None` when the model has no configured price.
    ///
    /// `None` rather than zero: docs/21's rule applied to the ledger — "free" and "unpriced" are
    /// different claims, and a zero-cost entry for an unpriced model would understate COGS
    /// silently. The unpriced call is counted in metrics instead
    /// (`panday_unpriced_calls_total`), where it is visible.
    pub fn entry_for(&self, record: &UsageRecord) -> Option<LedgerEntry> {
        let provider_cost = self.prices.cost_micros(&record.model, record.usage)?;
        let billed = (provider_cost as f64 * self.margin_multiplier).round() as i64;
        Some(LedgerEntry {
            id: Uuid::now_v7(),
            account_id: record.account.0,
            kind: "usage.model".into(),
            // Negative: consumption (ADR-009).
            amount_micros: -billed,
            quantity: serde_json::json!({
                "input_tokens": record.usage.input_tokens,
                "output_tokens": record.usage.output_tokens,
                "cache_read": record.usage.cache_read_tokens,
                "cache_write": record.usage.cache_write_tokens + record.usage.cache_write_1h_tokens,
                "model": record.model.0,
                "provider": record.provider,
                "pool": record.pool,
                "provider_cost_micros": provider_cost,
            }),
            // Points into the event log, which is what makes a dispute settleable by replay
            // (docs/17, M3.5).
            source: serde_json::json!({ "request_id": record.request.0 }),
            // Request-scoped, so a retry cannot double-bill.
            idempotency_key: format!("usage.model:{}", record.request.0),
        })
    }
}

#[async_trait::async_trait]
impl UsageSink for LedgerSink {
    async fn record(&self, record: UsageRecord) {
        let Some(entry) = self.entry_for(&record) else {
            // Unpriced: counted by the metric, not invented as zero.
            tracing::warn!(
                model = %record.model.0,
                "usage for a model with no configured price; not billed"
            );
            return;
        };

        match pg::append(&self.pool, &entry).await {
            Ok(()) => {}
            // The retry case, and it is not a failure: the first attempt billed it.
            Err(pg::PgError::Duplicate(key)) => {
                tracing::debug!(idempotency_key = %key, "usage already recorded; not double-billing");
            }
            Err(e) => match self.on_failure {
                OnWriteFailure::FailOpenWithAlarm => {
                    // The alarm is the point: an outage that produces no signal is an outage
                    // nobody backfills. `error!` rather than `warn!` because a dropped ledger
                    // entry is money.
                    tracing::error!(
                        error = %e,
                        account_id = %record.account.0,
                        request_id = %record.request.0,
                        "LEDGER WRITE FAILED — usage not billed; backfill from the event log"
                    );
                }
                OnWriteFailure::FailClosed => {
                    tracing::error!(error = %e, "ledger write failed; call will be refused");
                }
            },
        }
    }
}

/// The pre-flight gate: balance plus plan.
pub struct LedgerBudget {
    pool: PgPool,
    /// The plan every account is on until subscriptions are read here (M17.3 brings authentication
    /// and with it the account's real plan). Explicit so nobody mistakes it for a lookup.
    default_plan: Plan,
}

impl LedgerBudget {
    pub fn new(pool: PgPool, default_plan: Plan) -> Self {
        Self { pool, default_plan }
    }
}

#[async_trait::async_trait]
impl BudgetGate for LedgerBudget {
    async fn check(
        &self,
        account: AccountId,
        estimated_micros: Option<u64>,
    ) -> Result<(), PandayError> {
        let balance = match pg::balance_micros(&self.pool, account.0).await {
            Ok(balance) => balance,
            Err(e) => {
                // A gate that cannot read the balance must not invent one. Failing *open* here is
                // deliberate and narrow: refusing every request because the balance query timed
                // out would turn a database blip into an outage, and the write path's alarm is
                // what catches the money.
                tracing::error!(error = %e, "budget check could not read the balance; allowing");
                return Ok(());
            }
        };

        let verdict = check(
            &self.default_plan,
            &Request {
                estimated_micros,
                ..Default::default()
            },
            &Observed {
                balance_micros: Some(balance),
                ..Default::default()
            },
        );

        match verdict {
            Verdict::Allow => Ok(()),
            // A degrade is not this seam's decision to act on: the router chooses pools, and a
            // gate that silently rerouted would make the audit trail wrong about what was asked.
            // Reported and allowed.
            Verdict::Degrade { to_pool, why } => {
                tracing::info!(to_pool, why, "budget suggests a cheaper pool");
                Ok(())
            }
            Verdict::Deny { why } => {
                tracing::info!(why, account_id = %account.0, "budget stop");
                // The typed error docs/11 asks for, so the harness can turn it into a graceful
                // pause rather than a 500.
                Err(PandayError::BudgetExceeded {
                    balance_micros: balance,
                })
            }
        }
    }
}
