//! SQLite-backed vault. Ciphertext on disk; KEK is not in this file.

use super::{
    open, seal, validate_put, CredentialId, CredentialMeta, CredentialStore, Kek, Kind, Secret,
    State, VaultError,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::path::Path;
use std::str::FromStr;
use time::OffsetDateTime;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS credentials (
    id         TEXT PRIMARY KEY NOT NULL,
    provider   TEXT NOT NULL,
    kind       TEXT NOT NULL,
    label      TEXT NOT NULL,
    last4      TEXT NOT NULL,
    state      TEXT NOT NULL,
    nonce      BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

pub struct SqliteStore {
    kek: Kek,
    pool: SqlitePool,
}

impl SqliteStore {
    pub async fn open(path: impl AsRef<Path>, kek: Kek) -> Result<Self, VaultError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| VaultError::Io(e.to_string()))?;
            }
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .map_err(|e| VaultError::Io(e.to_string()))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Full);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|e| VaultError::Io(e.to_string()))?;
        sqlx::query(SCHEMA)
            .execute(&pool)
            .await
            .map_err(|e| VaultError::Io(e.to_string()))?;
        Ok(Self { kek, pool })
    }

    /// Empty vault in RAM. Tests.
    pub async fn in_memory(kek: Kek) -> Result<Self, VaultError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .map_err(|e| VaultError::Io(e.to_string()))?;
        sqlx::query(SCHEMA)
            .execute(&pool)
            .await
            .map_err(|e| VaultError::Io(e.to_string()))?;
        Ok(Self { kek, pool })
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

fn meta_from_row(
    id: String,
    provider: String,
    kind: String,
    label: String,
    last4: String,
    state: String,
) -> Result<CredentialMeta, VaultError> {
    Ok(CredentialMeta {
        id: id
            .parse()
            .map_err(|e: uuid::Error| VaultError::Io(e.to_string()))?,
        provider,
        kind: Kind::parse(&kind).ok_or_else(|| VaultError::Io(format!("bad kind {kind}")))?,
        label,
        last4,
        state: State::parse(&state).ok_or_else(|| VaultError::Io(format!("bad state {state}")))?,
    })
}

#[async_trait::async_trait]
impl CredentialStore for SqliteStore {
    async fn put(&self, mut meta: CredentialMeta, secret: &str) -> Result<(), VaultError> {
        validate_put(&mut meta, secret)?;
        let (nonce, ciphertext) = seal(&self.kek, &meta, secret)?;
        let now = now_rfc3339();
        let res = sqlx::query(
            "INSERT INTO credentials
                (id, provider, kind, label, last4, state, nonce, ciphertext, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(meta.id.to_string())
        .bind(&meta.provider)
        .bind(meta.kind.as_str())
        .bind(&meta.label)
        .bind(&meta.last4)
        .bind(meta.state.as_str())
        .bind(&nonce)
        .bind(&ciphertext)
        .bind(&now)
        .bind(&now)
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
             FROM credentials WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VaultError::Io(e.to_string()))?
        .ok_or(VaultError::NotFound)?;
        let meta = meta_from_row(
            id.to_string(),
            row.get("provider"),
            row.get("kind"),
            row.get("label"),
            row.get("last4"),
            row.get("state"),
        )?;
        if meta.state == State::Revoked {
            return Err(VaultError::Revoked);
        }
        let nonce: Vec<u8> = row.get("nonce");
        let ciphertext: Vec<u8> = row.get("ciphertext");
        open(&self.kek, &meta, &nonce, &ciphertext)
    }

    async fn list(&self) -> Result<Vec<CredentialMeta>, VaultError> {
        let rows = sqlx::query(
            "SELECT id, provider, kind, label, last4, state FROM credentials ORDER BY label",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| VaultError::Io(e.to_string()))?;
        rows.into_iter()
            .map(|row| {
                meta_from_row(
                    row.get("id"),
                    row.get("provider"),
                    row.get("kind"),
                    row.get("label"),
                    row.get("last4"),
                    row.get("state"),
                )
            })
            .collect()
    }

    async fn set_state(&self, id: &CredentialId, state: State) -> Result<(), VaultError> {
        if state == State::Revoked {
            return Err(VaultError::InvalidMeta);
        }
        let current = self.list_one(id).await?;
        if current.state == State::Revoked {
            return Err(VaultError::Revoked);
        }
        let n = sqlx::query("UPDATE credentials SET state = ?, updated_at = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(now_rfc3339())
            .bind(id.to_string())
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
             SET ciphertext = x'', nonce = x'', state = 'revoked', updated_at = ?
             WHERE id = ?",
        )
        .bind(now_rfc3339())
        .bind(id.to_string())
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

impl SqliteStore {
    async fn list_one(&self, id: &CredentialId) -> Result<CredentialMeta, VaultError> {
        let row = sqlx::query(
            "SELECT id, provider, kind, label, last4, state FROM credentials WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| VaultError::Io(e.to_string()))?
        .ok_or(VaultError::NotFound)?;
        meta_from_row(
            row.get("id"),
            row.get("provider"),
            row.get("kind"),
            row.get("label"),
            row.get("last4"),
            row.get("state"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{last4, CredentialStore, Kind, State};

    const A: &str = "sk-test-aaaa";

    fn meta(provider: &str) -> CredentialMeta {
        CredentialMeta {
            id: CredentialId::new(),
            provider: provider.into(),
            kind: Kind::ApiKey,
            label: String::new(),
            last4: String::new(),
            state: State::Active,
        }
    }

    #[tokio::test]
    async fn missing_file_is_an_empty_pool() {
        let dir = std::env::temp_dir().join(format!("panday-vault-db-{}", uuid::Uuid::new_v4()));
        let path = dir.join("credentials.sqlite");
        let store = SqliteStore::open(&path, Kek::generate()).await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sqlite_round_trip_and_last4_is_clear() {
        let store = SqliteStore::in_memory(Kek::generate()).await.unwrap();
        let m = meta("xai");
        let id = m.id;
        store.put(m, A).await.unwrap();
        let listed = store.list().await.unwrap();
        assert_eq!(listed[0].last4, last4(A));
        assert_eq!(store.get_secret(&id).await.unwrap().expose(), A);
        let dump = format!("{:?}", listed[0]);
        assert!(!dump.contains(A), "list/debug must not contain the secret");
    }

    #[tokio::test]
    async fn sqlite_revoke_wipes_and_wrong_kek_fails() {
        let kek_a = Kek::generate();
        let kek_b = Kek::generate();
        let store = SqliteStore::in_memory(kek_a.clone()).await.unwrap();
        let m = meta("anthropic");
        let id = m.id;
        store.put(m, A).await.unwrap();

        let other = SqliteStore {
            kek: kek_b,
            pool: store.pool.clone(),
        };
        assert!(matches!(
            other.get_secret(&id).await.unwrap_err(),
            VaultError::Corrupt
        ));

        store.revoke(&id).await.unwrap();
        assert!(matches!(
            store.get_secret(&id).await.unwrap_err(),
            VaultError::Revoked
        ));
        assert_eq!(store.list().await.unwrap()[0].state, State::Revoked);
    }

    #[tokio::test]
    async fn duplicate_id_is_rejected() {
        let store = SqliteStore::in_memory(Kek::generate()).await.unwrap();
        let m = meta("openai");
        let again = m.clone();
        store.put(m, A).await.unwrap();
        assert!(matches!(
            store.put(again, A).await.unwrap_err(),
            VaultError::AlreadyExists
        ));
    }
}
