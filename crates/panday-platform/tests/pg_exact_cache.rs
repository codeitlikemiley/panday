//! `PgExactCache` against a real Postgres (docs/11 §Caching, M11.10).
//!
//! The bulk of it is one call to `panday_gateway::cache::conformance::run` — the same matrix
//! `MemoryExactCache` passes, so the hosted cache is asked for nothing the in-memory one was not.
//! What is here beyond that is what only a shared, durable store can get wrong: surviving a
//! restart, being visible to another instance, and refusing to turn an unreadable row into a
//! failed request.
//!
//! `#[ignore]`d, because docs/02's unit lane is "no network, no docker". To run:
//!
//! ```text
//! docker compose -f deploy/integration-compose.yml up -d --wait
//! PANDAY_TEST_DATABASE_URL=postgres://panday:panday@127.0.0.1:5433/panday_test \
//!   cargo test -p panday-platform --test pg_exact_cache -- --ignored
//! ```

use panday_gateway::cache::{CacheKey, CachedResponse, ExactCache};
use panday_platform::exact_cache::{reap, PgExactCache};
use panday_platform::pg;
use panday_types::id::AccountId;
use panday_types::model::{StopReason, StreamItem};
use sqlx::postgres::PgPoolOptions;
use sqlx::AssertSqlSafe;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

fn url() -> String {
    pg::test_database_url()
        .expect("PANDAY_TEST_DATABASE_URL is unset — start deploy/integration-compose.yml first")
}

/// Ours, not anyone's input — the same call `pg::apply_all` makes, for the same reason.
async fn run_sql(pool: &sqlx::PgPool, sql: String) {
    sqlx::raw_sql(AssertSqlSafe(sql.clone()))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// A pool with a schema to itself.
///
/// The conformance suite asserts on absolute cache contents, and `exact_cache` is keyed by
/// `(account_id, digest)` with no notion of a test run — so two suites sharing a schema would see
/// each other's rows. `pg_vault.rs` isolates the same way and for the same reason.
async fn isolated_pool() -> sqlx::PgPool {
    let url = url();
    let schema = format!("cache_{}", Uuid::now_v7().simple());

    let admin = pg::connect(&url).await.expect("connect");
    run_sql(&admin, format!("CREATE SCHEMA {schema}")).await;
    admin.close().await;

    let scoped = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(8)
        // Per connection, not once: the pool opens more lazily, and one that missed this would
        // silently read and write `public`.
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

    // The real migration file, not a copy pasted here, so the DDL that ships is the DDL tested.
    let sql = std::fs::read_to_string(pg::migrations_dir().join("0010_exact_cache.sql"))
        .expect("0010_exact_cache.sql");
    run_sql(&pool, sql).await;
    pool
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
            run_sql(&admin, format!("DROP SCHEMA {name} CASCADE")).await;
        }
    }
    admin.close().await;
}

fn key(digest: &str) -> CacheKey {
    CacheKey {
        account: AccountId(Uuid::from_u128(7)),
        digest: digest.into(),
    }
}

fn response(text: &str) -> CachedResponse {
    CachedResponse {
        items: vec![
            StreamItem::Delta { text: text.into() },
            StreamItem::Done {
                reason: StopReason::EndTurn,
            },
        ],
    }
}

#[tokio::test]
#[ignore = "needs the integration lane (deploy/integration-compose.yml)"]
async fn pg_exact_cache_satisfies_the_conformance_suite() {
    drop_stale_schemas("cache").await;
    panday_gateway::cache::conformance::run(
        "PgExactCache",
        Arc::new(|| {
            Box::pin(async {
                Arc::new(PgExactCache::new(isolated_pool().await)) as Arc<dyn ExactCache>
            })
        }),
    )
    .await;
}

#[tokio::test]
#[ignore = "needs the integration lane (deploy/integration-compose.yml)"]
async fn another_instance_reads_what_this_one_wrote() {
    // The entire reason M11.10 exists (docs/11 M11.10: "replacing the in-memory one where a
    // deployment has a database"). Two `PgExactCache` values over one schema stand in for two
    // replicas; with the in-memory cache this is a guaranteed miss.
    let pool = isolated_pool().await;
    let writer = PgExactCache::new(pool.clone());
    let reader = PgExactCache::new(pool);

    writer
        .put(
            key("shared"),
            response("written by one"),
            Duration::from_secs(60),
        )
        .await;
    let got = reader.get(&key("shared")).await.expect("the other replica");
    assert_eq!(
        format!("{:?}", got.items),
        format!("{:?}", response("written by one").items)
    );
}

#[tokio::test]
#[ignore = "needs the integration lane (deploy/integration-compose.yml)"]
async fn a_row_this_binary_cannot_read_is_a_miss_not_a_failure() {
    // Two ways a row becomes unreadable: a schema version this binary does not know, and items
    // that will not deserialize. Both happen during a rolling deploy, when two binaries share one
    // table — and neither may turn into a failed request, because the trait has nowhere to put an
    // error and the caller has a perfectly good provider to fall back to.
    let pool = isolated_pool().await;
    let cache = PgExactCache::new(pool.clone());

    run_sql(
        &pool,
        "INSERT INTO exact_cache (account_id, digest, schema_version, items, expires_at)
         VALUES ('00000000-0000-0000-0000-000000000007', 'from-the-future', 99,
                 '[]'::jsonb, now() + interval '1 hour')"
            .to_string(),
    )
    .await;
    assert!(
        cache.get(&key("from-the-future")).await.is_none(),
        "a future schema_version must read as a miss"
    );

    run_sql(
        &pool,
        "INSERT INTO exact_cache (account_id, digest, schema_version, items, expires_at)
         VALUES ('00000000-0000-0000-0000-000000000007', 'garbage', 1,
                 '[{\"type\":\"no_such_variant\"}]'::jsonb, now() + interval '1 hour')"
            .to_string(),
    )
    .await;
    assert!(
        cache.get(&key("garbage")).await.is_none(),
        "an undeserializable row must read as a miss, not panic or error"
    );
}

#[tokio::test]
#[ignore = "needs the integration lane (deploy/integration-compose.yml)"]
async fn a_closed_pool_is_a_miss_and_a_dropped_write() {
    // The contract's hardest clause: an unreachable database costs a provider call, never a
    // failed request. Asserted by closing the pool, which is the cheapest honest outage.
    let pool = isolated_pool().await;
    let cache = PgExactCache::new(pool.clone());
    pool.close().await;

    assert!(cache.get(&key("anything")).await.is_none());
    // Must not panic. There is nothing to assert afterwards — the point is that it returns.
    cache
        .put(
            key("anything"),
            response("dropped"),
            Duration::from_secs(60),
        )
        .await;
}

#[tokio::test]
#[ignore = "needs the integration lane (deploy/integration-compose.yml)"]
async fn the_reaper_deletes_expired_rows_and_only_those() {
    // Read-time filtering keeps stale rows from being served; it does not keep them from
    // accumulating, and an unlogged table with no reaper grows until the disk does.
    let pool = isolated_pool().await;
    let cache = PgExactCache::new(pool.clone());

    cache
        .put(key("live"), response("keep"), Duration::from_secs(3600))
        .await;
    cache
        .put(key("dead"), response("sweep"), Duration::ZERO)
        .await;

    let removed = reap(&pool).await.expect("reap");
    assert_eq!(removed, 1, "exactly the expired row");
    assert!(
        cache.get(&key("live")).await.is_some(),
        "the live row survived"
    );

    let remaining: (i64,) = sqlx::query_as("SELECT count(*) FROM exact_cache")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(remaining.0, 1);
}

#[tokio::test]
#[ignore = "needs the integration lane (deploy/integration-compose.yml)"]
async fn the_migration_set_applies_and_includes_the_cache() {
    let pool = pg::connect(&url()).await.expect("connect");
    let applied = pg::migrate(&pool, &pg::migrations_dir())
        .await
        .expect("migrate");
    assert!(
        applied.iter().any(|m| m.contains("0010_exact_cache")),
        "0010 must be in the migration set, not a file only this test knows about: {applied:?}"
    );

    // Unlogged is not cosmetic: it is why the table may vanish after an unclean shutdown, which
    // docs/22 tells operators to expect. A future edit dropping the keyword would be silent.
    let (persistence,): (String,) =
        sqlx::query_as("SELECT relpersistence::text FROM pg_class WHERE relname = 'exact_cache'")
            .fetch_one(&pool)
            .await
            .expect("pg_class");
    assert_eq!(persistence, "u", "exact_cache must be UNLOGGED");
}
