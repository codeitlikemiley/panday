//! Route decisions, persisted (M12.2).
//!
//! docs/12 asks for an audit trail of what the router decided. The trail is only worth keeping if
//! it is cheap to write and safe to keep, so it is content-free (model ids, rule names, counts) and
//! written off the request path.
//!
//! **The write is fire-and-forget.** `RouteAudit::record` is awaited by the gateway, so a
//! synchronous insert here would put a database round trip in front of every token. An audit row is
//! evidence, not money: losing one to a database blip must degrade the dashboard, never the
//! request. That is the opposite of the ledger's contract (`OnWriteFailure`), and the difference is
//! deliberate — one of these is a bill and the other is a graph.

use crate::pg::PgError;
use panday_gateway::{RouteAudit, RouteRecord};
use sqlx::PgPool;
use uuid::Uuid;

pub struct PgRouteAudit {
    pool: PgPool,
}

impl PgRouteAudit {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl RouteAudit for PgRouteAudit {
    async fn record(&self, record: RouteRecord) {
        let pool = self.pool.clone();
        // Detached on purpose — see the module note. The task outliving the request is fine: it
        // holds a pool handle and a small struct, and the pool bounds how many can be in flight.
        tokio::spawn(async move {
            if let Err(e) = insert(&pool, &record).await {
                // Warn, never propagate. An operator wants to know the trail has holes; a caller
                // does not want their completion to fail because of one.
                tracing::warn!(error = %e, "route audit row was not written");
            }
        });
    }
}

/// Write one decision. Public so a caller that *wants* to wait — a test, a backfill — can.
pub async fn insert(pool: &PgPool, record: &RouteRecord) -> Result<(), PgError> {
    sqlx::query(
        "INSERT INTO route_decisions
             (request_id, account_id, requested, task, matched_rule, pool, chain, chosen, attempts,
              confidence, trusted)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (request_id) DO NOTHING",
    )
    .bind(record.request.0)
    .bind(record.account.0)
    .bind(&record.requested)
    .bind(record.task.as_str())
    .bind(&record.matched_rule)
    .bind(&record.pool)
    .bind(serde_json::json!(record.chain))
    .bind(record.chosen.as_deref())
    .bind(record.attempts as i32)
    .bind(record.confidence)
    .bind(record.trusted)
    .execute(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(())
}

/// One account's recent decisions, newest first — the support view.
pub async fn recent(pool: &PgPool, account_id: Uuid, limit: i64) -> Result<Vec<Decision>, PgError> {
    let rows: Vec<DecisionRow> = sqlx::query_as(
        "SELECT request_id, requested, task, matched_rule, pool, chain, chosen, attempts
         FROM route_decisions WHERE account_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    Ok(rows.into_iter().map(Decision::from_row).collect())
}

/// How often each rule fired, and how often it had to fail over, since `since`.
///
/// The aggregate docs/21's route board is built from. Computed in the database because the
/// alternative is streaming every row of the largest table into a process to count them.
pub async fn rule_health(
    pool: &PgPool,
    since: time::OffsetDateTime,
) -> Result<Vec<RuleHealth>, PgError> {
    let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "-- tenant-scoping: cross-tenant — operator aggregate keyed by rule, not by account; the
         -- result names no customer and carries no per-account number.
         SELECT matched_rule,
                count(*),
                count(*) FILTER (WHERE attempts > 1),
                count(*) FILTER (WHERE chosen IS NULL)
         FROM route_decisions WHERE created_at >= $1
         GROUP BY matched_rule ORDER BY count(*) DESC",
    )
    .bind(since)
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|(rule, total, failed_over, unserved)| RuleHealth {
            rule,
            total: total as u64,
            failed_over: failed_over as u64,
            unserved: unserved as u64,
        })
        .collect())
}

/// Drop decisions older than `days`, and say how many went.
///
/// An audit table with no retention is a disk-full incident with a scheduled date. The number is
/// the operator's to choose: long enough to investigate last month's bill, short enough that the
/// table is not the largest thing in the database.
pub async fn prune(pool: &PgPool, days: i64) -> Result<u64, PgError> {
    let done = sqlx::query(
        "-- tenant-scoping: cross-tenant — retention keyed by age; scoping it per account would
         -- leave every account that never calls again holding rows forever.
         DELETE FROM route_decisions WHERE created_at < now() - make_interval(days => $1::int)",
    )
    .bind(days as i32)
    .execute(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(done.rows_affected())
}

type DecisionRow = (
    Uuid,
    String,
    String,
    String,
    String,
    serde_json::Value,
    Option<String>,
    i32,
);

/// A decision as it comes back out.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub request_id: Uuid,
    pub requested: String,
    pub task: String,
    pub matched_rule: String,
    pub pool: String,
    pub chain: Vec<String>,
    pub chosen: Option<String>,
    pub attempts: u32,
}

impl Decision {
    fn from_row(
        (request_id, requested, task, matched_rule, pool, chain, chosen, attempts): DecisionRow,
    ) -> Self {
        Decision {
            request_id,
            requested,
            task,
            matched_rule,
            pool,
            chain: serde_json::from_value(chain).unwrap_or_default(),
            chosen,
            attempts: attempts.max(0) as u32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleHealth {
    pub rule: String,
    pub total: u64,
    /// Requests this rule served that took more than one leg.
    pub failed_over: u64,
    /// Requests this rule could not serve at all.
    pub unserved: u64,
}
