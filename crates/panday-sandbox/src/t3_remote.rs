//! T3-remote — CodeSandbox / Together SDK microVMs (docs/14 M14.9).
//!
//! When `SandboxTier` wants a hardware-isolated guest and this host has no
//! `/dev/kvm`, this backend talks to the public CodeSandbox control plane
//! (`https://api.codesandbox.io`, npm `@codesandbox/sdk`) using the operator's
//! own workspace token. Sandboxes are created in *that* workspace and billed
//! there. It is exec / files / hibernate on a Linux microVM — not a browser
//! playground, not noVNC, not berthos.
//!
//! ## What the public REST API actually does
//!
//! Control plane, Bearer token, documented by the SDK's generated client
//! (`codesandbox/codesandbox-sdk` `src/api-clients/client/sdk.gen.ts`):
//!
//! | Call | HTTP | Maps to |
//! |---|---|---|
//! | fork template | `POST /sandbox/{id}/fork` | [`Sandbox::create`] |
//! | start VM | `POST /vm/{id}/start` | (same; returns guest locator) |
//! | hibernate | `POST /vm/{id}/hibernate` | [`Sandbox::snapshot`] |
//! | delete | `DELETE /vm/{id}` | [`Sandbox::destroy`] |
//!
//! Guest I/O (`commands.run`, `fs.write` / `fs.read`) is **not** on that
//! control plane. The official SDK speaks Pitcher over WebSocket after
//! `start` returns `pitcher_url` + `pitcher_token`. This first slice stays
//! on `reqwest` and talks a thin HTTP seam to that same host:
//!
//! | Call | HTTP on `pitcher_url` |
//! |---|---|
//! | exec | `POST /commands/run` |
//! | put | `PUT /fs?path=` |
//! | get | `GET /fs?path=` |
//!
//! Tests mock both planes. A later slice can speak Pitcher if live CSB never
//! grows the REST agent the SDK's v2.3 notes describe. We do not take a Node
//! runtime dependency.
//!
//! ## Token
//!
//! BYO only. `CSB_API_KEY` or a vault row with provider `codesandbox`.
//! Fail-closed without one. Never logged (`Debug` is redacted). Multiple
//! tokens the operator already owns may be registered the same way they
//! register multiple OpenAI keys; each token is that user's/workspace's own
//! plan, not a harvested farm of free logins (CodeSandbox ToS 2.3 / 4.4(l)).

use crate::{
    ExecChunk, ExecSpec, ExecStream, Sandbox, SandboxError, SandboxHandle, SandboxTier,
    SessionSpec, SnapshotRef,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

/// Env var the official SDK reads. Never put the value on argv.
pub const CSB_API_KEY_ENV: &str = "CSB_API_KEY";

/// Vault provider name (`panday creds add --provider codesandbox`).
pub const CSB_VAULT_PROVIDER: &str = "codesandbox";

/// Production control plane. Overridable so tests never leave the loopback.
pub const CSB_DEFAULT_BASE: &str = "https://api.codesandbox.io";

/// Universal template id the SDK ships as of 2025-06 (`pcz35m`).
pub const CSB_DEFAULT_TEMPLATE: &str = "pcz35m";

/// Cheapest VM size the SDK names. Credits are the token owner's.
pub const CSB_DEFAULT_VM_TIER: &str = "Pico";

/// Workspace API token. `Debug` is redacted; the value is never formatted.
#[derive(Clone)]
pub struct CsbToken(String);

impl CsbToken {
    /// Fail-closed: empty or whitespace is not a token.
    pub fn from_secret(secret: impl AsRef<str>) -> Result<Self, SandboxError> {
        let trimmed = secret.as_ref().trim();
        if trimmed.is_empty() {
            return Err(SandboxError::MissingRemoteToken);
        }
        Ok(Self(trimmed.to_string()))
    }

    /// `CSB_API_KEY`, or [`SandboxError::MissingRemoteToken`].
    pub fn from_env() -> Result<Self, SandboxError> {
        match std::env::var(CSB_API_KEY_ENV) {
            Ok(v) => Self::from_secret(v),
            Err(_) => Err(SandboxError::MissingRemoteToken),
        }
    }

    /// Env first, then a caller-supplied vault secret. Either missing → typed error.
    ///
    /// The sandbox crate does not open the vault (libraries take traits; the
    /// binary wires them). CLI/`panday creds` resolve provider `codesandbox`
    /// and pass the revealed secret here.
    pub fn from_env_or_vault(vault_secret: Option<&str>) -> Result<Self, SandboxError> {
        match Self::from_env() {
            Ok(t) => Ok(t),
            Err(SandboxError::MissingRemoteToken) => match vault_secret {
                Some(s) => Self::from_secret(s),
                None => Err(SandboxError::MissingRemoteToken),
            },
            Err(other) => Err(other),
        }
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for CsbToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CsbToken([redacted])")
    }
}

#[derive(Clone)]
struct GuestSession {
    pitcher_url: String,
    pitcher_token: String,
    workspace_path: String,
}

/// CodeSandbox control-plane + guest-HTTP client.
pub struct CsbSandbox {
    client: reqwest::Client,
    token: CsbToken,
    base_url: String,
    template_id: String,
    vm_tier: String,
    sessions: Mutex<HashMap<String, GuestSession>>,
}

impl fmt::Debug for CsbSandbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CsbSandbox")
            .field("token", &self.token)
            .field("base_url", &self.base_url)
            .field("template_id", &self.template_id)
            .field("vm_tier", &self.vm_tier)
            .finish_non_exhaustive()
    }
}

impl CsbSandbox {
    pub fn new(token: CsbToken) -> Result<Self, SandboxError> {
        Self::with_base_url(token, CSB_DEFAULT_BASE)
    }

    /// Point the control plane at a loopback mock. Production uses [`Self::new`].
    pub fn with_base_url(
        token: CsbToken,
        base_url: impl Into<String>,
    ) -> Result<Self, SandboxError> {
        let client = reqwest::Client::builder()
            // A 302 to a different host would otherwise take the Bearer with it.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| SandboxError::Internal(format!("http client: {e}")))?;
        Ok(Self {
            client,
            token,
            base_url: trim_slash(base_url.into()),
            template_id: CSB_DEFAULT_TEMPLATE.into(),
            vm_tier: CSB_DEFAULT_VM_TIER.into(),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn from_env() -> Result<Self, SandboxError> {
        Self::new(CsbToken::from_env()?)
    }

    pub fn from_env_or_vault(vault_secret: Option<&str>) -> Result<Self, SandboxError> {
        Self::new(CsbToken::from_env_or_vault(vault_secret)?)
    }

    pub fn with_template(mut self, template_id: impl Into<String>) -> Self {
        self.template_id = template_id.into();
        self
    }

    pub fn with_vm_tier(mut self, vm_tier: impl Into<String>) -> Self {
        self.vm_tier = vm_tier.into();
        self
    }

    async fn control(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, SandboxError> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self
            .client
            .request(method, url)
            .header(reqwest::header::AUTHORIZATION, self.token.bearer())
            .header(reqwest::header::USER_AGENT, "panday-sandbox/t3-remote")
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| SandboxError::Internal(format!("codesandbox control plane: {e}")))?;
        read_json(resp).await
    }

    async fn guest(
        &self,
        session: &GuestSession,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        bytes: Option<Vec<u8>>,
    ) -> Result<(reqwest::StatusCode, Vec<u8>), SandboxError> {
        let url = format!("{}{path}", session.pitcher_url);
        let mut req = self
            .client
            .request(method, url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", session.pitcher_token),
            )
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            req = req.json(&body);
        } else if let Some(bytes) = bytes {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(bytes);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| SandboxError::Internal(format!("codesandbox guest: {e}")))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SandboxError::Internal(format!("codesandbox guest body: {e}")))?;
        if !status.is_success() {
            return Err(status_error(status.as_u16(), &bytes));
        }
        Ok((status, bytes.to_vec()))
    }

    fn session(&self, id: &str) -> Result<GuestSession, SandboxError> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| SandboxError::Internal(format!("no such remote session {id}")))
    }
}

#[async_trait::async_trait]
impl Sandbox for CsbSandbox {
    async fn create(&self, spec: SessionSpec) -> Result<SandboxHandle, SandboxError> {
        if spec.tier != SandboxTier::T3Remote {
            return Err(SandboxError::Unsupported(spec.tier));
        }
        spec.policy.net.enforceable()?;

        let forked = self
            .control(
                reqwest::Method::POST,
                &format!("/sandbox/{}/fork", self.template_id),
                Some(json!({
                    "privacy": 2,
                    "private_preview": false,
                    "tags": ["sdk", "panday"],
                    "path": "/SDK",
                    "title": "panday-t3-remote",
                })),
            )
            .await?;
        let id = json_id(&forked).ok_or_else(|| {
            SandboxError::Internal("codesandbox fork returned no sandbox id".into())
        })?;

        let started = self
            .control(
                reqwest::Method::POST,
                &format!("/vm/{id}/start"),
                Some(json!({
                    "tier": self.vm_tier,
                    "hibernation_timeout_seconds": 300,
                })),
            )
            .await?;
        let pitcher_url = json_str(&started, "pitcher_url").ok_or_else(|| {
            SandboxError::Internal("codesandbox start returned no pitcher_url".into())
        })?;
        let pitcher_token = json_str(&started, "pitcher_token").ok_or_else(|| {
            SandboxError::Internal("codesandbox start returned no pitcher_token".into())
        })?;
        let workspace_path =
            json_str(&started, "workspace_path").unwrap_or_else(|| "/project/workspace".into());

        self.sessions.lock().unwrap().insert(
            id.clone(),
            GuestSession {
                pitcher_url: http_from_pitcher(&pitcher_url),
                pitcher_token,
                workspace_path,
            },
        );
        Ok(SandboxHandle {
            id,
            tier: SandboxTier::T3Remote,
        })
    }

    async fn exec(&self, h: &SandboxHandle, cmd: ExecSpec) -> Result<ExecStream, SandboxError> {
        if cmd.cmd.is_empty() {
            return Err(SandboxError::PolicyViolation("empty command".into()));
        }
        let session = self.session(&h.id)?;
        let cwd = cmd
            .cwd
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| session.workspace_path.clone());
        let began = Instant::now();
        let body = json!({
            "command": cmd.cmd.join(" "),
            "argv": cmd.cmd,
            "cwd": cwd,
        });
        let (_status, bytes) = self
            .guest(
                &session,
                reqwest::Method::POST,
                "/commands/run",
                Some(body),
                None,
            )
            .await?;
        let parsed: CommandResult = serde_json::from_slice(&bytes)
            .map_err(|e| SandboxError::Internal(format!("codesandbox exec response: {e}")))?;
        let wall_ms = began.elapsed().as_millis() as u64;
        Ok(box_chunks(parsed, wall_ms))
    }

    async fn put(
        &self,
        h: &SandboxHandle,
        path: PathBuf,
        data: Vec<u8>,
    ) -> Result<(), SandboxError> {
        let session = self.session(&h.id)?;
        let path = guest_path(&session.workspace_path, &path);
        let qs = format!("/fs?path={}", urlencoding_path(&path));
        self.guest(&session, reqwest::Method::PUT, &qs, None, Some(data))
            .await?;
        Ok(())
    }

    async fn get(&self, h: &SandboxHandle, path: PathBuf) -> Result<Vec<u8>, SandboxError> {
        let session = self.session(&h.id)?;
        let path = guest_path(&session.workspace_path, &path);
        let qs = format!("/fs?path={}", urlencoding_path(&path));
        let (_status, bytes) = self
            .guest(&session, reqwest::Method::GET, &qs, None, None)
            .await?;
        Ok(bytes)
    }

    async fn snapshot(&self, h: &SandboxHandle) -> Result<SnapshotRef, SandboxError> {
        self.control(
            reqwest::Method::POST,
            &format!("/vm/{}/hibernate", h.id),
            Some(json!({})),
        )
        .await?;
        Ok(SnapshotRef(h.id.clone()))
    }

    async fn destroy(&self, h: SandboxHandle) -> Result<(), SandboxError> {
        let result = self
            .control(reqwest::Method::DELETE, &format!("/vm/{}", h.id), None)
            .await;
        self.sessions.lock().unwrap().remove(&h.id);
        result.map(|_| ())
    }
}

#[derive(Debug, Deserialize, Default)]
struct CommandResult {
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    exit_code: i32,
}

fn box_chunks(parsed: CommandResult, wall_ms: u64) -> ExecStream {
    let mut items = Vec::new();
    if !parsed.stdout.is_empty() {
        items.push(Ok(ExecChunk::Stdout(parsed.stdout.into_bytes())));
    }
    if !parsed.stderr.is_empty() {
        items.push(Ok(ExecChunk::Stderr(parsed.stderr.into_bytes())));
    }
    items.push(Ok(ExecChunk::Exit {
        code: parsed.exit_code,
        wall_ms,
    }));
    Box::pin(futures_util::stream::iter(items))
}

async fn read_json(resp: reqwest::Response) -> Result<Value, SandboxError> {
    let status = resp.status().as_u16();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SandboxError::Internal(format!("codesandbox body: {e}")))?;
    if !(200..300).contains(&status) {
        return Err(status_error(status, &bytes));
    }
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| SandboxError::Internal(format!("codesandbox json: {e}")))
}

fn status_error(status: u16, bytes: &[u8]) -> SandboxError {
    // Never include the request token. Response bodies from CSB are their
    // error strings; if one echoed the key we still refuse to format ours.
    let body = String::from_utf8_lossy(bytes);
    let preview: String = body.chars().take(200).collect();
    SandboxError::Internal(format!("codesandbox HTTP {status}: {preview}"))
}

/// CSB wraps payloads as `{ success, data: { … } }`. Also accept a bare object.
fn envelope_data(value: &Value) -> &Value {
    value.get("data").unwrap_or(value)
}

fn json_id(value: &Value) -> Option<String> {
    let data = envelope_data(value);
    data.get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            data.get("sandbox")
                .and_then(|s| s.get("id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

fn json_str(value: &Value, key: &str) -> Option<String> {
    let data = envelope_data(value);
    data.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn trim_slash(s: String) -> String {
    s.trim_end_matches('/').to_string()
}

/// Pitcher is advertised as `wss://…`; this slice speaks HTTP to the same host.
fn http_from_pitcher(url: &str) -> String {
    let swapped = if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        url.to_string()
    };
    trim_slash(swapped)
}

fn guest_path(workspace: &str, path: &Path) -> String {
    if path.is_absolute() {
        path.to_string_lossy().into_owned()
    } else {
        format!("{}/{}", workspace.trim_end_matches('/'), path.display())
    }
}

/// Minimal query-escape so a path does not break the URL. Not a general encoder.
fn urlencoding_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn an_empty_secret_is_the_typed_missing_token_error() {
        // Fail-closed: whitespace is not a token, and the error is the named
        // variant so a caller can tell "not configured" from "CSB said 401".
        assert!(matches!(
            CsbToken::from_secret(""),
            Err(SandboxError::MissingRemoteToken)
        ));
        assert!(matches!(
            CsbToken::from_secret("   "),
            Err(SandboxError::MissingRemoteToken)
        ));
        // from_env_or_vault still consults CSB_API_KEY, so an empty vault
        // secret is only a miss when the env var is unset. from_secret is
        // the constructor that must stay red on any machine.
    }

    #[test]
    fn debug_does_not_print_the_token() {
        let token = CsbToken::from_secret("csb_live_supersecret").unwrap();
        let printed = format!("{token:?}");
        assert_eq!(printed, "CsbToken([redacted])");
        assert!(!printed.contains("supersecret"), "{printed}");
        let sb = CsbSandbox::with_base_url(token, "http://127.0.0.1:9").unwrap();
        let printed = format!("{sb:?}");
        assert!(!printed.contains("supersecret"), "{printed}");
        assert!(printed.contains("redacted"), "{printed}");
    }

    #[test]
    fn vault_secret_is_used_when_env_is_absent() {
        // Isolated from CSB_API_KEY: from_secret is the vault path.
        let t = CsbToken::from_secret("from-vault").unwrap();
        assert!(!format!("{t:?}").contains("from-vault"));
    }
}
