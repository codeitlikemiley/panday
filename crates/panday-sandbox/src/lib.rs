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
    /// Always true: all egress via the logging proxy (docs/14 §policy).
    pub via_proxy: bool,
    /// Domain patterns allowed through the proxy; empty = full deny.
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
#[cfg(target_os = "linux")]
pub use t2_linux::T2LinuxSandbox;
#[cfg(target_os = "macos")]
pub use t2_macos::{SeatbeltProfile, T2MacosSandbox};

pub use t0::{Access, T0Sandbox};

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
