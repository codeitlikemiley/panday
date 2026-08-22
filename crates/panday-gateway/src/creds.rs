//! Boot + console helpers for the credential pools (docs/25).

use crate::adapters::pool::{
    anthropic_key_adapter, anthropic_oauth_adapter, env_grant, env_keys, last4, openai_key_adapter,
    xai_key_adapter, xai_oauth_adapter, CredHub, Grant, Rotate,
};
use panday_sdk::vault::{
    default_master_key_path, default_vault_db_path, CredentialMeta, CredentialStore, Kek, Kind,
    SqliteStore,
};

impl CredHub {
    /// Env keys, Grok/Claude CLI OAuth, then rows from the sealed vault.
    pub async fn seed_from_process() -> Self {
        let hub = CredHub::new();
        // Secrets are captured here as they are read, because the pool holds
        // adapters rather than secrets by design and cannot hand them back.
        // Used only to seed an *empty* vault, below (docs/25 M25.9).
        let mut seeds: Vec<(&'static str, Kind, String, String)> = Vec::new();
        if let Ok(v) = std::env::var("PANDAY_ROTATE") {
            if let Some(p) = Rotate::parse(&v) {
                hub.set_rotate(p);
            }
        }

        for (i, token) in panday_sdk::oauth::grok_access_all()
            .await
            .into_iter()
            .enumerate()
        {
            let tail = last4(&token);
            if hub.contains_last4("xai", &tail) {
                continue;
            }
            seeds.push((
                "xai",
                Kind::Oauth,
                format!("grok-cli-{}", i + 1),
                token.clone(),
            ));
            hub.xai.push(
                "oauth",
                &format!("grok-cli-{}", i + 1),
                &tail,
                xai_oauth_adapter(token),
            );
        }
        for (i, key) in env_keys("XAI_API_KEY", "PANDAY_XAI_API_KEYS")
            .into_iter()
            .enumerate()
        {
            let tail = last4(&key);
            if hub.contains_last4("xai", &tail) {
                continue;
            }
            seeds.push((
                "xai",
                Kind::ApiKey,
                format!("xai-key-{}", i + 1),
                key.clone(),
            ));
            hub.xai.push(
                "api_key",
                &format!("xai-key-{}", i + 1),
                &tail,
                xai_key_adapter(key),
            );
        }

        for (i, key) in env_keys("ANTHROPIC_API_KEY", "PANDAY_ANTHROPIC_API_KEYS")
            .into_iter()
            .enumerate()
        {
            let tail = last4(&key);
            if hub.contains_last4("anthropic", &tail) {
                continue;
            }
            seeds.push((
                "anthropic",
                Kind::ApiKey,
                format!("anthropic-key-{}", i + 1),
                key.clone(),
            ));
            hub.anthropic.push(
                "api_key",
                &format!("anthropic-key-{}", i + 1),
                &tail,
                anthropic_key_adapter(key),
            );
        }
        if let Some(tok) = panday_sdk::oauth::claude_code() {
            if tok.still_fresh() {
                let tail = last4(&tok.access);
                if !hub.contains_last4("anthropic", &tail) {
                    seeds.push((
                        "anthropic",
                        Kind::Oauth,
                        "claude-code".to_string(),
                        tok.access.clone(),
                    ));
                    hub.anthropic.push(
                        "oauth",
                        "claude-code",
                        &tail,
                        anthropic_oauth_adapter(tok.access),
                    );
                }
            }
        }

        let openai_base = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://api.openai.com/v1".into());
        for (i, key) in env_keys("OPENAI_API_KEY", "PANDAY_OPENAI_API_KEYS")
            .into_iter()
            .enumerate()
        {
            let tail = last4(&key);
            if hub.contains_last4("openai", &tail) {
                continue;
            }
            seeds.push((
                "openai",
                Kind::ApiKey,
                format!("openai-key-{}", i + 1),
                key.clone(),
            ));
            hub.openai.push(
                "api_key",
                &format!("openai-key-{}", i + 1),
                &tail,
                openai_key_adapter(&openai_base, key),
            );
        }

        let gemini_base = std::env::var("GEMINI_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta/openai".into());
        for (i, key) in env_keys("GEMINI_API_KEY", "PANDAY_GEMINI_API_KEYS")
            .into_iter()
            .enumerate()
        {
            let tail = last4(&key);
            if hub.contains_last4("gemini", &tail) {
                continue;
            }
            seeds.push((
                "gemini",
                Kind::ApiKey,
                format!("gemini-key-{}", i + 1),
                key.clone(),
            ));
            hub.gemini.push(
                "api_key",
                &format!("gemini-key-{}", i + 1),
                &tail,
                openai_key_adapter(&gemini_base, key),
            );
        }

        // What env and the CLI stores found, before the vault contributes. Used
        // below to decide whether this is a first boot worth seeding from.
        // Ceilings the operator declared per credential, keyed by last4 — the
        // only handle that survives a restart, because pool member ids are
        // regenerated every boot while the vault row's id is not.
        let mut stored_grants: std::collections::BTreeMap<(String, String), Grant> =
            Default::default();

        if let Some(store) = open_vault().await {
            let rows = store.list().await.unwrap_or_default();
            for row in &rows {
                if row.state == panday_sdk::vault::State::Revoked {
                    continue;
                }
                if let (Some(ceiling), window) = (row.ceiling, row.window_secs) {
                    stored_grants.insert(
                        (row.provider.clone(), row.last4.clone()),
                        Grant {
                            ceiling,
                            window: std::time::Duration::from_secs(
                                window.unwrap_or(30 * 24 * 3600),
                            ),
                        },
                    );
                }
            }
            for row in rows {
                if row.state == panday_sdk::vault::State::Revoked {
                    continue;
                }
                if hub.contains_last4(&row.provider, &row.last4) {
                    continue;
                }
                let Ok(secret) = store.get_secret(&row.id).await else {
                    continue;
                };
                let _ = hub.add_secret(
                    &row.provider,
                    row.kind.as_str(),
                    &row.label,
                    secret.expose(),
                );
            }

            // Back-compat, and the clause that makes the vault the boot source
            // of truth: a vault with nothing in it adopts whatever env and the
            // CLI stores just supplied, so the *next* boot can find them there
            // even if the env is gone (docs/25 M25.9).
            //
            // Only when empty. Seeding a populated vault would resurrect a
            // credential the operator had revoked, every time they restarted
            // with a stale variable still exported.
            if store.list().await.map(|r| r.is_empty()).unwrap_or(false) {
                for (provider, kind, label, secret) in &seeds {
                    let meta = CredentialMeta {
                        id: panday_sdk::vault::CredentialId::new(),
                        provider: (*provider).to_string(),
                        kind: *kind,
                        label: label.clone(),
                        last4: String::new(),
                        state: panday_sdk::vault::State::Active,
                        ceiling: None,
                        window_secs: None,
                    };
                    let _ = store.put(meta, secret).await;
                }
            }
        }

        // Grants last, so every credential this boot found — env, CLI OAuth, and
        // vault rows alike — starts with the operator's declared ceiling rather
        // than only the ones that happened to be added first (docs/25 M25.6).
        //
        // A vault row's ceiling wins over the env default: env declares one
        // number for a whole provider, the row declares one for *this*
        // credential, and the specific statement is the one the operator made
        // most recently and most deliberately (docs/25 M25.9).
        for (provider, pool) in [
            ("xai", &hub.xai),
            ("anthropic", &hub.anthropic),
            ("openai", &hub.openai),
            ("gemini", &hub.gemini),
        ] {
            let default = env_grant(provider);
            for m in pool.list() {
                let stored = stored_grants
                    .get(&(provider.to_string(), m.last4.clone()))
                    .copied();
                if let Some(grant) = stored.or(default) {
                    pool.set_grant(&m.id, Some(grant));
                }
            }
        }

        hub
    }

    /// Add to the matching pool. Does not persist; caller may `persist`.
    pub fn add_secret(
        &self,
        provider: &str,
        kind: &str,
        label: &str,
        secret: &str,
    ) -> Result<crate::adapters::pool::MemberMeta, String> {
        let secret = secret.trim();
        if secret.is_empty() {
            return Err("secret is empty".into());
        }
        let tail = last4(secret);
        if self.contains_last4(provider, &tail) {
            return Err(format!(
                "already have a {provider} credential ending {tail}"
            ));
        }
        let pool = self
            .pool(provider)
            .ok_or_else(|| format!("unknown provider `{provider}`"))?;
        let adapter = match (provider, kind) {
            ("xai", _) => xai_key_adapter(secret),
            ("openai", _) => {
                let base = std::env::var("OPENAI_BASE_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "https://api.openai.com/v1".into());
                openai_key_adapter(&base, secret)
            }
            ("gemini", _) => {
                let base = std::env::var("GEMINI_BASE_URL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| {
                        "https://generativelanguage.googleapis.com/v1beta/openai".into()
                    });
                openai_key_adapter(&base, secret)
            }
            ("anthropic", "oauth") => anthropic_oauth_adapter(secret),
            ("anthropic", _) => anthropic_key_adapter(secret),
            (other, _) => return Err(format!("unknown provider `{other}`")),
        };
        Ok(pool.push(kind, label, &tail, adapter))
    }
}

pub async fn open_vault() -> Option<SqliteStore> {
    // One precedence, in `Kek::resolve` (docs/25 M25.12), so the gateway and the
    // CLI cannot disagree about which key seals the same vault.
    let path = default_master_key_path()?;
    let kek = Kek::resolve(&path).ok()?;
    let db = default_vault_db_path()?;
    SqliteStore::open(db, kek).await.ok()
}

pub async fn persist_secret(
    provider: &str,
    kind: Kind,
    label: &str,
    secret: &str,
) -> Result<Option<CredentialMeta>, String> {
    let Some(store) = open_vault().await else {
        return Ok(None);
    };
    let meta = CredentialMeta {
        id: panday_sdk::vault::CredentialId::new(),
        provider: provider.into(),
        kind,
        label: label.into(),
        last4: String::new(),
        state: panday_sdk::vault::State::Active,
        ceiling: None,
        window_secs: None,
    };
    store
        .put(meta.clone(), secret)
        .await
        .map_err(|e| e.to_string())?;
    Ok(store
        .list()
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|m| m.id == meta.id))
}

pub async fn persist_revoke(provider: &str, last4: &str) {
    let Some(store) = open_vault().await else {
        return;
    };
    let Ok(rows) = store.list().await else {
        return;
    };
    for row in rows {
        if row.provider == provider && row.last4 == last4 {
            let _ = store.revoke(&row.id).await;
        }
    }
}

/// Persist a credential's declared grant to the sealed vault (docs/25 M25.9).
///
/// Matched by `(provider, last4)`, not by pool member id: member ids are
/// regenerated on every boot, so an id written today would match nothing
/// tomorrow. `last4` is stable, stored in the clear by design, and unique
/// within a provider because the pool refuses a duplicate.
///
/// Best-effort. A laptop with no writable `~/.panday` still gets the in-process
/// grant; it just will not survive a restart, exactly as before M25.9.
pub async fn persist_grant(
    provider: &str,
    last4: &str,
    ceiling: Option<u64>,
    window_secs: Option<u64>,
) {
    let Some(store) = open_vault().await else {
        return;
    };
    let Ok(rows) = store.list().await else {
        return;
    };
    let Some(row) = rows
        .into_iter()
        .find(|r| r.provider == provider && r.last4 == last4)
    else {
        return;
    };
    let _ = store.set_grant(&row.id, ceiling, window_secs).await;
}
