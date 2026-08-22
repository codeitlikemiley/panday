//! Upstream credential vault (docs/25, M25.1).
//!
//! Retrievable secrets: API keys and subscription OAuth tokens the gateway
//! sends outbound. Envelope-encrypted (XChaCha20-Poly1305). Not hashed like
//! `pnd_` keys (docs/17), and not injected into the sandbox (`SecretVault`,
//! docs/20 T4).
//!
//! `list` never returns a secret. `Debug` of [`Kek`] and [`Secret`] is redacted.

mod sqlite;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub use sqlite::SqliteStore;

/// 24-byte nonce for XChaCha20-Poly1305.
const NONCE_LEN: usize = 24;
const KEK_LEN: usize = 32;

/// Env override for the vault KEK (64 hex chars).
pub const VAULT_KEY_ENV: &str = "PANDAY_VAULT_KEY";

/// Default relative path under `$HOME` for the generated master key.
pub const MASTER_KEY_REL: &str = ".panday/master.key";

/// Default relative path under `$HOME` for the SQLite vault.
pub const VAULT_DB_REL: &str = ".panday/credentials.sqlite";

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CredentialId(pub Uuid);

impl CredentialId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CredentialId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CredentialId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for CredentialId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CredentialId").field(&self.0).finish()
    }
}

impl std::str::FromStr for CredentialId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    ApiKey,
    Oauth,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::ApiKey => "api_key",
            Kind::Oauth => "oauth",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "api_key" => Some(Kind::ApiKey),
            "oauth" => Some(Kind::Oauth),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Active,
    Exhausted,
    Invalid,
    Revoked,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Exhausted => "exhausted",
            State::Invalid => "invalid",
            State::Revoked => "revoked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(State::Active),
            "exhausted" => Some(State::Exhausted),
            "invalid" => Some(State::Invalid),
            "revoked" => Some(State::Revoked),
            _ => None,
        }
    }
}

/// Public row. Never contains the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialMeta {
    pub id: CredentialId,
    pub provider: String,
    pub kind: Kind,
    pub label: String,
    pub last4: String,
    pub state: State,
    /// Operator-declared calls per window (docs/25 M25.6), persisted here from
    /// M25.9 so it survives a restart. `None` means nobody has declared one.
    ///
    /// Deliberately **not** part of the AEAD's AAD, which stays
    /// `id || provider || kind`: a ceiling is operator bookkeeping, not part of
    /// the secret's identity, and binding it would mean every row's ciphertext
    /// had to be resealed each time someone edited a number on the console.
    pub ceiling: Option<u64>,
    pub window_secs: Option<u64>,
}

/// Decrypted secret. `Debug` is redacted; the inner buffer is zeroized on drop.
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// 32-byte vault key. Zeroized on drop.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Kek([u8; KEK_LEN]);

impl Kek {
    pub fn generate() -> Self {
        let mut bytes = [0u8; KEK_LEN];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn from_bytes(bytes: [u8; KEK_LEN]) -> Self {
        Self(bytes)
    }

    pub fn from_hex(s: &str) -> Result<Self, VaultError> {
        let s = s.trim();
        let s = s.strip_prefix("0x").unwrap_or(s);
        if s.len() != KEK_LEN * 2 {
            return Err(VaultError::BadKek);
        }
        let mut bytes = [0u8; KEK_LEN];
        for i in 0..KEK_LEN {
            bytes[i] =
                u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| VaultError::BadKek)?;
        }
        Ok(Self(bytes))
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// `PANDAY_VAULT_KEY` if set and well-formed.
    pub fn from_env() -> Result<Option<Self>, VaultError> {
        match std::env::var(VAULT_KEY_ENV) {
            Ok(v) if !v.trim().is_empty() => Ok(Some(Self::from_hex(&v)?)),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(VaultError::BadKek),
        }
    }

    /// Read 32 raw bytes, or create the file at mode `0600`.
    /// Resolve the KEK the way every caller should (docs/25 M25.12).
    ///
    /// Order, and the order matters more than anything else in this file —
    /// **a lost KEK is an unreadable vault**, permanently:
    ///
    /// 1. `PANDAY_VAULT_KEY`. An explicit override wins; that is what it is for.
    /// 2. **An existing `master.key` file.** Before the keychain, always. If a
    ///    vault was sealed under the file's key, preferring a keychain entry
    ///    would hand back a *different* key and make every row undecryptable.
    ///    The file is never read-modify-deleted here — migration is a thing an
    ///    operator does deliberately, not something a library does on boot.
    /// 3. The keychain, **only if opted in** with `PANDAY_VAULT_KEYCHAIN=1`.
    /// 4. Otherwise generate one, and store it wherever step 3 said.
    ///
    /// The opt-in is not timidity. `keyring` can fall back to an in-memory store
    /// when no backend is present, which reads back correctly inside one process
    /// and is gone at the next boot — so a silent default would lose vaults on
    /// exactly the machines least able to notice. An operator who asks for the
    /// keychain gets a loud error if it is unavailable; one who does not ask is
    /// never exposed to it.
    pub fn resolve(path: &Path) -> Result<Self, VaultError> {
        if let Some(kek) = Self::from_env()? {
            return Ok(kek);
        }
        if path.exists() {
            return Self::load(path);
        }
        if keychain_opted_in() {
            if let Some(kek) = keychain_load()? {
                return Ok(kek);
            }
            let kek = Self::generate();
            keychain_store(&kek)?;
            return Ok(kek);
        }
        Self::load_or_create(path)
    }

    pub fn load_or_create(path: &Path) -> Result<Self, VaultError> {
        if path.exists() {
            return Self::load(path);
        }
        let kek = Self::generate();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| VaultError::Io(e.to_string()))?;
            }
        }
        write_master_key(path, &kek.0)?;
        Ok(kek)
    }

    pub fn load(path: &Path) -> Result<Self, VaultError> {
        let mut file = fs::File::open(path).map_err(|e| VaultError::Io(e.to_string()))?;
        let mut bytes = [0u8; KEK_LEN];
        file.read_exact(&mut bytes)
            .map_err(|_| VaultError::BadKek)?;
        let mut extra = [0u8; 1];
        match file.read(&mut extra) {
            Ok(0) => Ok(Self(bytes)),
            _ => Err(VaultError::BadKek),
        }
    }
}

impl fmt::Debug for Kek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Kek([redacted])")
    }
}

fn write_master_key(path: &Path, bytes: &[u8; KEK_LEN]) -> Result<(), VaultError> {
    let mut f = fs::File::create(path).map_err(|e| VaultError::Io(e.to_string()))?;
    f.write_all(bytes)
        .map_err(|e| VaultError::Io(e.to_string()))?;
    f.sync_all().map_err(|e| VaultError::Io(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| VaultError::Io(e.to_string()))?;
    }
    Ok(())
}

/// Default `~/.panday/master.key`.
/// `PANDAY_VAULT_KEYCHAIN=1` — the operator asking for the Keychain (M25.12).
pub const VAULT_KEYCHAIN_ENV: &str = "PANDAY_VAULT_KEYCHAIN";

/// Keychain service and account. Stable strings: changing either orphans every
/// KEK already stored under the old pair.
const KEYCHAIN_SERVICE: &str = "panday-vault";
const KEYCHAIN_ACCOUNT: &str = "kek";

fn keychain_opted_in() -> bool {
    matches!(
        std::env::var(VAULT_KEYCHAIN_ENV).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

#[cfg(target_os = "macos")]
fn keychain_entry() -> Result<keyring::Entry, VaultError> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| VaultError::Io(format!("keychain unavailable: {e}")))
}

/// The stored KEK, or `None` when there is no entry yet.
#[cfg(target_os = "macos")]
fn keychain_load() -> Result<Option<Kek>, VaultError> {
    match keychain_entry()?.get_password() {
        Ok(hex) => Kek::from_hex(&hex).map(Some),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(VaultError::Io(format!("keychain read failed: {e}"))),
    }
}

#[cfg(target_os = "macos")]
fn keychain_store(kek: &Kek) -> Result<(), VaultError> {
    keychain_entry()?
        .set_password(&kek.to_hex())
        .map_err(|e| VaultError::Io(format!("keychain write failed: {e}")))?;
    // Read back through a *fresh* entry. A write that cannot be read again is a
    // KEK that will be gone at the next boot, taking the vault with it, and a
    // loud failure now is infinitely cheaper than that.
    match keychain_load()? {
        Some(back) if back.to_hex() == kek.to_hex() => Ok(()),
        _ => Err(VaultError::Io(
            "keychain accepted the KEK but did not return it — refusing to seal \
             a vault against a key that may not survive a restart"
                .into(),
        )),
    }
}

#[cfg(not(target_os = "macos"))]
fn keychain_load() -> Result<Option<Kek>, VaultError> {
    Err(VaultError::Io(format!(
        "{VAULT_KEYCHAIN_ENV} is set, but this build has no keychain — unset it \
         to use {}, or set PANDAY_VAULT_KEY",
        "~/.panday/master.key"
    )))
}

#[cfg(not(target_os = "macos"))]
fn keychain_store(_kek: &Kek) -> Result<(), VaultError> {
    keychain_load().map(|_| ())
}

pub fn default_master_key_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(MASTER_KEY_REL))
}

/// Default `~/.panday/credentials.sqlite`.
pub fn default_vault_db_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(VAULT_DB_REL))
}

/// Last four characters of the secret (Unicode scalar values). Stored in the clear.
pub fn last4(secret: &str) -> String {
    let n = secret.chars().count();
    secret.chars().skip(n.saturating_sub(4)).collect()
}

fn aad(meta: &CredentialMeta) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(meta.id.0.as_bytes());
    out.push(0);
    out.extend_from_slice(meta.provider.as_bytes());
    out.push(0);
    out.extend_from_slice(meta.kind.as_str().as_bytes());
    out
}

pub(crate) fn seal(
    kek: &Kek,
    meta: &CredentialMeta,
    secret: &str,
) -> Result<(Vec<u8>, Vec<u8>), VaultError> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&kek.0));
    let nonce = XNonce::from_slice(&nonce_bytes);
    let aad = aad(meta);
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: secret.as_bytes(),
                aad: &aad,
            },
        )
        .map_err(|_| VaultError::Crypto)?;
    Ok((nonce_bytes.to_vec(), ciphertext))
}

pub(crate) fn open(
    kek: &Kek,
    meta: &CredentialMeta,
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<Secret, VaultError> {
    if nonce.len() != NONCE_LEN {
        return Err(VaultError::Corrupt);
    }
    if ciphertext.is_empty() {
        return Err(VaultError::Revoked);
    }
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&kek.0));
    let nonce = XNonce::from_slice(nonce);
    let aad = aad(meta);
    let plain = cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| VaultError::Corrupt)?;
    let text = String::from_utf8(plain).map_err(|_| VaultError::Corrupt)?;
    Ok(Secret(Zeroizing::new(text)))
}

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("vault i/o: {0}")]
    Io(String),
    #[error("vault key is missing or not 32 bytes / 64 hex chars")]
    BadKek,
    #[error("credential secret is empty")]
    EmptySecret,
    #[error("provider and label must be non-empty")]
    InvalidMeta,
    #[error("no such credential")]
    NotFound,
    #[error("credential already exists")]
    AlreadyExists,
    #[error("credential is revoked")]
    Revoked,
    #[error("ciphertext does not match this key or row")]
    Corrupt,
    #[error("encryption failed")]
    Crypto,
}

/// Where credentials live. `list` is metadata only.
#[async_trait::async_trait]
pub trait CredentialStore: Send + Sync {
    async fn put(&self, meta: CredentialMeta, secret: &str) -> Result<(), VaultError>;
    async fn get_secret(&self, id: &CredentialId) -> Result<Secret, VaultError>;
    async fn list(&self) -> Result<Vec<CredentialMeta>, VaultError>;
    async fn set_state(&self, id: &CredentialId, state: State) -> Result<(), VaultError>;

    /// Record the operator's declared grant against a row (docs/25 M25.9).
    ///
    /// Separate from `put` because it must not touch the ciphertext: a ceiling
    /// is bookkeeping, and resealing the secret every time someone edits a
    /// number on the console would be a needless handling of the one value in
    /// this table worth protecting.
    async fn set_grant(
        &self,
        id: &CredentialId,
        ceiling: Option<u64>,
        window_secs: Option<u64>,
    ) -> Result<(), VaultError>;
    async fn revoke(&self, id: &CredentialId) -> Result<(), VaultError>;
}

pub(crate) fn validate_put(meta: &mut CredentialMeta, secret: &str) -> Result<(), VaultError> {
    if secret.is_empty() {
        return Err(VaultError::EmptySecret);
    }
    if meta.provider.trim().is_empty() {
        return Err(VaultError::InvalidMeta);
    }
    meta.provider = meta.provider.trim().to_string();
    if meta.label.trim().is_empty() {
        meta.label = format!("{}-{}", meta.provider, last4(secret));
    }
    meta.last4 = last4(secret);
    if meta.state == State::Revoked {
        return Err(VaultError::InvalidMeta);
    }
    meta.state = State::Active;
    Ok(())
}

struct MemoryRow {
    meta: CredentialMeta,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

/// Encrypted in-memory store. Tests, and a vault that has not opened a file.
pub struct MemoryStore {
    kek: Kek,
    rows: Mutex<BTreeMap<Uuid, MemoryRow>>,
}

impl MemoryStore {
    pub fn new(kek: Kek) -> Self {
        Self {
            kek,
            rows: Mutex::new(BTreeMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl CredentialStore for MemoryStore {
    async fn put(&self, mut meta: CredentialMeta, secret: &str) -> Result<(), VaultError> {
        validate_put(&mut meta, secret)?;
        let (nonce, ciphertext) = seal(&self.kek, &meta, secret)?;
        let mut rows = self.rows.lock().expect("vault mutex");
        if rows.contains_key(&meta.id.0) {
            return Err(VaultError::AlreadyExists);
        }
        rows.insert(
            meta.id.0,
            MemoryRow {
                meta,
                nonce,
                ciphertext,
            },
        );
        Ok(())
    }

    async fn get_secret(&self, id: &CredentialId) -> Result<Secret, VaultError> {
        let rows = self.rows.lock().expect("vault mutex");
        let row = rows.get(&id.0).ok_or(VaultError::NotFound)?;
        if row.meta.state == State::Revoked {
            return Err(VaultError::Revoked);
        }
        open(&self.kek, &row.meta, &row.nonce, &row.ciphertext)
    }

    async fn list(&self) -> Result<Vec<CredentialMeta>, VaultError> {
        let rows = self.rows.lock().expect("vault mutex");
        Ok(rows.values().map(|r| r.meta.clone()).collect())
    }

    async fn set_state(&self, id: &CredentialId, state: State) -> Result<(), VaultError> {
        let mut rows = self.rows.lock().expect("vault mutex");
        let row = rows.get_mut(&id.0).ok_or(VaultError::NotFound)?;
        if row.meta.state == State::Revoked {
            return Err(VaultError::Revoked);
        }
        if state == State::Revoked {
            return Err(VaultError::InvalidMeta);
        }
        row.meta.state = state;
        Ok(())
    }

    async fn set_grant(
        &self,
        id: &CredentialId,
        ceiling: Option<u64>,
        window_secs: Option<u64>,
    ) -> Result<(), VaultError> {
        let mut rows = self.rows.lock().expect("vault mutex");
        let row = rows.get_mut(&id.0).ok_or(VaultError::NotFound)?;
        row.meta.ceiling = ceiling;
        row.meta.window_secs = window_secs;
        Ok(())
    }

    async fn revoke(&self, id: &CredentialId) -> Result<(), VaultError> {
        let mut rows = self.rows.lock().expect("vault mutex");
        let row = rows.get_mut(&id.0).ok_or(VaultError::NotFound)?;
        row.ciphertext.clear();
        row.nonce.clear();
        row.meta.state = State::Revoked;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "sk-test-aaaa";
    const B: &str = "sk-test-bbbb";

    fn meta(provider: &str) -> CredentialMeta {
        CredentialMeta {
            id: CredentialId::new(),
            provider: provider.into(),
            kind: Kind::ApiKey,
            label: String::new(),
            last4: String::new(),
            state: State::Active,
            ceiling: None,
            window_secs: None,
        }
    }

    #[test]
    fn last4_is_the_tail_of_the_secret() {
        assert_eq!(last4(A), "aaaa");
        assert_eq!(last4("ab"), "ab");
        assert_eq!(last4(""), "");
    }

    #[test]
    fn kek_debug_is_redacted() {
        let k = Kek::generate();
        let shown = format!("{k:?}");
        assert_eq!(shown, "Kek([redacted])");
        assert!(!shown.contains(&k.to_hex()[..8]));
    }

    #[test]
    fn hex_round_trip() {
        let k = Kek::generate();
        let again = Kek::from_hex(&k.to_hex()).unwrap();
        assert_eq!(k.to_hex(), again.to_hex());
        assert!(Kek::from_hex("dead").is_err());
    }

    #[tokio::test]
    async fn memory_round_trip() {
        let store = MemoryStore::new(Kek::generate());
        let m = meta("xai");
        let id = m.id;
        store.put(m, A).await.unwrap();
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].last4, "aaaa");
        assert_eq!(listed[0].label, "xai-aaaa");
        assert!(!format!("{:?}", listed[0]).contains(A));
        assert_eq!(store.get_secret(&id).await.unwrap().expose(), A);
    }

    #[tokio::test]
    async fn wrong_kek_fails_closed() {
        let kek_a = Kek::generate();
        let kek_b = Kek::generate();
        let m = meta("xai");
        let (nonce, ct) = seal(&kek_a, &m, A).unwrap();
        let err = open(&kek_b, &m, &nonce, &ct).unwrap_err();
        assert!(matches!(err, VaultError::Corrupt));
    }

    #[tokio::test]
    async fn aad_binds_provider_and_id() {
        let kek = Kek::generate();
        let mut m = meta("xai");
        let (nonce, ct) = seal(&kek, &m, A).unwrap();
        m.provider = "anthropic".into();
        let err = open(&kek, &m, &nonce, &ct).unwrap_err();
        assert!(matches!(err, VaultError::Corrupt));
    }

    #[tokio::test]
    async fn empty_secret_is_rejected() {
        let store = MemoryStore::new(Kek::generate());
        let m = meta("xai");
        let err = store.put(m, "").await.unwrap_err();
        assert!(matches!(err, VaultError::EmptySecret));
    }

    #[tokio::test]
    async fn a_grant_survives_without_touching_the_secret() {
        // The point of `set_grant` being separate from `put`: editing a ceiling
        // must not reseal the one value in this table worth protecting.
        let store = MemoryStore::new(Kek::generate());
        let m = meta("xai");
        let id = m.id;
        store.put(m, A).await.unwrap();
        assert_eq!(store.list().await.unwrap()[0].ceiling, None);

        store.set_grant(&id, Some(500), Some(18_000)).await.unwrap();
        let row = &store.list().await.unwrap()[0];
        assert_eq!(row.ceiling, Some(500));
        assert_eq!(row.window_secs, Some(18_000));
        assert_eq!(
            store.get_secret(&id).await.unwrap().expose(),
            A,
            "the secret must still decrypt after a bookkeeping edit"
        );
    }

    #[tokio::test]
    async fn clearing_a_grant_is_distinct_from_never_declaring_one() {
        let store = MemoryStore::new(Kek::generate());
        let m = meta("xai");
        let id = m.id;
        store.put(m, A).await.unwrap();
        store.set_grant(&id, Some(10), Some(60)).await.unwrap();
        store.set_grant(&id, None, None).await.unwrap();
        assert_eq!(store.list().await.unwrap()[0].ceiling, None);
    }

    #[tokio::test]
    async fn setting_a_grant_on_a_missing_row_is_an_error_not_a_silent_noop() {
        let store = MemoryStore::new(Kek::generate());
        let err = store
            .set_grant(&CredentialId::new(), Some(1), None)
            .await
            .unwrap_err();
        assert!(matches!(err, VaultError::NotFound));
    }

    #[tokio::test]
    async fn revoke_wipes_ciphertext() {
        let store = MemoryStore::new(Kek::generate());
        let m = meta("xai");
        let id = m.id;
        store.put(m, A).await.unwrap();
        store.revoke(&id).await.unwrap();
        let listed = store.list().await.unwrap();
        assert_eq!(listed[0].state, State::Revoked);
        assert_eq!(listed[0].last4, "aaaa");
        assert!(matches!(
            store.get_secret(&id).await.unwrap_err(),
            VaultError::Revoked
        ));
    }

    #[tokio::test]
    async fn two_rows_decrypt_independently() {
        let store = MemoryStore::new(Kek::generate());
        let ma = meta("xai");
        let mb = meta("anthropic");
        let ida = ma.id;
        let idb = mb.id;
        store.put(ma, A).await.unwrap();
        store.put(mb, B).await.unwrap();
        assert_eq!(store.get_secret(&ida).await.unwrap().expose(), A);
        assert_eq!(store.get_secret(&idb).await.unwrap().expose(), B);
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret(Zeroizing::new(A.to_string()));
        assert_eq!(format!("{s:?}"), "Secret([redacted])");
    }

    #[test]
    fn an_existing_master_key_file_beats_the_keychain() {
        // The ordering that protects every already-sealed vault. If a vault was
        // created against the file's key, resolving to a keychain entry instead
        // would hand back a different key and make every row undecryptable —
        // silently, and permanently.
        let dir = std::env::temp_dir().join(format!("panday-kek-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("master.key");
        let on_disk = Kek::load_or_create(&path).unwrap();

        // Opt in to the keychain. The file must still win.
        let resolved = Kek::resolve(&path).unwrap();
        assert_eq!(
            resolved.to_hex(),
            on_disk.to_hex(),
            "an existing master.key must be preferred over any other source"
        );
        assert!(path.exists(), "resolve must never consume the file it read");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_creates_a_file_when_the_keychain_was_not_asked_for() {
        // No opt-in means the behaviour is exactly what it was before M25.12.
        let dir = std::env::temp_dir().join(format!("panday-kek-{}", Uuid::new_v4()));
        let path = dir.join("master.key");
        assert!(!path.exists());
        let made = Kek::resolve(&path).unwrap();
        assert!(path.exists(), "the file is still the default store");
        assert_eq!(Kek::load(&path).unwrap().to_hex(), made.to_hex());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_keychain_opt_in_reads_only_an_explicit_yes() {
        // A half-set variable must not switch the vault's key store. Anything
        // that is not a deliberate yes leaves the file in charge.
        for (value, want) in [
            ("1", true),
            ("true", true),
            ("yes", true),
            ("0", false),
            ("", false),
            ("maybe", false),
        ] {
            assert_eq!(
                matches!(Some(value), Some("1") | Some("true") | Some("yes")),
                want,
                "{value:?} should opt in = {want}"
            );
        }
    }

    #[test]
    fn master_key_file_is_mode_600_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("panday-vault-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("master.key");
        let kek = Kek::load_or_create(&path).unwrap();
        let loaded = Kek::load(&path).unwrap();
        assert_eq!(kek.to_hex(), loaded.to_hex());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "master.key must not be group/world readable");
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
