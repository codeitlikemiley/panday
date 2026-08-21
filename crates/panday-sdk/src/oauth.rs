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

/// Grok CLI's xAI OIDC session, if any configured store has one.
///
/// First token only — same contract as before this experiment. Every xAI
/// entry / extra `auth.json` is [`grok_cli_all`].
pub fn grok_cli() -> Option<Token> {
    grok_cli_all().into_iter().next()
}

pub fn grok_cli_from_path(path: &Path) -> Option<Token> {
    grok_cli_all_from_path(path).into_iter().next()
}

/// Every xAI OIDC entry in one Grok CLI `auth.json`.
pub fn grok_cli_all_from_path(path: &Path) -> Vec<Token> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    grok_cli_all_from_json(&raw)
}

/// First xAI OIDC entry in `raw`. Old callers that assumed one session.
pub fn grok_cli_from_json(raw: &str) -> Option<Token> {
    grok_cli_all_from_json(raw).into_iter().next()
}

/// Every `https://auth.x.ai::` object in one JSON document.
///
/// One file can hold two SuperGrok accounts pasted side by side. Empty `key`
/// values are skipped. Map iteration order is what [`grok_cli_from_json`]
/// used to pick as "first".
pub fn grok_cli_all_from_json(raw: &str) -> Vec<Token> {
    let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(raw) else {
        return Vec::new();
    };
    map.into_iter()
        .filter(|(k, _)| k.starts_with(XAI_AUTH_KEY_PREFIX))
        .filter_map(|(k, entry)| token_from_xai_entry(&k, &entry))
        .collect()
}

fn token_from_xai_entry(key: &str, entry: &serde_json::Value) -> Option<Token> {
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

/// Union of xAI sessions across several `auth.json` files.
///
/// Missing or empty files are skipped, not fatal. A second SuperGrok login is
/// a copy of another machine's file, not a second Grok CLI install. Duplicate
/// access tokens (same file listed twice) are kept once, first path wins.
pub fn grok_cli_from_paths<P: AsRef<Path>>(paths: &[P]) -> Vec<Token> {
    let mut out = Vec::new();
    for path in paths {
        for tok in grok_cli_all_from_path(path.as_ref()) {
            if out.iter().any(|have: &Token| have.access == tok.access) {
                continue;
            }
            out.push(tok);
        }
    }
    out
}

/// Default `~/.grok/auth.json` plus colon-separated extras in `PANDAY_GROK_AUTH`.
pub fn grok_cli_all() -> Vec<Token> {
    grok_cli_from_paths(&grok_auth_paths())
}

/// Access token for the `xai` adapter, refreshing if the CLI session is stale.
pub async fn grok_access() -> Option<String> {
    grok_access_all().await.into_iter().next()
}

/// Every still-usable Grok access token.
///
/// Stale tokens are refreshed in memory via the existing xAI OIDC flow; a
/// failed refresh drops that token rather than writing `auth.json`. There is
/// no write-back of rotated refresh tokens (same as [`grok_access`]).
pub async fn grok_access_all() -> Vec<String> {
    let mut out = Vec::new();
    for tok in grok_cli_all() {
        if tok.still_fresh() {
            out.push(tok.access);
            continue;
        }
        if let Ok(refreshed) = refresh_xai(&tok).await {
            out.push(refreshed.access);
        }
    }
    out
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

fn grok_auth_paths() -> Vec<PathBuf> {
    grok_auth_paths_from(
        grok_auth_path(),
        std::env::var("PANDAY_GROK_AUTH").ok().as_deref(),
    )
}

/// `default` first, then colon-separated extras. Duplicate paths are dropped.
fn grok_auth_paths_from(default: PathBuf, extra: Option<&str>) -> Vec<PathBuf> {
    let mut paths = vec![default];
    if let Some(extra) = extra {
        for raw in extra.split(':') {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let path = PathBuf::from(raw);
            if paths.iter().any(|have| have == &path) {
                continue;
            }
            paths.push(path);
        }
    }
    paths
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
        assert!(grok_cli_all_from_json(raw).is_empty());
    }

    const TWO_GROK_SESSIONS: &str = r#"{
      "https://example.com::other": { "key": "nope" },
      "https://auth.x.ai::client-a": {
        "key": "sk-test-aaaa",
        "refresh_token": "refresh-a",
        "expires_at": "2099-01-01T00:00:00Z",
        "oidc_client_id": "client-a",
        "email": "a@example.com"
      },
      "https://auth.x.ai::client-b": {
        "key": "sk-test-bbbb",
        "refresh_token": "refresh-b",
        "expires_at": "2099-01-01T00:00:00Z",
        "oidc_client_id": "client-b",
        "email": "b@example.com"
      },
      "https://auth.x.ai::empty": { "key": "" }
    }"#;

    #[test]
    fn grok_cli_all_from_json_keeps_every_xai_entry() {
        let toks = grok_cli_all_from_json(TWO_GROK_SESSIONS);
        assert_eq!(
            toks.iter().map(|t| t.access.as_str()).collect::<Vec<_>>(),
            vec!["sk-test-aaaa", "sk-test-bbbb"]
        );
        assert_eq!(toks[0].client_id.as_deref(), Some("client-a"));
        assert_eq!(toks[1].client_id.as_deref(), Some("client-b"));
        let first = grok_cli_from_json(TWO_GROK_SESSIONS).expect("first session");
        assert_eq!(first.access, toks[0].access);
        assert_eq!(first.access, "sk-test-aaaa");
    }

    #[test]
    fn grok_cli_from_paths_unions_files_and_skips_missing_or_empty() {
        let root =
            std::env::temp_dir().join(format!("panday-grok-auth-{}-union", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let a = root.join("a.json");
        let b = root.join("b.json");
        let missing = root.join("nope.json");
        let empty = root.join("empty.json");
        // Same oidc client_id in both files: two machines, one Grok CLI app.
        std::fs::write(
            &a,
            r#"{
              "https://auth.x.ai::test-client": {
                "key": "access-token-a",
                "refresh_token": "refresh-a",
                "expires_at": "2099-01-01T00:00:00Z",
                "oidc_client_id": "test-client"
              }
            }"#,
        )
        .unwrap();
        std::fs::write(
            &b,
            r#"{
              "https://auth.x.ai::test-client": {
                "key": "access-token-b",
                "refresh_token": "refresh-b",
                "expires_at": "2099-01-01T00:00:00Z",
                "oidc_client_id": "test-client"
              }
            }"#,
        )
        .unwrap();
        std::fs::write(&empty, "   ").unwrap();
        let toks = grok_cli_from_paths(&[
            a.as_path(),
            missing.as_path(),
            empty.as_path(),
            b.as_path(),
            a.as_path(),
        ]);
        assert_eq!(
            toks.iter().map(|t| t.access.as_str()).collect::<Vec<_>>(),
            vec!["access-token-a", "access-token-b"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grok_auth_paths_default_then_colon_separated_extras() {
        let default = PathBuf::from("/tmp/default-auth.json");
        let paths = grok_auth_paths_from(
            default.clone(),
            Some(" /tmp/second.json : /tmp/third.json : /tmp/default-auth.json : "),
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/tmp/default-auth.json"),
                PathBuf::from("/tmp/second.json"),
                PathBuf::from("/tmp/third.json"),
            ]
        );
        assert_eq!(
            grok_auth_paths_from(default.clone(), None),
            vec![default.clone()]
        );
        assert_eq!(
            grok_auth_paths_from(default.clone(), Some("")),
            vec![default]
        );
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
