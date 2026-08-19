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

/// Sandbox time, billed (M14.7, docs/17: "the second metered good").
///
/// Priced per tier-second, because that is the only honest unit: a T0 in-process call costs us
/// nothing marginal, a T2 jail costs a process, and a T3 microVM costs a machine slice. One price
/// for "a sandbox second" would either overcharge for T0 or give T3 away.
pub struct SandboxLedger {
    pool: PgPool,
    /// Micro-credits per second, by tier.
    per_second: TierPrices,
    on_failure: OnWriteFailure,
}

/// Per-tier rates. Placeholders against measured COGS, like docs/17's plan tiers — and written in
/// code rather than a config file so a change is reviewed.
#[derive(Debug, Clone, Copy)]
pub struct TierPrices {
    pub t0_in_process: u64,
    pub t1_wasm: u64,
    pub t2_os_jail: u64,
    pub t3_micro_vm: u64,
}

impl Default for TierPrices {
    fn default() -> Self {
        Self {
            // Zero, and deliberately so: a T0 tool is a Rust function in our own process. Billing
            // for it would be billing for CPU we already paid for in the request.
            t0_in_process: 0,
            // A wasmtime instantiation: cheap, but not free, and metered so a plugin that spins is
            // visible in the ledger rather than only in a metric.
            t1_wasm: 10,
            // A process with a jail around it, on the user's own machine in the local case and on
            // ours in the cloud one.
            t2_os_jail: 100,
            // A microVM: the only tier where a second of somebody's code costs a slice of a
            // machine we rent (docs/14 T3).
            t3_micro_vm: 500,
        }
    }
}

impl TierPrices {
    /// What one execution costs, in micro-credits.
    ///
    /// Milliseconds are billed as milliseconds rather than rounded up to a second: a loop of forty
    /// 50ms tool calls would otherwise be charged forty seconds, which is not a rounding error but a
    /// different price. Pure arithmetic, so it is testable without a database — the first draft put
    /// this inside a method that needed a pool, and the test had to build one to check multiplication.
    pub fn amount_micros(
        &self,
        tier: panday_sandbox::SandboxTier,
        duration: std::time::Duration,
    ) -> i64 {
        let rate = self.for_tier(tier);
        let millis = duration.as_millis() as u64;
        (rate.saturating_mul(millis) / 1_000) as i64
    }

    pub fn for_tier(&self, tier: panday_sandbox::SandboxTier) -> u64 {
        use panday_sandbox::SandboxTier::*;
        match tier {
            T0InProcess => self.t0_in_process,
            T1Wasm => self.t1_wasm,
            T2OsJail => self.t2_os_jail,
            T3MicroVm => self.t3_micro_vm,
        }
    }
}

impl SandboxLedger {
    pub fn new(pool: PgPool, on_failure: OnWriteFailure) -> Self {
        Self {
            pool,
            per_second: TierPrices::default(),
            on_failure,
        }
    }

    pub fn with_prices(mut self, per_second: TierPrices) -> Self {
        self.per_second = per_second;
        self
    }

    /// The entry one execution produces.
    pub fn entry_for(&self, usage: &panday_harness::SandboxUsage) -> LedgerEntry {
        let rate = self.per_second.for_tier(usage.tier);
        let millis = usage.duration.as_millis() as u64;
        let billed = self.per_second.amount_micros(usage.tier, usage.duration);
        LedgerEntry {
            id: Uuid::now_v7(),
            account_id: usage.account.0,
            kind: "usage.sandbox".into(),
            amount_micros: -billed,
            quantity: serde_json::json!({
                "sandbox_ms": millis,
                "tier": usage.tier.as_str(),
                "tool": usage.tool,
                "rate_micros_per_second": rate,
            }),
            source: serde_json::json!({
                "session_id": usage.session.0,
                "call_id": usage.call_id.0,
            }),
            // The call is the unit of work, so a resumed turn re-running a replay-safe call is
            // billed once (docs/13 §persist-before-proceed meets docs/17 §idempotency).
            idempotency_key: format!("usage.sandbox:{}", usage.call_id.0),
        }
    }
}

#[async_trait::async_trait]
impl panday_harness::SandboxUsageSink for SandboxLedger {
    async fn record(&self, usage: panday_harness::SandboxUsage) {
        let entry = self.entry_for(&usage);
        // A zero-cost tier still gets an entry: the execution happened, and a ledger that omitted
        // free work could not answer "what did this session do" — which is the question a dispute
        // starts from.
        match pg::append(&self.pool, &entry).await {
            Ok(()) => {}
            Err(pg::PgError::Duplicate(key)) => {
                tracing::debug!(idempotency_key = %key, "sandbox usage already recorded");
            }
            Err(e) => match self.on_failure {
                OnWriteFailure::FailOpenWithAlarm => tracing::error!(
                    error = %e,
                    account_id = %usage.account.0,
                    call_id = %usage.call_id.0,
                    "SANDBOX LEDGER WRITE FAILED — not billed; backfill from the event log"
                ),
                OnWriteFailure::FailClosed => {
                    tracing::error!(error = %e, "sandbox ledger write failed")
                }
            },
        }
    }
}
