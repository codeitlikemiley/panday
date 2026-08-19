//! The Postgres store (docs/02 M2.3, extended by docs/17 M17.1).
//!
//! ## Migrations are files, applied in order, and checked in
//!
//! `migrations/NNNN_name.sql`, applied by `migrate`. Not `sqlx::migrate!` — that macro embeds
//! the files at compile time, which means a schema change needs a rebuild of anything that
//! links this crate, and it hides the SQL from the M20.3 tenant-scoping lint, which reads
//! `.sql` files. The loop below is ten lines and keeps both properties.
//!
//! ## Every query names `account_id`
//!
//! docs/20 T5: "every query is tenant-scoped by construction". The lint enforces it across the
//! repo; this module is where it first has something to enforce. The one query that does not
//! scope is `migrate`, which is DDL.

use serde::Serialize;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum PgError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate {file}: {detail}")]
    Migrate { file: String, detail: String },
    #[error("query: {0}")]
    Query(String),
    /// The ledger's `idempotency_key` did its job.
    #[error("already recorded: {0}")]
    Duplicate(String),
}

/// Connect with a pool sized for a service rather than a script.
pub async fn connect(url: &str) -> Result<PgPool, PgError> {
    PgPoolOptions::new()
        .max_connections(8)
        // Fail fast: a service that hangs on a dead database looks like a slow service, and an
        // operator debugs the wrong thing for an hour.
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await
        .map_err(|e| PgError::Connect(e.to_string()))
}

/// Apply every migration in `dir`, in filename order, idempotently.
///
/// The migrations themselves are written `CREATE TABLE IF NOT EXISTS`, so applying them twice is
/// a no-op — a tracking table can come with M17.1 when there is a second migration whose order
/// matters. Being honest about that now is better than a half-built migration framework.
pub async fn migrate(pool: &PgPool, dir: &std::path::Path) -> Result<Vec<String>, PgError> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| PgError::Migrate {
            file: dir.display().to_string(),
            detail: e.to_string(),
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "sql"))
        .collect();
    files.sort();

    let mut owned: Vec<(String, String)> = Vec::new();
    for path in files {
        let sql = std::fs::read_to_string(&path).map_err(|e| PgError::Migrate {
            file: path.display().to_string(),
            detail: e.to_string(),
        })?;
        owned.push((
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            sql,
        ));
    }
    let borrowed: Vec<(&str, &str)> = owned
        .iter()
        .map(|(n, s)| (n.as_str(), s.as_str()))
        .collect();
    apply(pool, &borrowed).await
}

/// The migrations compiled into the binary, in order.
///
/// A service in a container has no `migrations/` directory to read — `CARGO_MANIFEST_DIR` is a
/// build machine's path — so the deployable path is this one. Listed by hand rather than globbed
/// because a build script that walked the directory would make the set depend on what happened to
/// be checked out; a test asserts this list and the directory agree, so adding a file and
/// forgetting this line fails in CI rather than at the first boot after a deploy.
pub const EMBEDDED_MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init.sql", include_str!("../migrations/0001_init.sql")),
    (
        "0002_accounts_keys_plans.sql",
        include_str!("../migrations/0002_accounts_keys_plans.sql"),
    ),
    (
        "0003_balances.sql",
        include_str!("../migrations/0003_balances.sql"),
    ),
    (
        "0004_key_last_used.sql",
        include_str!("../migrations/0004_key_last_used.sql"),
    ),
    (
        "0005_route_decisions.sql",
        include_str!("../migrations/0005_route_decisions.sql"),
    ),
    (
        "0006_session_events.sql",
        include_str!("../migrations/0006_session_events.sql"),
    ),
    (
        "0007_billing_inbox.sql",
        include_str!("../migrations/0007_billing_inbox.sql"),
    ),
];

/// Apply the compiled-in migrations. What a deployed service calls.
pub async fn migrate_embedded(pool: &PgPool) -> Result<Vec<String>, PgError> {
    apply(pool, EMBEDDED_MIGRATIONS).await
}

async fn apply(pool: &PgPool, migrations: &[(&str, &str)]) -> Result<Vec<String>, PgError> {
    // One migrator at a time. `CREATE TABLE IF NOT EXISTS` is *not* atomic against a concurrent
    // create: two of them race in the system catalog and one gets "duplicate key value violates
    // unique constraint pg_type_typname_nsp_index". Found by the integration suite, whose six
    // tests each migrate on entry — and it is the same race a rolling deploy has when several
    // pods boot at once, so the lock belongs here rather than in the test.
    //
    // A session-level advisory lock: held until this connection releases it, which happens
    // below, and released automatically if the process dies mid-migration.
    const MIGRATION_LOCK: i64 = 0x0070_616e_6461_7901;
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| PgError::Connect(e.to_string()))?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATION_LOCK)
        .execute(&mut *conn)
        .await
        .map_err(|e| PgError::Migrate {
            file: "advisory lock".into(),
            detail: e.to_string(),
        })?;

    let result = apply_all(&mut conn, migrations).await;

    // Released even on failure: holding it would block every other migrator on a database that
    // is already in trouble.
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATION_LOCK)
        .execute(&mut *conn)
        .await;
    result
}

async fn apply_all(
    conn: &mut sqlx::PgConnection,
    migrations: &[(&str, &str)],
) -> Result<Vec<String>, PgError> {
    let mut applied = Vec::new();
    for (name, sql) in migrations {
        // `AssertSqlSafe`, because the string comes from a checked-in migration file rather
        // than from anything a user typed. sqlx makes this explicit on purpose, and the honest
        // answer is the one it asks for: this SQL is ours.
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&mut *conn)
            .await
            .map_err(|e| PgError::Migrate {
                file: (*name).to_string(),
                detail: e.to_string(),
            })?;
        applied.push((*name).to_string());
    }
    Ok(applied)
}

/// One ledger entry, in the shape docs/17's schema defines.
#[derive(Debug, Clone, PartialEq)]
pub struct LedgerEntry {
    pub id: Uuid,
    pub account_id: Uuid,
    pub kind: String,
    /// Negative = consumption, in credit-micros.
    pub amount_micros: i64,
    pub quantity: serde_json::Value,
    /// Points into the event log, which is what makes a dispute settleable by replay.
    pub source: serde_json::Value,
    pub idempotency_key: String,
}

pub async fn create_account(pool: &PgPool, name: &str) -> Result<Uuid, PgError> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO accounts (account_id, name) VALUES ($1, $2)")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(id)
}

/// Append one entry, and move the balance in the same transaction (M17.2).
///
/// Both statements or neither: a balance that can diverge from its entries is worse than a `SUM`,
/// because it is wrong quietly. The duplicate case gets that for free — the insert fails, the
/// transaction rolls back, and the balance never moved.
///
/// A duplicate `idempotency_key` is reported as `Duplicate` rather than as a hard error, because the
/// caller's correct response is "already done, carry on" — a retried request must not double-bill
/// and must not fail either (docs/17: "retries can't double-bill").
pub async fn append(pool: &PgPool, entry: &LedgerEntry) -> Result<(), PgError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;

    let inserted = sqlx::query(
        "INSERT INTO ledger_entries
             (id, account_id, kind, amount_micros, quantity, source, idempotency_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(entry.id)
    .bind(entry.account_id)
    .bind(&entry.kind)
    .bind(entry.amount_micros)
    .bind(&entry.quantity)
    .bind(&entry.source)
    .bind(&entry.idempotency_key)
    .execute(&mut *tx)
    .await;

    match inserted {
        Ok(_) => {}
        Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
            return Err(PgError::Duplicate(entry.idempotency_key.clone()))
        }
        Err(e) => return Err(PgError::Query(e.to_string())),
    }

    // The balance moves by the entry's amount rather than being recomputed: a `SUM` here would make
    // every write cost a scan, which is the thing the view exists to avoid.
    sqlx::query(
        "INSERT INTO balances (account_id, balance_micros, entry_count)
         VALUES ($1, $2, 1)
         ON CONFLICT (account_id) DO UPDATE
             SET balance_micros = balances.balance_micros + EXCLUDED.balance_micros,
                 entry_count    = balances.entry_count + 1,
                 updated_at     = now()",
    )
    .bind(entry.account_id)
    .bind(entry.amount_micros)
    .execute(&mut *tx)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    tx.commit().await.map_err(|e| PgError::Query(e.to_string()))
}

/// The balance, in credit-micros: **one indexed lookup**, no `SUM` (docs/17).
///
/// An account with no entries has no row, and that reads as zero — which is right, and is why the
/// query cannot be a plain `fetch_one`.
pub async fn balance_micros(pool: &PgPool, account_id: Uuid) -> Result<i64, PgError> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT balance_micros FROM balances WHERE account_id = $1")
            .bind(account_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(row.map_or(0, |(balance,)| balance))
}

/// The balance recomputed from the entries. **Not the request path** — this is what the drift check
/// compares the view against (M21.4's monitor will run it on a schedule).
///
/// `SUM(bigint)` is NUMERIC in Postgres, not BIGINT: a real database taught this suite that, and
/// reading it as `i64` failed with a type mismatch. The cast is safe rather than convenient — i64
/// micro-credits is ~9.2e12 dollars, so a balance that overflows it is a reconciliation problem long
/// before it is a decoding problem.
pub async fn balance_from_entries(pool: &PgPool, account_id: Uuid) -> Result<i64, PgError> {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(amount_micros), 0)::bigint AS balance
         FROM ledger_entries WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_one(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    row.try_get::<i64, _>("balance")
        .map_err(|e| PgError::Query(e.to_string()))
}

/// Recompute one account's cached balance from its entries.
///
/// The other half of a drift check: a monitor that only reports leaves an operator hand-writing
/// `UPDATE`s against a money table at 3am, which is how a drift becomes a bigger drift. Scoped to one
/// account so a repair is a decision about a known problem rather than a database-wide rewrite.
pub async fn repair_balance(pool: &PgPool, account_id: Uuid) -> Result<i64, PgError> {
    let computed = balance_from_entries(pool, account_id).await?;
    sqlx::query(
        "INSERT INTO balances (account_id, balance_micros, entry_count)
         SELECT $1, $2, COUNT(*) FROM ledger_entries WHERE account_id = $1
         ON CONFLICT (account_id) DO UPDATE
             SET balance_micros = EXCLUDED.balance_micros,
                 entry_count    = EXCLUDED.entry_count,
                 updated_at     = now()",
    )
    .bind(account_id)
    .bind(computed)
    .execute(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(computed)
}

/// Accounts whose cached balance disagrees with their entries.
///
/// Empty is the invariant; anything else is a bug in the write path, and finding it by query is how
/// M21.4's drift monitor will report it. One statement rather than a loop, because a drift check that
/// takes a minute per account is a drift check nobody schedules.
pub async fn balance_drift(pool: &PgPool) -> Result<Vec<(Uuid, i64, i64)>, PgError> {
    let rows: Vec<(Uuid, i64, i64)> = sqlx::query_as(
        "SELECT b.account_id, b.balance_micros,
                COALESCE(SUM(e.amount_micros), 0)::bigint AS computed
         FROM balances b
         LEFT JOIN ledger_entries e ON e.account_id = b.account_id
         GROUP BY b.account_id, b.balance_micros
         HAVING b.balance_micros <> COALESCE(SUM(e.amount_micros), 0)::bigint",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(rows)
}

/// An account's entries, newest first.
pub async fn entries(
    pool: &PgPool,
    account_id: Uuid,
    limit: i64,
) -> Result<Vec<LedgerEntry>, PgError> {
    let rows = sqlx::query(
        "SELECT id, account_id, kind, amount_micros, quantity, source, idempotency_key
         FROM ledger_entries WHERE account_id = $1 ORDER BY at DESC LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    rows.into_iter()
        .map(|row| {
            Ok(LedgerEntry {
                id: row.try_get("id").map_err(q)?,
                account_id: row.try_get("account_id").map_err(q)?,
                kind: row.try_get("kind").map_err(q)?,
                amount_micros: row.try_get("amount_micros").map_err(q)?,
                quantity: row.try_get("quantity").map_err(q)?,
                source: row.try_get("source").map_err(q)?,
                idempotency_key: row.try_get("idempotency_key").map_err(q)?,
            })
        })
        .collect()
}

fn q(e: sqlx::Error) -> PgError {
    PgError::Query(e.to_string())
}

/// Where the migrations live, for a caller that has not vendored them.
pub fn migrations_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations")
}

/// The integration lane's database, or `None` when it is not running.
///
/// A missing URL means "skip", not "fail": the unit lane must stay runnable with no Docker
/// (docs/02 §CI shape), and the integration tests are `#[ignore]`d for the same reason.
pub fn test_database_url() -> Option<String> {
    std::env::var("PANDAY_TEST_DATABASE_URL").ok()
}

/// Convenience for a caller that wants JSON out of a typed value.
pub fn json(value: &impl Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod embedded_tests {
    use super::*;

    #[test]
    fn every_migration_file_is_compiled_into_the_binary() {
        // The failure this prevents: a new migration is written, tests pass (they read the
        // directory), and the deployed service — which can only see what was compiled in — boots
        // against a schema that is one table short.
        let mut on_disk: Vec<String> = std::fs::read_dir(migrations_dir())
            .expect("migrations directory")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        on_disk.sort();

        let embedded: Vec<String> = EMBEDDED_MIGRATIONS
            .iter()
            .map(|(n, _)| (*n).to_string())
            .collect();
        assert_eq!(
            embedded, on_disk,
            "EMBEDDED_MIGRATIONS and migrations/ disagree — add the new file to the list, in order"
        );
    }

    #[test]
    fn the_embedded_migrations_are_in_filename_order() {
        // Order is the schema: 0004 alters a table 0002 creates.
        let mut sorted: Vec<&str> = EMBEDDED_MIGRATIONS.iter().map(|(n, _)| *n).collect();
        let original = sorted.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, original);
    }
}
