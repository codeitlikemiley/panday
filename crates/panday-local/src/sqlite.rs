//! The offline event store (M18.3, docs/18 §Composition: "harness with SQLite ... event
//! store").
//!
//! tenant-scoping: single-tenant — one laptop, one user, no accounts (docs/18: "no account
//! needed at all"). There is nothing to scope by but the session, and a constant
//! `account_id` column added to satisfy the M20.3 lint would be theatre. The boundary here
//! is the filesystem, which is the same boundary the user's source code has.
//!
//! ## Why SQLite and not the JSONL file
//!
//! `JsonlStore` (M21.3) is one file per session and a scan per read, which is right for a
//! debugging artifact and wrong for a laptop that accumulates months of sessions. SQLite
//! gives an index, a transaction per append, and one file for every session — and it needs
//! no server, which is the whole point of the offline tier.
//!
//! ## The schema is the invariant
//!
//! `PRIMARY KEY (session_id, seq)` is what makes a repeated `seq` an error rather than a
//! silent overwrite: two writers on one session is exactly what ADR-002's single-writer
//! rule forbids, and the database refuses it without our help. Gaplessness needs one more
//! check, because a database is happy to store 1 and 3.
//!
//! Events are stored as JSON text, not as columns. Two reasons: the protocol is versioned
//! by its own rules (docs/03) and unknown kinds must round-trip verbatim, which a column
//! layout cannot do; and a migration for every new event kind would make the protocol's
//! "additive fields are always ok" false in practice.

use panday_harness::{EventStore, StoreError};
use panday_types::event::Envelope;
use panday_types::SessionId;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating if absent) a database at `path`.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let options =
            SqliteConnectOptions::from_str(&format!("sqlite://{}", path.as_ref().display()))
                .map_err(|e| StoreError::Io(e.to_string()))?
                .create_if_missing(true)
                // Durable per append (docs/13 §persist-before-proceed). WAL plus FULL sync is
                // the combination that survives a laptop lid closing: WAL for the write
                // throughput, FULL because "the event is in the log" has to mean it is on the
                // disk, not in the page cache.
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .synchronous(sqlx::sqlite::SqliteSynchronous::Full);

        let pool = SqlitePoolOptions::new()
            // One connection: a session has one writer (ADR-002), and a pool that let two
            // tasks interleave writes would turn the single-writer invariant into a race
            // the database catches instead of a rule the code keeps.
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|e| StoreError::Io(e.to_string()))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events (
                 session_id TEXT NOT NULL,
                 seq        INTEGER NOT NULL,
                 at         TEXT NOT NULL,
                 envelope   TEXT NOT NULL,
                 PRIMARY KEY (session_id, seq)
             )",
        )
        .execute(&pool)
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?;

        Ok(Self { pool })
    }

    /// An in-memory database. For tests, and for a session someone explicitly does not
    /// want on disk.
    pub async fn in_memory() -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            // `:memory:` is per-connection, so a pool of one is also what keeps the
            // database from vanishing between queries.
            .connect("sqlite::memory:")
            .await
            .map_err(|e| StoreError::Io(e.to_string()))?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events (
                 session_id TEXT NOT NULL,
                 seq        INTEGER NOT NULL,
                 at         TEXT NOT NULL,
                 envelope   TEXT NOT NULL,
                 PRIMARY KEY (session_id, seq)
             )",
        )
        .execute(&pool)
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?;
        Ok(Self { pool })
    }

    /// Every session this database holds, oldest first.
    pub async fn sessions(&self) -> Result<Vec<SessionId>, StoreError> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT session_id FROM events GROUP BY session_id ORDER BY MIN(rowid)")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| StoreError::Io(e.to_string()))?;
        rows.into_iter()
            .map(|(id,)| {
                uuid::Uuid::parse_str(&id)
                    .map(SessionId)
                    .map_err(|e| StoreError::Io(format!("session id {id}: {e}")))
            })
            .collect()
    }

    /// Write one session out as JSONL, so `panday replay` can read it.
    ///
    /// The replay tool takes a log file (M21.3), and a session in a database is not one.
    /// Exporting rather than teaching the replay tool SQL keeps the tool's input a single
    /// portable artifact — which is what you want when the person debugging is not the
    /// person whose laptop it happened on.
    pub async fn export_jsonl(
        &self,
        session: SessionId,
        path: impl AsRef<Path>,
    ) -> Result<usize, StoreError> {
        use std::io::Write;
        let events = self.read_after(session, 0).await?;
        let mut file = std::fs::File::create(path.as_ref())
            .map_err(|e| StoreError::Io(format!("{}: {e}", path.as_ref().display())))?;
        for envelope in &events {
            let line = serde_json::to_string(envelope)
                .map_err(|e| StoreError::Io(format!("serialize: {e}")))?;
            writeln!(file, "{line}").map_err(|e| StoreError::Io(e.to_string()))?;
        }
        Ok(events.len())
    }
}

#[async_trait::async_trait]
impl EventStore for SqliteStore {
    async fn append(&self, e: Envelope) -> Result<(), StoreError> {
        // Gaplessness first, in the same transaction as the insert: checking outside one
        // would let two tasks both read `max=1` and both write 2 — and the primary key
        // would then reject the loser with a confusing error instead of this clear one.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Io(e.to_string()))?;

        let head: Option<(i64,)> =
            sqlx::query_as("SELECT MAX(seq) FROM events WHERE session_id = ?")
                .bind(e.session_id.0.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(|err| StoreError::Io(err.to_string()))?;
        let expected = head.map_or(1, |(max,)| max as u64 + 1);
        if e.seq != expected {
            return Err(StoreError::SeqConflict(e.seq));
        }

        let envelope = serde_json::to_string(&e).map_err(|err| StoreError::Io(err.to_string()))?;
        sqlx::query("INSERT INTO events (session_id, seq, at, envelope) VALUES (?, ?, ?, ?)")
            .bind(e.session_id.0.to_string())
            .bind(e.seq as i64)
            .bind(e.at.to_string())
            .bind(envelope)
            .execute(&mut *tx)
            .await
            .map_err(|err| match &err {
                // The primary key firing means someone else wrote this seq between our
                // check and our insert — the single-writer violation, reported as itself.
                sqlx::Error::Database(db) if db.is_unique_violation() => {
                    StoreError::SeqConflict(e.seq)
                }
                _ => StoreError::Io(err.to_string()),
            })?;

        tx.commit().await.map_err(|e| StoreError::Io(e.to_string()))
    }

    async fn read_after(
        &self,
        session: SessionId,
        after_seq: u64,
    ) -> Result<Vec<Envelope>, StoreError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT envelope FROM events WHERE session_id = ? AND seq > ? ORDER BY seq",
        )
        .bind(session.0.to_string())
        .bind(after_seq as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?;

        rows.into_iter()
            .map(|(json,)| {
                serde_json::from_str(&json)
                    .map_err(|e| StoreError::Io(format!("stored event: {e}")))
            })
            .collect()
    }

    async fn next_seq(&self, session: SessionId) -> Result<u64, StoreError> {
        let head: Option<(i64,)> =
            sqlx::query_as("SELECT MAX(seq) FROM events WHERE session_id = ?")
                .bind(session.0.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| StoreError::Io(e.to_string()))?;
        Ok(head.map_or(1, |(max,)| max as u64 + 1))
    }
}
