//! `PgStore` against a real Postgres (docs/25 M25.11).
//!
//! The whole test is one call: [`panday_sdk::vault::conformance::run`] is the same
//! matrix `MemoryStore` and `SqliteStore` already pass, so this asks the hosted store
//! for nothing the laptop store was not already held to. That is the point of having
//! written the suite before the third implementation — a divergence between stores
//! shows up as a named failure here rather than as a support ticket.
//!
//! `#[ignore]`d, because docs/02's unit lane is "no network, no docker". To run:
//!
//! ```text
//! docker compose -f deploy/integration-compose.yml up -d --wait
//! PANDAY_TEST_DATABASE_URL=postgres://panday:panday@127.0.0.1:5433/panday_test \
//!   cargo test -p panday-platform --test pg_vault -- --ignored
//! ```

use panday_platform::pg;
use panday_sdk::vault::{CredentialStore, Kek, PgStore};
use sqlx::postgres::PgPoolOptions;
use sqlx::AssertSqlSafe;
use std::sync::Arc;
use uuid::Uuid;

/// Ours, not anyone's input — see `pg::apply_all` for the same call and the same reason.
async fn run(pool: &sqlx::PgPool, sql: String) {
    sqlx::raw_sql(AssertSqlSafe(sql.clone()))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn url() -> String {
    pg::test_database_url()
        .expect("PANDAY_TEST_DATABASE_URL is unset — start deploy/integration-compose.yml first")
}

/// A store with a schema to itself.
///
/// The conformance suite asserts on `list()`, which is deliberately operator-global —
/// there is no `account_id` to scope by, so two stores sharing a schema would see each
/// other's rows and the suite would fail on a count. Every other integration test here
/// isolates by owning an account; this one owns a schema instead.
///
/// The table comes from the real `0009_credentials.sql` rather than a copy pasted into
/// this file, so the migration that ships is the one under test.
async fn isolated_store() -> Arc<dyn CredentialStore> {
    let url = url();
    let schema = format!("vault_{}", Uuid::now_v7().simple());

    let admin = pg::connect(&url).await.expect("connect");
    // `raw_sql` + `AssertSqlSafe`, as `pg::apply_all` does: a borrowed query is pinned to
    // `'static` by the pool's `Executor` impl, and this string is built here rather than
    // typed by anyone.
    run(&admin, format!("CREATE SCHEMA {schema}")).await;
    admin.close().await;

    let scoped = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        // Per connection, not once: the pool opens more of them lazily, and a connection
        // that missed this would silently read and write `public` instead.
        .after_connect(move |conn, _meta| {
            let scoped = scoped.clone();
            Box::pin(async move {
                sqlx::raw_sql(AssertSqlSafe(format!("SET search_path = {scoped}")))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("connect");

    let sql = std::fs::read_to_string(pg::migrations_dir().join("0009_credentials.sql"))
        .expect("0009_credentials.sql");
    run(&pool, sql).await;

    Arc::new(PgStore::new(pool, Kek::generate()))
}

/// Schemas left behind by *earlier runs*, and only those.
///
/// A bare `DROP SCHEMA <prefix>_% CASCADE` is not safe here: nextest runs each test in its own
/// process, concurrently, so a sweep that matched on the prefix alone would delete the schemas the
/// other tests in this file are actively using. That is exactly how this suite first went red on
/// CI while passing locally, where one `cargo test` binary was slow enough to get away with it.
///
/// The names embed a UUIDv7, whose leading 48 bits are a millisecond timestamp, so "earlier run"
/// is a fact the name carries rather than something to infer. An hour is far longer than any run
/// and far shorter than the database's life. Done at the start rather than the end, so a failing
/// run leaves its schema behind to inspect.
async fn drop_stale_schemas(prefix: &str) {
    let admin = pg::connect(&url()).await.expect("connect");
    let names: Vec<(String,)> = sqlx::query_as(
        "SELECT schema_name FROM information_schema.schemata WHERE schema_name LIKE $1",
    )
    .bind(format!("{prefix}\\_%"))
    .fetch_all(&admin)
    .await
    .expect("list schemas");

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    for (name,) in names {
        let stamp = name
            .strip_prefix(prefix)
            .and_then(|r| r.strip_prefix('_'))
            .and_then(|hex| u64::from_str_radix(hex.get(..12)?, 16).ok());
        // A name that does not parse is not ours to delete.
        let Some(created_ms) = stamp else { continue };
        if now_ms.saturating_sub(created_ms) > 60 * 60 * 1000 {
            run(&admin, format!("DROP SCHEMA {name} CASCADE")).await;
        }
    }
    admin.close().await;
}

#[tokio::test]
#[ignore = "needs the integration lane (deploy/integration-compose.yml)"]
async fn pg_store_satisfies_the_conformance_suite() {
    drop_stale_schemas("vault").await;
    panday_sdk::vault::conformance::run(
        "PgStore",
        Arc::new(|| Box::pin(async { isolated_store().await })),
    )
    .await;
}

/// The credentials table is reachable through the ordinary migration path, not only
/// through the hand-applied one above — and `/status` now asserts nine of them.
#[tokio::test]
#[ignore = "needs the integration lane (deploy/integration-compose.yml)"]
async fn the_migration_set_applies_and_includes_credentials() {
    let pool = pg::connect(&url()).await.expect("connect");
    let applied = pg::migrate(&pool, &pg::migrations_dir())
        .await
        .expect("migrate");
    assert!(
        applied.iter().any(|m| m.contains("0009_credentials")),
        "0009 must be part of the migration set, not a file only the vault test knows about: {applied:?}"
    );

    let (count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM information_schema.tables
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE'",
    )
    .fetch_one(&pool)
    .await
    .expect("count tables");
    assert!(
        count >= 9,
        "admin.rs reports the schema unhealthy below nine tables; found {count}"
    );
}
