//! `panday creds` — operator outbound vault (docs/25 M25.2).
//!
//! Secrets enter the sealed store by stdin or by a read-only copy of an official
//! CLI login. They never appear on argv (`ps` would see them). Official stores
//! (`~/.grok/auth.json`, Claude Code Keychain / credentials.json, `~/.codex/auth.json`)
//! are never written.

use crate::{Command, CredsAction, CredsSource};
use panday_sdk::vault::{
    default_master_key_path, default_vault_db_path, CredentialId, CredentialMeta, CredentialStore,
    Kek, Kind, SqliteStore, State, VaultError,
};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Env override for the SQLite vault path. Default: `~/.panday/credentials.sqlite`.
pub const VAULT_DB_ENV: &str = "PANDAY_VAULT_DB";

/// Env override for the Codex auth file (tests). Default: `$CODEX_HOME/auth.json`
/// or `~/.codex/auth.json`.
pub const CODEX_AUTH_ENV: &str = "PANDAY_CODEX_AUTH";

/// Open the operator vault: `PANDAY_VAULT_KEY` else `~/.panday/master.key`;
/// `PANDAY_VAULT_DB` else `~/.panday/credentials.sqlite`.
pub async fn open_operator_vault() -> Result<SqliteStore, String> {
    let kek = match Kek::from_env().map_err(|e| e.to_string())? {
        Some(k) => k,
        None => {
            let path = default_master_key_path().ok_or(
                "cannot locate ~/.panday/master.key (HOME is unset); set HOME or PANDAY_VAULT_KEY",
            )?;
            Kek::load_or_create(&path).map_err(|e| e.to_string())?
        }
    };
    let db = std::env::var_os(VAULT_DB_ENV)
        .map(PathBuf::from)
        .or_else(default_vault_db_path)
        .ok_or(
            "cannot locate ~/.panday/credentials.sqlite (HOME is unset); set HOME or PANDAY_VAULT_DB",
        )?;
    SqliteStore::open(db, kek).await.map_err(|e| e.to_string())
}

/// Run a parsed `creds` command. `stdin` is only read for `add` without `--from-*`.
pub async fn run_creds<R: Read>(
    cmd: &Command,
    stdin: R,
    stdin_is_tty: bool,
) -> Result<String, String> {
    let Command::Creds(action) = cmd else {
        return Err("not a creds command".into());
    };
    let store = open_operator_vault().await?;
    match action {
        CredsAction::List => {
            let rows = store.list().await.map_err(|e| e.to_string())?;
            Ok(format_list(&rows))
        }
        CredsAction::Revoke { id } => revoke_credential(&store, id).await,
        CredsAction::Add {
            provider,
            label,
            kind,
            source,
        } => {
            let meta = match source {
                CredsSource::Stdin => {
                    add_from_stdin(
                        &store,
                        stdin,
                        stdin_is_tty,
                        provider,
                        *kind,
                        label.as_deref(),
                    )
                    .await?
                }
                CredsSource::Grok => add_from_grok(&store, label.as_deref()).await?,
                CredsSource::Claude => add_from_claude(&store, label.as_deref()).await?,
                CredsSource::Codex => add_from_codex(&store, label.as_deref()).await?,
            };
            Ok(format_row(&meta))
        }
    }
}

/// Read the secret from `stdin` (never argv) and seal it.
///
/// A TTY is refused so the token is not echoed and so operators pipe it.
/// `Cursor<&[u8]>` is the test equivalent of a pipe.
pub async fn add_from_stdin<R, S>(
    store: &S,
    stdin: R,
    stdin_is_tty: bool,
    provider: &str,
    kind: Kind,
    label: Option<&str>,
) -> Result<CredentialMeta, String>
where
    R: Read,
    S: CredentialStore,
{
    let secret = read_secret(stdin, stdin_is_tty)?;
    put_secret(store, provider, kind, label, &secret).await
}

pub async fn add_from_grok<S: CredentialStore>(
    store: &S,
    label: Option<&str>,
) -> Result<CredentialMeta, String> {
    let token = panday_sdk::oauth::grok_cli().ok_or_else(grok_missing)?;
    put_secret(store, "xai", Kind::Oauth, label, &token.access).await
}

pub async fn add_from_claude<S: CredentialStore>(
    store: &S,
    label: Option<&str>,
) -> Result<CredentialMeta, String> {
    let token = panday_sdk::oauth::claude_code().ok_or_else(claude_missing)?;
    put_secret(store, "anthropic", Kind::Oauth, label, &token.access).await
}

pub async fn add_from_codex<S: CredentialStore>(
    store: &S,
    label: Option<&str>,
) -> Result<CredentialMeta, String> {
    let path = codex_auth_path();
    let raw = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "no Codex login at {} (set CODEX_HOME or {CODEX_AUTH_ENV})",
            path.display()
        )
    })?;
    let access = access_token_from_codex_json(&raw)
        .map_err(|why| format!("could not parse Codex login at {}: {why}", path.display()))?;
    put_secret(store, "openai", Kind::Oauth, label, &access).await
}

pub async fn revoke_credential<S: CredentialStore>(store: &S, id: &str) -> Result<String, String> {
    let id: CredentialId = id
        .parse()
        .map_err(|_| format!("not a credential id: {id}"))?;
    store.revoke(&id).await.map_err(|e| match e {
        VaultError::NotFound => format!("no credential {id}"),
        other => other.to_string(),
    })?;
    Ok(format!("revoked {id}"))
}

/// Stable columns. Never the secret.
pub fn format_list(rows: &[CredentialMeta]) -> String {
    let mut out = String::from("id  provider  kind  label  last4  state");
    for row in rows {
        out.push('\n');
        out.push_str(&format_row(row));
    }
    out
}

fn format_row(row: &CredentialMeta) -> String {
    format!(
        "{}  {}  {}  {}  {}  {}",
        row.id,
        row.provider,
        row.kind.as_str(),
        row.label,
        row.last4,
        row.state.as_str()
    )
}

fn read_secret<R: Read>(mut stdin: R, stdin_is_tty: bool) -> Result<String, String> {
    if stdin_is_tty {
        return Err(
            "secret must be piped on stdin (a TTY would echo it; never pass the token on argv)"
                .into(),
        );
    }
    let mut raw = String::new();
    stdin
        .read_to_string(&mut raw)
        .map_err(|e| format!("read stdin: {e}"))?;
    // Trim the pipe contents, then one line: a trailing newline from `echo` is
    // not part of the token, and extra lines after a paste are ignored.
    let line = raw.trim().lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return Err("credential secret is empty".into());
    }
    Ok(line.to_string())
}

async fn put_secret<S: CredentialStore>(
    store: &S,
    provider: &str,
    kind: Kind,
    label: Option<&str>,
    secret: &str,
) -> Result<CredentialMeta, String> {
    if secret.is_empty() {
        return Err("credential secret is empty".into());
    }
    let meta = CredentialMeta {
        id: CredentialId::new(),
        provider: provider.trim().to_string(),
        kind,
        label: label.unwrap_or("").trim().to_string(),
        last4: String::new(),
        state: State::Active,
        ceiling: None,
        window_secs: None,
    };
    let id = meta.id;
    store.put(meta, secret).await.map_err(|e| e.to_string())?;
    store
        .list()
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| "vault put succeeded but the row is missing from list".into())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn grok_auth_path() -> PathBuf {
    home_dir().join(".grok").join("auth.json")
}

fn grok_missing() -> String {
    let path = grok_auth_path();
    if path.exists() {
        format!(
            "could not parse Grok CLI login at {} (expected an xAI OIDC entry with key)",
            path.display()
        )
    } else {
        format!("no Grok CLI login at {}", path.display())
    }
}

fn claude_missing() -> String {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            let path = Path::new(dir).join(".credentials.json");
            if path.exists() {
                return format!("could not parse Claude Code login at {}", path.display());
            }
            return format!(
                "no Claude Code login at {} (or Keychain item 'Claude Code-credentials')",
                path.display()
            );
        }
    }
    let path = home_dir().join(".claude").join(".credentials.json");
    if path.exists() {
        format!("could not parse Claude Code login at {}", path.display())
    } else {
        "no Claude Code login (Keychain item 'Claude Code-credentials' or ~/.claude/.credentials.json)"
            .into()
    }
}

/// Exposed for the M25.10 live probe (`tests/codex_probe.rs`).
pub fn codex_auth_path() -> PathBuf {
    if let Ok(p) = std::env::var(CODEX_AUTH_ENV) {
        let p = p.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(home) = std::env::var("CODEX_HOME") {
        let home = home.trim();
        if !home.is_empty() {
            return PathBuf::from(home).join("auth.json");
        }
    }
    home_dir().join(".codex").join("auth.json")
}

/// Codex CLI's ChatGPT OAuth. Typical file has `tokens.access_token`.
/// Exposed for the M25.10 live probe (`tests/codex_probe.rs`).
pub fn access_token_from_codex_json(raw: &str) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| "file is not JSON".to_string())?;
    v.pointer("/tokens/access_token")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            v.get("access_token")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| "expected tokens.access_token (ChatGPT OAuth)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use panday_sdk::vault::{CredentialStore, Kek, MemoryStore};
    use std::ffi::OsString;
    use std::io::Cursor;
    use std::sync::{Mutex, MutexGuard};
    use uuid::Uuid;

    const A: &str = "sk-test-aaaa";

    static ENV: Mutex<()> = Mutex::new(());

    struct IsolatedHome {
        _lock: MutexGuard<'static, ()>,
        dir: PathBuf,
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl IsolatedHome {
        const KEYS: &'static [&'static str] = &[
            "HOME",
            "USERPROFILE",
            "PANDAY_VAULT_KEY",
            "PANDAY_VAULT_DB",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "PANDAY_CODEX_AUTH",
        ];

        fn new() -> Self {
            let lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
            let saved = Self::KEYS
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
            let dir = std::env::temp_dir().join(format!("panday-creds-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let home = IsolatedHome {
                _lock: lock,
                dir,
                saved,
            };
            std::env::set_var("HOME", &home.dir);
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("PANDAY_VAULT_KEY");
            std::env::remove_var("PANDAY_VAULT_DB");
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("CODEX_HOME");
            std::env::remove_var("PANDAY_CODEX_AUTH");
            home
        }

        fn write(&self, rel: &str, contents: &str) {
            let path = self.dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, contents).unwrap();
        }
    }

    impl Drop for IsolatedHome {
        fn drop(&mut self) {
            for (k, v) in self.saved.drain(..) {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
            let tmp = std::env::temp_dir();
            if self.dir.starts_with(&tmp) {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }

    const GROK_JSON: &str = r#"{
      "https://auth.x.ai::test-client": {
        "key": "sk-test-aaaa",
        "refresh_token": "refresh-token-value",
        "expires_at": "2099-01-01T00:00:00Z",
        "oidc_client_id": "test-client"
      }
    }"#;

    const CLAUDE_JSON: &str = r#"{"claudeAiOauth":{"accessToken":"sk-test-aaaa","refreshToken":"rt","expiresAt":9999999999999}}"#;

    const CODEX_JSON: &str = r#"{
      "auth_mode": "chatgpt",
      "OPENAI_API_KEY": null,
      "tokens": {
        "id_token": "id-token-must-not-be-stored",
        "access_token": "sk-test-aaaa",
        "refresh_token": "refresh-token-value",
        "account_id": "acct"
      }
    }"#;

    fn listed_debug(rows: &[CredentialMeta]) -> String {
        format!("{rows:?}")
    }

    #[test]
    fn tty_stdin_is_refused_without_reading() {
        let err = read_secret(Cursor::new(A), true).unwrap_err();
        assert!(err.contains("piped"), "{err}");
        assert!(!err.contains(A), "{err}");
    }

    #[test]
    fn empty_stdin_is_an_error() {
        let err = read_secret(Cursor::new(b"  \n"), false).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn stdin_takes_the_trimmed_line() {
        assert_eq!(
            read_secret(Cursor::new(b"sk-test-aaaa\n"), false).unwrap(),
            A
        );
        assert_eq!(
            read_secret(Cursor::new(b"  sk-test-aaaa  \nignored\n"), false).unwrap(),
            A
        );
    }

    #[test]
    fn codex_json_reads_tokens_access_token() {
        assert_eq!(access_token_from_codex_json(CODEX_JSON).unwrap(), A);
        assert!(access_token_from_codex_json("{}").is_err());
        assert!(access_token_from_codex_json("not-json").is_err());
        assert_eq!(
            access_token_from_codex_json(r#"{"access_token":"sk-test-aaaa"}"#).unwrap(),
            A
        );
    }

    #[tokio::test]
    async fn add_from_stdin_stores_last4_and_hides_the_secret() {
        let store = MemoryStore::new(Kek::generate());
        let meta = add_from_stdin(
            &store,
            Cursor::new(b"sk-test-aaaa\n"),
            false,
            "xai",
            Kind::ApiKey,
            Some("paid"),
        )
        .await
        .unwrap();
        assert_eq!(meta.last4, "aaaa");
        assert_eq!(meta.provider, "xai");
        assert_eq!(meta.kind, Kind::ApiKey);
        assert_eq!(meta.label, "paid");
        assert_eq!(meta.state, State::Active);
        let listed = store.list().await.unwrap();
        let out = format_list(&listed);
        assert!(out.contains("aaaa"), "{out}");
        assert!(
            out.contains("id  provider  kind  label  last4  state"),
            "{out}"
        );
        assert!(out.contains("xai"), "{out}");
        assert!(out.contains("api_key"), "{out}");
        assert!(!out.contains(A), "list leaked the secret: {out}");
        assert!(
            !listed_debug(&listed).contains(A),
            "debug leaked the secret"
        );
        assert!(!format!("{meta:?}").contains(A));
        assert_eq!(store.get_secret(&meta.id).await.unwrap().expose(), A);
    }

    #[tokio::test]
    async fn tty_add_does_not_put_a_row() {
        let store = MemoryStore::new(Kek::generate());
        let err = add_from_stdin(
            &store,
            Cursor::new(A.as_bytes()),
            true,
            "xai",
            Kind::ApiKey,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("piped"), "{err}");
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn revoke_then_get_secret_fails() {
        let store = MemoryStore::new(Kek::generate());
        let meta = add_from_stdin(
            &store,
            Cursor::new(A.as_bytes()),
            false,
            "xai",
            Kind::ApiKey,
            None,
        )
        .await
        .unwrap();
        let msg = revoke_credential(&store, &meta.id.to_string())
            .await
            .unwrap();
        assert!(msg.contains(&meta.id.to_string()), "{msg}");
        assert!(matches!(
            store.get_secret(&meta.id).await.unwrap_err(),
            VaultError::Revoked
        ));
        let listed = store.list().await.unwrap();
        assert_eq!(listed[0].state, State::Revoked);
        assert_eq!(listed[0].last4, "aaaa");
        let out = format_list(&listed);
        assert!(!out.contains(A), "{out}");
    }

    #[tokio::test]
    async fn from_grok_copies_the_access_token_and_does_not_write_auth_json() {
        let home = IsolatedHome::new();
        home.write(".grok/auth.json", GROK_JSON);
        let before = std::fs::read(home.dir.join(".grok/auth.json")).unwrap();
        let store = MemoryStore::new(Kek::generate());
        let meta = add_from_grok(&store, Some("laptop")).await.unwrap();
        assert_eq!(meta.provider, "xai");
        assert_eq!(meta.kind, Kind::Oauth);
        assert_eq!(meta.last4, "aaaa");
        assert_eq!(meta.label, "laptop");
        let out = format_list(&store.list().await.unwrap());
        assert!(out.contains("oauth"), "{out}");
        assert!(!out.contains(A), "{out}");
        assert_eq!(store.get_secret(&meta.id).await.unwrap().expose(), A);
        let after = std::fs::read(home.dir.join(".grok/auth.json")).unwrap();
        assert_eq!(before, after, "must never write ~/.grok/auth.json");
        // Two imports are two rows — that is the point of the pool.
        let again = add_from_grok(&store, Some("laptop-2")).await.unwrap();
        assert_ne!(meta.id, again.id);
        assert_eq!(store.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn from_grok_without_auth_json_errors_clearly() {
        let _home = IsolatedHome::new();
        let store = MemoryStore::new(Kek::generate());
        let err = add_from_grok(&store, None).await.unwrap_err();
        assert!(err.contains("no Grok CLI login"), "{err}");
        assert!(err.contains("auth.json"), "{err}");
    }

    #[tokio::test]
    async fn from_claude_copies_oauth_and_does_not_write_credentials() {
        let home = IsolatedHome::new();
        home.write(".claude/.credentials.json", CLAUDE_JSON);
        std::env::set_var("CLAUDE_CONFIG_DIR", home.dir.join(".claude"));
        let before = std::fs::read(home.dir.join(".claude/.credentials.json")).unwrap();
        let store = MemoryStore::new(Kek::generate());
        let meta = add_from_claude(&store, Some("max")).await.unwrap();
        assert_eq!(meta.provider, "anthropic");
        assert_eq!(meta.kind, Kind::Oauth);
        assert_eq!(meta.last4, "aaaa");
        assert!(!format_list(&store.list().await.unwrap()).contains(A));
        let after = std::fs::read(home.dir.join(".claude/.credentials.json")).unwrap();
        assert_eq!(before, after, "must never write Claude credentials.json");
    }

    #[tokio::test]
    async fn from_claude_malformed_file_does_not_fall_through_silently() {
        let home = IsolatedHome::new();
        // A present but unparsable file must not be treated as "no login" in a
        // way that hides the path; Keychain is not consulted once the file reads.
        home.write(".claude/.credentials.json", "{");
        std::env::set_var("CLAUDE_CONFIG_DIR", home.dir.join(".claude"));
        let store = MemoryStore::new(Kek::generate());
        let err = add_from_claude(&store, None).await.unwrap_err();
        assert!(err.contains("could not parse Claude Code login"), "{err}");
    }

    #[tokio::test]
    async fn from_codex_reads_tokens_access_token_via_codex_home() {
        let home = IsolatedHome::new();
        home.write(".codex/auth.json", CODEX_JSON);
        std::env::set_var("CODEX_HOME", home.dir.join(".codex"));
        let before = std::fs::read(home.dir.join(".codex/auth.json")).unwrap();
        let store = MemoryStore::new(Kek::generate());
        let meta = add_from_codex(&store, Some("chatgpt")).await.unwrap();
        assert_eq!(meta.provider, "openai");
        assert_eq!(meta.kind, Kind::Oauth);
        assert_eq!(meta.last4, "aaaa");
        assert_eq!(store.get_secret(&meta.id).await.unwrap().expose(), A);
        let out = format_list(&store.list().await.unwrap());
        assert!(!out.contains(A), "{out}");
        assert!(!out.contains("id-token-must-not-be-stored"), "{out}");
        let after = std::fs::read(home.dir.join(".codex/auth.json")).unwrap();
        assert_eq!(before, after, "must never write ~/.codex/auth.json");
    }

    #[tokio::test]
    async fn from_codex_missing_file_errors_with_the_path() {
        let home = IsolatedHome::new();
        std::env::set_var("PANDAY_CODEX_AUTH", home.dir.join("missing-auth.json"));
        let store = MemoryStore::new(Kek::generate());
        let err = add_from_codex(&store, None).await.unwrap_err();
        assert!(err.contains("no Codex login"), "{err}");
        assert!(err.contains("missing-auth.json"), "{err}");
    }

    #[tokio::test]
    async fn run_creds_add_list_revoke_through_the_sqlite_vault() {
        let _home = IsolatedHome::new();
        let add = Command::Creds(CredsAction::Add {
            provider: "xai".into(),
            label: Some("paid".into()),
            kind: Kind::ApiKey,
            source: CredsSource::Stdin,
        });
        let added = run_creds(&add, Cursor::new(b"sk-test-aaaa\n"), false)
            .await
            .unwrap();
        assert!(added.contains("aaaa"), "{added}");
        assert!(added.contains("paid"), "{added}");
        assert!(!added.contains(A), "add output leaked the secret: {added}");

        let listed = run_creds(&Command::Creds(CredsAction::List), Cursor::new(b""), false)
            .await
            .unwrap();
        assert!(
            listed.starts_with("id  provider  kind  label  last4  state"),
            "{listed}"
        );
        assert!(listed.contains("aaaa"), "{listed}");
        assert!(listed.contains("api_key"), "{listed}");
        assert!(!listed.contains(A), "list leaked the secret: {listed}");

        let id = added.split_whitespace().next().expect("id column");
        let revoked = run_creds(
            &Command::Creds(CredsAction::Revoke { id: id.into() }),
            Cursor::new(b""),
            false,
        )
        .await
        .unwrap();
        assert!(revoked.contains("revoked"), "{revoked}");
        let store = open_operator_vault().await.unwrap();
        let cid: CredentialId = id.parse().unwrap();
        assert!(matches!(
            store.get_secret(&cid).await.unwrap_err(),
            VaultError::Revoked
        ));
    }
}
