//! Subscription OAuth — reuse the login the official CLIs already did.
//!
//! The piece taken from OpenCodex (`local-token-detect` / `oauth/xai.ts`) is
//! the *pattern*, not a proxy: read Grok CLI's `~/.grok/auth.json` (xAI OIDC)
//! and Claude Code's Keychain / `~/.claude/.credentials.json`, then call the
//! provider's own API with that access token. **Never write those files.**
//! A refresh, if the access token is stale, stays in memory.
//!
//! This is the user's own subscription, the same way Grok CLI and Claude Code
//! authenticate. It is not an API key and it is not a third-party proxy.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const XAI_AUTH_KEY_PREFIX: &str = "https://auth.x.ai::";
const XAI_API: &str = "https://api.x.ai";
const XAI_DISCOVERY: &str = "https://auth.x.ai/.well-known/openid-configuration";
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const REFRESH_SKEW: Duration = Duration::from_secs(120);

/// A bearer token we can hand to an adapter.
#[derive(Debug, Clone)]
pub struct Token {
    pub access: String,
    pub refresh: Option<String>,
    /// Unix millis. `None` means "unknown — try it, refresh on 401".
    pub expires_unix_ms: Option<u64>,
    pub client_id: Option<String>,
}

impl Token {
    pub fn still_fresh(&self) -> bool {
        let Some(exp) = self.expires_unix_ms else {
            return true;
        };
        now_ms().saturating_add(REFRESH_SKEW.as_millis() as u64) < exp
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    #[error("oauth refresh: {0}")]
    Refresh(String),
}

/// Grok CLI's xAI OIDC session, if `~/.grok/auth.json` has one.
pub fn grok_cli() -> Option<Token> {
    grok_cli_from_path(&grok_auth_path())
}

pub fn grok_cli_from_path(path: &Path) -> Option<Token> {
    let raw = std::fs::read_to_string(path).ok()?;
    grok_cli_from_json(&raw)
}

pub fn grok_cli_from_json(raw: &str) -> Option<Token> {
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(raw).ok()?;
    let (key, entry) = map
        .into_iter()
        .find(|(k, _)| k.starts_with(XAI_AUTH_KEY_PREFIX))?;
    let access = entry.get("key")?.as_str()?.to_string();
    if access.is_empty() {
        return None;
    }
    let refresh = entry
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let expires_unix_ms = entry
        .get("expires_at")
        .and_then(|v| v.as_str())
        .and_then(parse_rfc3339_millis);
    let client_id = entry
        .get("oidc_client_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            key.strip_prefix(XAI_AUTH_KEY_PREFIX)
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        });
    Some(Token {
        access,
        refresh,
        expires_unix_ms,
        client_id,
    })
}

/// Access token for the `xai` adapter, refreshing if the CLI session is stale.
pub async fn grok_access() -> Option<String> {
    let tok = grok_cli()?;
    if tok.still_fresh() {
        return Some(tok.access);
    }
    refresh_xai(&tok).await.ok().map(|t| t.access)
}

/// Where the `xai` openai_compat adapter should point.
pub fn xai_api_base() -> &'static str {
    XAI_API
}

async fn refresh_xai(tok: &Token) -> Result<Token, OauthError> {
    let refresh = tok
        .refresh
        .as_deref()
        .ok_or_else(|| OauthError::Refresh("no refresh token".into()))?;
    let client_id = tok
        .client_id
        .as_deref()
        .ok_or_else(|| OauthError::Refresh("no oidc client id".into()))?;
    let discovery: Discovery = reqwest::Client::new()
        .get(XAI_DISCOVERY)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| OauthError::Refresh(e.to_string()))?
        .error_for_status()
        .map_err(|e| OauthError::Refresh(e.to_string()))?
        .json()
        .await
        .map_err(|e| OauthError::Refresh(e.to_string()))?;
    let resp: TokenResponse = reqwest::Client::new()
        .post(&discovery.token_endpoint)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .map_err(|e| OauthError::Refresh(e.to_string()))?
        .error_for_status()
        .map_err(|e| OauthError::Refresh(e.to_string()))?
        .json()
        .await
        .map_err(|e| OauthError::Refresh(e.to_string()))?;
    let access = resp
        .access_token
        .ok_or_else(|| OauthError::Refresh("token response had no access_token".into()))?;
    let expires_unix_ms = resp
        .expires_in
        .map(|s| now_ms().saturating_add((s.saturating_sub(120)) * 1000));
    Ok(Token {
        access,
        refresh: resp.refresh_token.or_else(|| tok.refresh.clone()),
        expires_unix_ms,
        client_id: tok.client_id.clone(),
    })
}

#[derive(Deserialize)]
struct Discovery {
    token_endpoint: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

/// Claude Code's subscription token, if the Keychain or credentials file has one.
pub fn claude_code() -> Option<Token> {
    claude_code_from_payload(&read_claude_payload()?)
}

pub fn claude_code_from_payload(raw: &str) -> Option<Token> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let o = v.get("claudeAiOauth")?;
    let access = o.get("accessToken")?.as_str()?.to_string();
    let refresh = o
        .get("refreshToken")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let expires_unix_ms = o.get("expiresAt").and_then(|x| x.as_u64());
    if access.is_empty() {
        return None;
    }
    Some(Token {
        access,
        refresh,
        expires_unix_ms,
        client_id: None,
    })
}

fn read_claude_payload() -> Option<String> {
    if let Some(dir) = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return read_claude_file(Path::new(&dir).join(".credentials.json"))
            .or_else(read_claude_keychain);
    }
    read_claude_keychain()
        .or_else(|| read_claude_file(home_dir().join(".claude").join(".credentials.json")))
}

fn read_claude_file(path: PathBuf) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn read_claude_keychain() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE, "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn grok_auth_path() -> PathBuf {
    home_dir().join(".grok").join("auth.json")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_rfc3339_millis(s: &str) -> Option<u64> {
    // `2026-08-20T13:31:25.953466Z` — Grok CLI's shape. Split rather than
    // taking `time` in this crate just to parse one timestamp.
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let y: i32 = d.next()?.parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let da: u32 = d.next()?.parse().ok()?;
    let (hms, frac) = match time.split_once('.') {
        Some((hms, f)) => (hms, f),
        None => (time, "0"),
    };
    let mut t = hms.split(':');
    let h: u32 = t.next()?.parse().ok()?;
    let mi: u32 = t.next()?.parse().ok()?;
    let se: u32 = t.next()?.parse().ok()?;
    let mut ms: u32 = 0;
    if !frac.is_empty() {
        let padded = format!("{frac:0<3}");
        ms = padded.chars().take(3).collect::<String>().parse().ok()?;
    }
    let days = days_from_civil(y, mo, da)?;
    let secs = days
        .checked_mul(86400)?
        .checked_add(i64::from(h * 3600 + mi * 60 + se))?;
    if secs < 0 {
        return None;
    }
    Some(
        (secs as u64)
            .saturating_mul(1000)
            .saturating_add(u64::from(ms)),
    )
}

/// Howard's days_from_civil, unix epoch = 1970-01-01.
fn days_from_civil(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || d == 0 || d > 31 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m = m as i64;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = i64::from(yoe) * 365 + i64::from(yoe / 4) - i64::from(yoe / 100) + doy;
    Some(i64::from(era) * 146097 + doe - 719468)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROK_FIXTURE: &str = r#"{
      "https://auth.x.ai::test-client": {
        "key": "access-token-value",
        "refresh_token": "refresh-token-value",
        "expires_at": "2099-01-01T00:00:00Z",
        "oidc_client_id": "test-client",
        "email": "dev@example.com"
      }
    }"#;

    #[test]
    fn grok_cli_json_reads_the_xai_oidc_entry() {
        let tok = grok_cli_from_json(GROK_FIXTURE).expect("parse");
        assert_eq!(tok.access, "access-token-value");
        assert_eq!(tok.refresh.as_deref(), Some("refresh-token-value"));
        assert_eq!(tok.client_id.as_deref(), Some("test-client"));
        assert!(tok.still_fresh(), "2099 is in the future");
    }

    #[test]
    fn grok_cli_json_ignores_unrelated_keys() {
        let raw = r#"{"https://example.com::other":{"key":"nope"}}"#;
        assert!(grok_cli_from_json(raw).is_none());
    }

    #[test]
    fn a_past_expiry_is_not_fresh() {
        let raw = r#"{
          "https://auth.x.ai::c": {
            "key": "k",
            "refresh_token": "r",
            "expires_at": "2020-01-01T00:00:00Z",
            "oidc_client_id": "c"
          }
        }"#;
        let tok = grok_cli_from_json(raw).unwrap();
        assert!(!tok.still_fresh());
    }

    #[test]
    fn claude_payload_reads_claude_ai_oauth() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"at","refreshToken":"rt","expiresAt":9999999999999}}"#;
        let tok = claude_code_from_payload(raw).expect("parse");
        assert_eq!(tok.access, "at");
        assert_eq!(tok.refresh.as_deref(), Some("rt"));
        assert!(tok.still_fresh());
    }

    #[test]
    fn rfc3339_with_fractional_seconds_parses() {
        let ms = parse_rfc3339_millis("2026-08-20T13:31:25.953466Z").expect("parse");
        assert!(ms > 1_700_000_000_000);
        assert_eq!(parse_rfc3339_millis("1970-01-01T00:00:00Z"), Some(0));
    }
}
