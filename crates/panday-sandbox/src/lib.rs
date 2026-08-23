//! # panday-sandbox
//!
//! Four trust problems, one trait (docs/14-sandbox.md, ADR-004).
//! This seed defines the trait, tiers and policy types; mechanisms land in
//! M14.2 (T2 Linux), M14.4 (T1 wasmtime), M14.5 (T3 Firecracker).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxTier {
    /// In-process Rust fn; path policy enforced in code; no exec.
    T0InProcess,
    /// wasmtime component; capability = granted WIT imports only.
    T1Wasm,
    /// OS jail: namespaces+seccomp (Linux) / Seatbelt (macOS); egress-deny proxy.
    T2OsJail,
    /// Firecracker microVM; hardware isolation; cloud multi-tenant.
    T3MicroVm,
    /// Hosted microVM (CodeSandbox / Together) when this host has no KVM.
    /// Same isolation *class* as [`Self::T3MicroVm`]; different host and bill.
    T3Remote,
}

impl SandboxTier {
    /// The wire name — same string serde emits, so a metric label, a log line
    /// and a policy file all agree (`t2_os_jail`, never `T2OsJail`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::T0InProcess => "t0_in_process",
            Self::T1Wasm => "t1_wasm",
            Self::T2OsJail => "t2_os_jail",
            Self::T3MicroVm => "t3_micro_vm",
            Self::T3Remote => "t3_remote",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FsPolicy {
    pub workspace_rw: PathBuf,
    #[serde(default)]
    pub staged_ro: Vec<PathBuf>,
    // everything else: deny.
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetPolicy {
    /// Reserved for the egress proxy docs/14 §policy describes. No tier reads it: there is no
    /// proxy, so there is nothing for it to switch between.
    pub via_proxy: bool,
    /// Domain patterns that would be allowed through the proxy.
    ///
    /// **Refused, not honoured** — see [`NetPolicy::enforceable`]. Empty is the only accepted
    /// value today, and it means no egress at all.
    #[serde(default)]
    pub allow: Vec<String>,
}

impl Default for NetPolicy {
    fn default() -> Self {
        Self {
            via_proxy: true,
            allow: vec![],
        } // default-deny
    }
}

impl NetPolicy {
    /// Refuse a policy no tier can enforce, before anything is spawned.
    ///
    /// docs/14 specifies a per-domain allowlist served by an egress proxy. The proxy is not built
    /// (M14.8), so no tier can distinguish `api.github.com` from anything else — and the honest
    /// answer to "may this reach exactly these hosts?" is an error, not a guess.
    ///
    /// It is an error rather than a silent downgrade to full deny for two reasons. A caller that
    /// asked for network and got none would fail later, somewhere less obvious, with a timeout
    /// instead of a reason. And the previous behaviour was worse than a downgrade: T2 Linux read a
    /// non-empty allowlist as "do not unshare the network namespace", so asking for one host
    /// granted **the host's entire network** — the opposite of what the field reads like. Nothing
    /// constructed such a policy, so it was never reachable, but it was one caller away.
    pub fn enforceable(&self) -> Result<(), SandboxError> {
        if self.allow.is_empty() {
            return Ok(());
        }
        Err(SandboxError::PolicyViolation(format!(
            "net.allow names {} host(s) and no tier can enforce a per-domain allowlist: the egress \
             proxy docs/14 §policy describes is not built. Leave `allow` empty for no egress. \
             Refused rather than approximated — granting more than was asked for is how this used \
             to behave, and granting less would fail later as a timeout with no reason attached.",
            self.allow.len()
        )))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Limits {
    pub cpu_ms: u64,
    pub mem_bytes: u64,
    pub pids: u32,
    pub disk_bytes: u64,
    pub wall_clock_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            cpu_ms: 60_000,
            mem_bytes: 2 << 30,
            pids: 256,
            disk_bytes: 4 << 30,
            wall_clock_ms: 120_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxPolicy {
    pub fs: FsPolicy,
    pub net: NetPolicy,
    pub limits: Limits,
    /// Secrets are injected explicitly per-tool, never inherited (docs/20 T4).
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    pub tier: SandboxTier,
    pub policy: SandboxPolicy,
}

#[derive(Debug, Clone)]
pub struct SandboxHandle {
    pub id: String,
    pub tier: SandboxTier,
}

#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub cmd: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub pty: bool,
    pub stdin: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub enum ExecChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit { code: i32, wall_ms: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("unsupported on tier {0:?}")]
    Unsupported(SandboxTier),
    #[error("policy violation: {0}")]
    PolicyViolation(String),
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("sandbox failure: {0}")]
    Internal(String),
    /// T3-remote is opt-in and fail-closed: no token means no hosted VM, never
    /// a silent downgrade to T2 and never a harvested farm of free accounts.
    #[error(
        "T3-remote is fail-closed without a CodeSandbox token (CSB_API_KEY or vault provider codesandbox)"
    )]
    MissingRemoteToken,
}

pub mod exec_stream;
pub mod path;
pub mod t0;
pub mod t1_hook;
pub mod t1_wasm;
#[cfg(target_os = "linux")]
pub mod t2_linux;
#[cfg(target_os = "macos")]
pub mod t2_macos;
/// T3 compiles everywhere and *runs* only where there is KVM: the API client, the jailer's
/// arguments and the boot sequence are protocol and argv, and both are just as wrong on a Mac as on
/// a hypervisor host. Type-checking them on every platform is free; pretending to run them is not.
pub mod t3;
/// T3-remote — CodeSandbox / Together microVMs when this host has no KVM (docs/14 M14.9).
pub mod t3_remote;
#[cfg(target_os = "linux")]
pub use t2_linux::T2LinuxSandbox;
#[cfg(target_os = "macos")]
pub use t2_macos::{SeatbeltProfile, T2MacosSandbox};

pub use t0::{Access, T0Sandbox};
pub use t3_remote::{CsbSandbox, CsbToken, CSB_API_KEY_ENV, CSB_VAULT_PROVIDER};

pub type ExecStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<ExecChunk, SandboxError>> + Send>>;

#[derive(Debug, Clone)]
pub struct SnapshotRef(pub String);

#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn create(&self, spec: SessionSpec) -> Result<SandboxHandle, SandboxError>;
    async fn exec(&self, h: &SandboxHandle, cmd: ExecSpec) -> Result<ExecStream, SandboxError>;
    async fn put(
        &self,
        h: &SandboxHandle,
        path: PathBuf,
        data: Vec<u8>,
    ) -> Result<(), SandboxError>;
    async fn get(&self, h: &SandboxHandle, path: PathBuf) -> Result<Vec<u8>, SandboxError>;
    /// T3 only; others return `Err(Unsupported)`.
    async fn snapshot(&self, h: &SandboxHandle) -> Result<SnapshotRef, SandboxError>;
    async fn destroy(&self, h: SandboxHandle) -> Result<(), SandboxError>;
}

#[cfg(test)]
mod net_policy_tests {
    use super::*;

    #[test]
    fn the_default_policy_is_enforceable_because_it_asks_for_nothing() {
        NetPolicy::default().enforceable().expect("empty is fine");
    }

    #[test]
    fn a_named_host_is_refused_rather_than_approximated() {
        // The two wrong answers this guards against. Granting more than was asked for is what T2
        // actually did — a non-empty allowlist dropped the network namespace on Linux and emitted
        // `(allow network-outbound)` on macOS. Granting less, by quietly downgrading to full deny,
        // would surface as a timeout somewhere later with no reason attached.
        let policy = NetPolicy {
            via_proxy: true,
            allow: vec!["api.github.com".into()],
        };
        let err = policy
            .enforceable()
            .expect_err("a named host must be refused");
        let message = err.to_string();
        assert!(
            message.contains("net.allow") && message.contains("proxy"),
            "the refusal must say which field and why: {message}"
        );
    }
}
