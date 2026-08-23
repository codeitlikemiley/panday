//! Postgres-backed exact cache (docs/11 §Caching, M11.10).
//!
//! The spec has named a "PG unlogged table" since it was written; what existed was the trait and
//! an in-memory implementation, and no milestone owned the gap until M11.10 numbered it.
//!
//! **Why it lives here and not beside the trait.** `ExactCache` is `panday-gateway`'s, and that
//! crate has no `sqlx` dependency and must not grow one — CLAUDE.md §6, "libraries take traits,
//! binaries do the wiring". `panday-platform` already depends on `panday-gateway` and on `sqlx`,
//! which is the same arrangement `LedgerSink`/`UsageSink` and `PgRouteAudit`/`RouteAudit` have.
//!
//! **What a shared cache buys, and what it costs.** One replica's answer serves every other
//! replica's identical request, which is the point. The cost is that a lookup is now I/O on the
//! request path — bounded by the caller (`panday_gateway`'s `CACHE_GET_BUDGET`), and escalated per
//! docs/22 if PG p99 ever passes 5ms.

use panday_gateway::cache::{CacheKey, CachedResponse, ExactCache, CACHED_RESPONSE_SCHEMA_VERSION};
use panday_types::model::StreamItem;
use sqlx::postgres::PgPool;
use sqlx::Row;
use std::time::Duration;

pub struct PgExactCache {
    pool: PgPool,
}

impl PgExactCache {
    /// Wraps an existing pool. Does not create its table — the schema lives with the other
    /// platform migrations (`migrations/0010_exact_cache.sql`), the same arrangement `PgStore`
    /// has with 0009: a hosted database is migrated by the deployment, not by whichever process
    /// opens it first.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    async fn select(&self, key: &CacheKey) -> Result<Option<CachedResponse>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT items FROM exact_cache
             WHERE account_id = $1 AND digest = $2 AND schema_version = $3 AND expires_at > now()",
        )
        .bind(key.account.0)
        .bind(&key.digest)
        .bind(CACHED_RESPONSE_SCHEMA_VERSION)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let raw: serde_json::Value = row.get("items");
        match serde_json::from_value::<Vec<StreamItem>>(raw) {
            Ok(items) => Ok(Some(CachedResponse { items })),
            // A row this binary cannot read is a miss, never an error. The row is left in place
            // rather than deleted: it may be perfectly readable by the other half of a rolling
            // deploy, and it expires on its own regardless.
            Err(e) => {
                tracing::warn!(error = %e, "exact cache row did not deserialize; treating as a miss");
                Ok(None)
            }
        }
    }

    async fn upsert(
        &self,
        key: &CacheKey,
        response: &CachedResponse,
        ttl: Duration,
    ) -> Result<(), sqlx::Error> {
        // The interval is computed by the database, so the deadline is measured against the same
        // clock every replica filters on. This is the cross-process analogue of the in-memory
        // impl's `Instant::now() + ttl`, which has no meaning outside one process.
        sqlx::query(
            "INSERT INTO exact_cache (account_id, digest, schema_version, items, expires_at)
             VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5::double precision))
             ON CONFLICT (account_id, digest) DO UPDATE
                SET schema_version = EXCLUDED.schema_version,
                    items          = EXCLUDED.items,
                    written_at     = now(),
                    expires_at     = EXCLUDED.expires_at",
        )
        .bind(key.account.0)
        .bind(&key.digest)
        .bind(CACHED_RESPONSE_SCHEMA_VERSION)
        .bind(crate::pg::json(&response.items))
        .bind(ttl.as_secs_f64())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ExactCache for PgExactCache {
    async fn get(&self, key: &CacheKey) -> Option<CachedResponse> {
        match self.select(key).await {
            Ok(hit) => hit,
            // Infallible by contract: a database problem is a miss and a provider call, never a
            // failed request. Note that `pg::connect` sets a 5s acquire timeout, two orders of
            // magnitude above the caller's 25ms budget — on a saturated pool the caller's timeout
            // fires first, which is the intended behaviour.
            Err(e) => {
                tracing::warn!(error = %e, "exact cache read failed; treating as a miss");
                None
            }
        }
    }

    async fn put(&self, key: CacheKey, response: CachedResponse, ttl: Duration) {
        if let Err(e) = self.upsert(&key, &response, ttl).await {
            tracing::warn!(error = %e, "exact cache write failed; entry dropped");
        }
    }
}

/// Delete expired rows, forever, on an interval.
///
/// Filtering on `expires_at` at read time keeps stale rows from being *served*; it does not keep
/// them from accumulating. `MemoryExactCache` is bounded by an entry count, which has no analogue
/// here and must not be invented as one — an unlogged table with no reaper grows until the disk
/// does.
///
/// Every replica reaps and the deletes race harmlessly: the predicate only moves forward and the
/// rows are disposable. Spawned by the binary rather than by [`PgExactCache::new`], so a caller
/// that wants a cache without a background task can have one (docs/01: binaries do the wiring).
pub fn spawn_reaper(pool: PgPool, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            match reap(&pool).await {
                Ok(n) if n > 0 => tracing::debug!(rows = n, "reaped expired exact-cache rows"),
                Ok(_) => {}
                // Logged and retried on the next tick. A reaper that gave up on one failure turns
                // a transient blip into unbounded growth nobody is watching.
                Err(e) => tracing::warn!(error = %e, "exact cache reap failed"),
            }
        }
    })
}

/// One sweep. Separate from [`spawn_reaper`] so a test can run it without a background task.
pub async fn reap(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let done = sqlx::query(
        "-- tenant-scoping: cross-tenant — a retention delete keyed by age; it names no account
         -- and returns none.
         DELETE FROM exact_cache WHERE expires_at < now()",
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}
