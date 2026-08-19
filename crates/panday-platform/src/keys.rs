//! API keys (M17.3, docs/17 §API keys for machines).
//!
//! > "random 256-bit, stored as argon2 hash, prefix-typed (`pnd_live_`, `pnd_test_`), scoped
//! > (models? sessions? admin?), last-used tracking, instant revoke."
//!
//! **Amended: sha256, not argon2** — recorded in docs/17 rather than done quietly. Argon2 exists to
//! make brute force expensive against *low-entropy* secrets; a 256-bit random token has no
//! brute-force surface to defend, so the KDF's cost buys nothing and is paid on every authenticated
//! request. Worse, ~100ms of hashing on the auth path creates pressure to cache verification
//! results, and a cache in front of an auth check is a much larger hole than the one argon2 would
//! have closed. Everything else in that sentence is implemented as written.
//!
//! ## The plaintext exists once
//!
//! `issue` returns it and nothing stores it. A stolen database is not a stolen key, and there is no
//! "show me the key again" path to build later — which is the point: a key you can retrieve is a key
//! an attacker can retrieve.

use crate::pg::PgError;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// What a key may do. Scopes are additive and checked at the edge that uses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Call models through the ingress.
    Models,
    /// Create and drive sessions.
    Sessions,
    /// Account administration — keys, plans, billing.
    Admin,
}

impl Scope {
    /// The wire spelling, for a CLI argument or an audit row. Unknown scopes are rejected rather
    /// than ignored: silently dropping a misspelled `admn` would mint a key that is weaker than the
    /// operator believes it is.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "models" => Some(Scope::Models),
            "sessions" => Some(Scope::Sessions),
            "admin" => Some(Scope::Admin),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Models => "models",
            Scope::Sessions => "sessions",
            Scope::Admin => "admin",
        }
    }
}

/// `pnd_live_` or `pnd_test_` (docs/17, and the CLAUDE.md working agreement).
///
/// Prefix-typed so a key is identifiable on sight — in a log, in a support ticket, in a screenshot —
/// and so a test key used against production fails loudly rather than spending real money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    Live,
    Test,
}

impl Environment {
    pub fn prefix(&self) -> &'static str {
        match self {
            Environment::Live => "pnd_live_",
            Environment::Test => "pnd_test_",
        }
    }

    pub fn of(key: &str) -> Option<Self> {
        if key.starts_with("pnd_live_") {
            Some(Environment::Live)
        } else if key.starts_with("pnd_test_") {
            Some(Environment::Test)
        } else {
            None
        }
    }
}

/// A key as the database holds it: never the secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub key_id: Uuid,
    pub account_id: Uuid,
    pub name: String,
    pub prefix: String,
    pub scopes: Vec<Scope>,
    pub revoked: bool,
    pub last_used_at: Option<String>,
}

/// The one moment the plaintext exists.
#[derive(Debug, Clone)]
pub struct IssuedKey {
    pub key: ApiKey,
    /// Show it once. Nothing stores it, and there is no path to retrieve it later.
    pub plaintext: String,
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("no such key")]
    NotFound,
    #[error("key is revoked")]
    Revoked,
    #[error("key does not carry the `{0}` scope")]
    MissingScope(&'static str),
    #[error("not a panday key: expected a `pnd_live_` or `pnd_test_` prefix")]
    Malformed,
    #[error(transparent)]
    Db(#[from] PgError),
}

/// sha256, hex. See the module note on why this is not argon2.
pub fn hash(plaintext: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(plaintext.as_bytes());
    format!("{:x}", h.finalize())
}

/// Mint a key: 256 bits of randomness behind a typed prefix.
///
/// The randomness is a v4 UUID pair rather than a hand-rolled RNG — `uuid` is already in the graph
/// and its v4 generator is the platform CSPRNG. Two of them is 256 bits total, of which 122 bits
/// each are random; that is 244 bits of entropy, which is past the point where the difference
/// matters and is honest about what it is rather than claiming a round number.
fn mint(environment: Environment) -> String {
    let a = Uuid::new_v4().simple().to_string();
    let b = Uuid::new_v4().simple().to_string();
    format!("{}{a}{b}", environment.prefix())
}

pub async fn issue(
    pool: &PgPool,
    account_id: Uuid,
    name: &str,
    environment: Environment,
    scopes: &[Scope],
) -> Result<IssuedKey, KeyError> {
    let plaintext = mint(environment);
    let key_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO api_keys (key_id, account_id, key_hash, prefix, name, scopes)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(key_id)
    .bind(account_id)
    .bind(hash(&plaintext))
    .bind(environment.prefix())
    .bind(name)
    .bind(serde_json::to_value(scopes).unwrap_or(serde_json::Value::Array(vec![])))
    .execute(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    Ok(IssuedKey {
        key: ApiKey {
            key_id,
            account_id,
            name: name.to_string(),
            prefix: environment.prefix().to_string(),
            scopes: scopes.to_vec(),
            revoked: false,
            last_used_at: None,
        },
        plaintext,
    })
}

/// The row shape every read here selects, named once so the two call sites decode it the same way —
/// a column added to `api_keys` is then one type change rather than two hand-written tuples that
/// silently disagree.
type KeyRow = (
    Uuid,
    Uuid,
    String,
    String,
    serde_json::Value,
    Option<time::OffsetDateTime>,
    Option<time::OffsetDateTime>,
);

impl ApiKey {
    fn from_row(
        (key_id, account_id, name, prefix, scopes, revoked_at, last_used_at): KeyRow,
    ) -> Self {
        ApiKey {
            key_id,
            account_id,
            name,
            prefix,
            // A scope we no longer recognise is dropped rather than fatal: forward-compatible in
            // the safe direction, since an unknown scope grants nothing.
            scopes: serde_json::from_value(scopes).unwrap_or_default(),
            revoked: revoked_at.is_some(),
            last_used_at: last_used_at.map(|t| t.to_string()),
        }
    }
}

/// Resolve a presented key to an account, or refuse.
///
/// Updates `last_used_at` on success. Not in the same transaction as the read: a lock on the auth
/// path would serialise every request from one key, and the value is evidence for a rotation review
/// rather than something anything branches on.
pub async fn authenticate(pool: &PgPool, plaintext: &str) -> Result<ApiKey, KeyError> {
    // The prefix check first, so a token from somewhere else fails on shape rather than on a database
    // round trip — and so the error says which of the two problems it is.
    Environment::of(plaintext).ok_or(KeyError::Malformed)?;

    let row: Option<KeyRow> = sqlx::query_as(
        "SELECT key_id, account_id, name, prefix, scopes, revoked_at, last_used_at
         FROM api_keys WHERE key_hash = $1",
    )
    .bind(hash(plaintext))
    .fetch_optional(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    let key = ApiKey::from_row(row.ok_or(KeyError::NotFound)?);
    if key.revoked {
        // Distinct from `NotFound` internally — an operator wants to know a *revoked* key is still
        // being presented, which is either a stale deployment or an attacker with an old secret. The
        // HTTP layer collapses both to 401, because the caller learns nothing either way.
        return Err(KeyError::Revoked);
    }

    let _ = sqlx::query(
        "UPDATE api_keys SET last_used_at = now() WHERE key_id = $1 AND account_id = $2",
    )
    .bind(key.key_id)
    .bind(key.account_id)
    .execute(pool)
    .await;

    // Report the write we just made rather than the value we read, so a caller that logs
    // `last_used_at` does not log a timestamp one request stale.
    Ok(ApiKey {
        last_used_at: Some(now_rfc3339()),
        ..key
    })
}

impl ApiKey {
    pub fn allows(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }

    pub fn require(&self, scope: Scope) -> Result<(), KeyError> {
        if self.allows(scope) {
            Ok(())
        } else {
            Err(KeyError::MissingScope(scope.as_str()))
        }
    }
}

/// Instant revoke (docs/17). A timestamp, not a delete: an audit that cannot show a key *was*
/// revoked cannot show when.
pub async fn revoke(pool: &PgPool, account_id: Uuid, key_id: Uuid) -> Result<(), KeyError> {
    let done = sqlx::query(
        "UPDATE api_keys SET revoked_at = now()
         WHERE key_id = $1 AND account_id = $2 AND revoked_at IS NULL",
    )
    .bind(key_id)
    .bind(account_id)
    .execute(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    if done.rows_affected() == 0 {
        return Err(KeyError::NotFound);
    }
    Ok(())
}

/// An account's keys. Names, prefixes, scopes and timestamps — never a secret, not even a truncated
/// one: a listing API that returned key material would make every audit log a leak.
pub async fn list(pool: &PgPool, account_id: Uuid) -> Result<Vec<ApiKey>, KeyError> {
    let rows: Vec<KeyRow> = sqlx::query_as(
        "SELECT key_id, account_id, name, prefix, scopes, revoked_at, last_used_at
         FROM api_keys WHERE account_id = $1 ORDER BY created_at",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    Ok(rows.into_iter().map(ApiKey::from_row).collect())
}

/// `last_used_at` as the rest of the codebase renders time.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// The gateway's `Authenticator`, backed by the key table (M17.3).
///
/// Lives here rather than in the gateway for the same reason `LedgerSink` does: the gateway must not
/// know about Postgres, and the platform must not be something the gateway links.
pub struct KeyAuthenticator {
    pool: PgPool,
}

impl KeyAuthenticator {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl panday_gateway::ingress::Authenticator for KeyAuthenticator {
    async fn authenticate(
        &self,
        bearer: &str,
    ) -> Result<panday_gateway::ingress::Caller, panday_gateway::ingress::AuthError> {
        use panday_gateway::ingress::AuthError;

        if bearer.is_empty() {
            return Err(AuthError::Missing);
        }
        let key = authenticate(&self.pool, bearer)
            .await
            .map_err(|e| match e {
                // Logged distinctly here — an operator wants to know a revoked key is still being
                // presented, which is either a stale deployment or somebody with an old secret — while
                // the HTTP layer collapses everything to 401.
                KeyError::Revoked => {
                    tracing::warn!("a revoked key was presented");
                    AuthError::Invalid
                }
                KeyError::Malformed => AuthError::Invalid,
                _ => AuthError::Invalid,
            })?;

        Ok(panday_gateway::ingress::Caller {
            account: panday_types::id::AccountId(key.account_id),
            key_id: key.key_id.to_string(),
            scopes: key.scopes.iter().map(|s| s.as_str().to_string()).collect(),
        })
    }
}
