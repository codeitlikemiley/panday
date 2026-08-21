//! Boot + console helpers for the credential pools (docs/25).

use crate::adapters::pool::{
    anthropic_key_adapter, anthropic_oauth_adapter, env_keys, last4, openai_key_adapter,
    xai_key_adapter, xai_oauth_adapter, CredHub, Rotate,
};
use panday_sdk::vault::{
    default_master_key_path, default_vault_db_path, CredentialMeta, CredentialStore, Kek, Kind,
    SqliteStore,
};

impl CredHub {
    /// Env keys, Grok/Claude CLI OAuth, then rows from the sealed vault.
    pub async fn seed_from_process() -> Self {
        let hub = CredHub::new();
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
            hub.gemini.push(
                "api_key",
                &format!("gemini-key-{}", i + 1),
                &tail,
                openai_key_adapter(&gemini_base, key),
            );
        }

        if let Some(store) = open_vault().await {
            if let Ok(rows) = store.list().await {
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
    let kek = match Kek::from_env().ok()? {
        Some(k) => k,
        None => {
            let path = default_master_key_path()?;
            Kek::load_or_create(&path).ok()?
        }
    };
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
