//! Postgres-backed vault (docs/25 M25.11). Ciphertext on a hosted database.
//!
//! Deliberately the same shape as [`super::sqlite`], statement for statement, so the
//! two can be read side by side and a divergence is visible rather than inferred.
//! The differences are only the ones Postgres forces: `$1` placeholders, `bytea`
//! instead of `BLOB`, `timestamptz` defaults instead of a formatted string, and
//! `uuid` as a real column type rather than text.
//!
//! **The envelope is untouched.** `seal`/`open`/`validate_put` and the AAD
//! (`id || provider || kind`) are shared with every other store, so a row written by
//! the laptop vault and one written here are the same bytes under the same key. That
//! is the whole point of M25.11 being a store rather than a format.
//!
//! **The KEK is not in this database and must never be.** On a hosted deployment
//! neither `~/.panday/master.key` nor the macOS Keychain exists, so
//! `PANDAY_VAULT_KEY` is the path — see `Kek::resolve`. Losing it makes every row
//! here permanently unreadable.
//!
//! The schema lives with the other platform migrations
//! (`crates/panday-platform/migrations/0009_credentials.sql`) rather than being
//! created on connect: a hosted database is migrated by the deployment, not by
//! whichever process happens to open it first.

use super::{
    open, seal, validate_put, CredentialId, CredentialMeta, CredentialStore, Kek, Kind, Secret,
    State, VaultError,
};
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

pub struct PgStore {
    kek: Kek,
    pool: PgPool,
}

impl PgStore {
    /// Wrap an existing pool. The caller owns connection setup and migration —
    /// this store does not create its table.
    pub fn new(pool: PgPool, kek: Kek) -> Self {
        Self { kek, pool }
    }

    async fn list_one(&self, id: &CredentialId) -> Result<CredentialMeta, VaultError> {
        let row = sqlx::query(
            "SELECT id, provider, kind, label, last4, state, ceiling, window_secs
             FROM credentials WHERE id = $1",
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VaultError::Io(e.to_string()))?
        .ok_or(VaultError::NotFound)?;
        meta_from_row(&row)
    }
}

#[async_trait::async_trait]
impl CredentialStore for PgStore {
    async fn put(&self, mut meta: CredentialMeta, secret: &str) -> Result<(), VaultError> {
        validate_put(&mut meta, secret)?;
        let (nonce, ciphertext) = seal(&self.kek, &meta, secret)?;
        let res = sqlx::query(
            "INSERT INTO credentials
                (id, provider, kind, label, last4, state, nonce, ciphertext, ceiling, window_secs)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(meta.id.0)
        .bind(&meta.provider)
        .bind(meta.kind.as_str())
        .bind(&meta.label)
        .bind(&meta.last4)
        .bind(meta.state.as_str())
        .bind(&nonce)
        .bind(&ciphertext)
        .bind(meta.ceiling.map(|v| v as i64))
        .bind(meta.window_secs.map(|v| v as i64))
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                Err(VaultError::AlreadyExists)
            }
            Err(e) => Err(VaultError::Io(e.to_string())),
        }
    }

    async fn get_secret(&self, id: &CredentialId) -> Result<Secret, VaultError> {
        let row = sqlx::query(
            "SELECT provider, kind, label, last4, state, nonce, ciphertext
             FROM credentials WHERE id = $1",
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VaultError::Io(e.to_string()))?
        .ok_or(VaultError::NotFound)?;

        let meta = CredentialMeta {
            id: *id,
            provider: row.get("provider"),
            kind: Kind::parse(row.get("kind")).ok_or_else(|| VaultError::Io("bad kind".into()))?,
            label: row.get("label"),
            last4: row.get("last4"),
            state: State::parse(row.get("state"))
                .ok_or_else(|| VaultError::Io("bad state".into()))?,
            // Not selected and not needed: the AAD is `id || provider || kind`, so a
            // grant cannot affect whether this row decrypts.
            ceiling: None,
            window_secs: None,
        };
        if meta.state == State::Revoked {
            return Err(VaultError::Revoked);
        }
        let nonce: Vec<u8> = row.get("nonce");
        let ciphertext: Vec<u8> = row.get("ciphertext");
        open(&self.kek, &meta, &nonce, &ciphertext)
    }

    async fn list(&self) -> Result<Vec<CredentialMeta>, VaultError> {
        let rows = sqlx::query(
            "SELECT id, provider, kind, label, last4, state, ceiling, window_secs
             FROM credentials ORDER BY label",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| VaultError::Io(e.to_string()))?;
        rows.iter().map(meta_from_row).collect()
    }

    async fn set_state(&self, id: &CredentialId, state: State) -> Result<(), VaultError> {
        if state == State::Revoked {
            return Err(VaultError::InvalidMeta);
        }
        let current = self.list_one(id).await?;
        if current.state == State::Revoked {
            return Err(VaultError::Revoked);
        }
        let n = sqlx::query("UPDATE credentials SET state = $1, updated_at = now() WHERE id = $2")
            .bind(state.as_str())
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| VaultError::Io(e.to_string()))?;
        if n.rows_affected() == 0 {
            Err(VaultError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn set_grant(
        &self,
        id: &CredentialId,
        ceiling: Option<u64>,
        window_secs: Option<u64>,
    ) -> Result<(), VaultError> {
        // Touches only the two bookkeeping columns; the ciphertext is not read,
        // not rewritten, and not resealed.
        let n = sqlx::query(
            "UPDATE credentials SET ceiling = $1, window_secs = $2, updated_at = now()
             WHERE id = $3",
        )
        .bind(ceiling.map(|v| v as i64))
        .bind(window_secs.map(|v| v as i64))
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| VaultError::Io(e.to_string()))?;
        if n.rows_affected() == 0 {
            Err(VaultError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn revoke(&self, id: &CredentialId) -> Result<(), VaultError> {
        let n = sqlx::query(
            "UPDATE credentials
             SET ciphertext = ''::bytea, nonce = ''::bytea, state = 'revoked', updated_at = now()
             WHERE id = $1",
        )
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| VaultError::Io(e.to_string()))?;
        if n.rows_affected() == 0 {
            Err(VaultError::NotFound)
        } else {
            Ok(())
        }
    }
}

fn meta_from_row(row: &sqlx::postgres::PgRow) -> Result<CredentialMeta, VaultError> {
    let id: Uuid = row.get("id");
    let kind: String = row.get("kind");
    let state: String = row.get("state");
    let ceiling: Option<i64> = row.get("ceiling");
    let window_secs: Option<i64> = row.get("window_secs");
    Ok(CredentialMeta {
        id: CredentialId(id),
        provider: row.get("provider"),
        kind: Kind::parse(&kind).ok_or_else(|| VaultError::Io(format!("bad kind {kind}")))?,
        label: row.get("label"),
        last4: row.get("last4"),
        state: State::parse(&state).ok_or_else(|| VaultError::Io(format!("bad state {state}")))?,
        // Negative is not a ceiling. A hand-edited row must not become a silently
        // huge one via `as u64`.
        ceiling: ceiling.and_then(|v| u64::try_from(v).ok()),
        window_secs: window_secs.and_then(|v| u64::try_from(v).ok()),
    })
}
