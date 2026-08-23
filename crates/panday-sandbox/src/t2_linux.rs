//! T2 on Linux — namespaces + seccomp via bubblewrap (docs/14, M14.2).
//!
//! docs/14 names the mechanism: "Linux = bubblewrap + seccomp BPF ... `bwrap`
//! vendored/invoked or direct `clone3`+namespaces". We invoke `bwrap`, which
//! is the option the spec lists first and avoids `unsafe` in a crate whose
//! whole job is containment.
//!
//! ## Why this tier is stronger than its macOS sibling
//!
//! Reads here are a genuine **allowlist**, not a denylist. Nothing is visible
//! inside the jail unless it was explicitly bound in, so `/etc/shadow` is not
//! "denied" — it does not exist. That is the parity gap recorded in M14.3:
//! Seatbelt could not express a read allowlist without breaking the dynamic
//! loader, and mount namespaces can.
//!
//! | Guarantee | Mechanism | Status |
//! |---|---|---|
//! | No read outside the bound set | mount namespace | **strict** |
//! | No write outside the workspace | only the workspace is bound rw | **strict** |
//! | No network egress | `--unshare-net` | **strict** |
//! | Wall-clock ceiling | us (SIGKILL at the deadline) | **strict** |
//! | No parent env inherited | `env_clear` + `--clearenv` | **strict** |
//! | pid ceiling | pid namespace + `--die-with-parent` | *partial* |
//! | Memory ceiling | needs cgroup v2 delegation | **not enforced** |

use crate::{
    ExecSpec, ExecStream, FsPolicy, Limits, Sandbox, SandboxError, SandboxHandle, SandboxPolicy,
    SandboxTier, SessionSpec, SnapshotRef,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Read-only system paths bound into every jail.
///
/// Without these nothing can execute: the dynamic loader, libc and the shell
/// all live outside the workspace. They are bound **read-only**, so this
/// widens what can be read, never what can be written.
const SYSTEM_RO: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc/alternatives",
];

static SESSION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone)]
struct Session {
    workspace: PathBuf,
    staged_ro: Vec<PathBuf>,
    limits: Limits,
    // No `net`. It was here so `build_args` could decide whether to unshare the network
    // namespace; that decision is gone — the answer is always yes — and `NetPolicy::enforceable`
    // guarantees at `create` that `allow` is empty, so a stored copy would record a constant.
    // When the egress proxy exists, what belongs here is the proxy's endpoint, not the policy.
    env: Vec<(String, String)>,
}

#[derive(Default)]
pub struct T2LinuxSandbox {
    sessions: Mutex<HashMap<String, Session>>,
}

impl T2LinuxSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when this host can *actually* run the tier.
    ///
    /// Probes rather than just checking the binary exists. Unprivileged user
    /// namespaces are commonly restricted — Ubuntu 24.04 does it by default
    /// via AppArmor, and there `bwrap` is installed and on PATH but every
    /// jail dies with `loopback: Failed RTM_NEWADDR`. A presence check would
    /// report the tier as available and then fail every call confusingly;
    /// probing turns that into one clear answer.
    ///
    /// Cached: this spawns a process, and callers ask per session.
    pub fn available() -> bool {
        static PROBE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *PROBE.get_or_init(|| {
            let Some(bwrap) = which_bwrap() else {
                return false;
            };
            // Probe with the same bind construction the real jails use, so a
            // merged-/usr mistake fails here rather than in every session.
            let probe_session = Session {
                workspace: std::env::temp_dir(),
                staged_ro: vec![],
                limits: Limits::default(),
                env: vec![],
            };
            let mut args = build_args(&probe_session);
            args.push("--".into());
            args.push("/usr/bin/true".into());

            std::process::Command::new(bwrap)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }

    /// Why the tier is unavailable, for an operator staring at a skip.
    pub fn unavailable_reason() -> Option<String> {
        if Self::available() {
            return None;
        }
        Some(match which_bwrap() {
            None => "bubblewrap (`bwrap`) is not installed".to_string(),
            Some(_) => "`bwrap` is installed but cannot create a user namespace — \
                        unprivileged user namespaces are probably restricted \
                        (Ubuntu 24.04: kernel.apparmor_restrict_unprivileged_userns=1)"
                .to_string(),
        })
    }

    /// The exact `bwrap` argv for a session — exposed so the escape suite can
    /// assert on what is actually run rather than on what we intended.
    pub fn jail_args(&self, h: &SandboxHandle) -> Option<Vec<String>> {
        let s = self.sessions.lock().unwrap().get(&h.id)?.clone();
        Some(build_args(&s))
    }
}

fn which_bwrap() -> Option<PathBuf> {
    for dir in ["/usr/bin", "/bin", "/usr/local/bin"] {
        let p = Path::new(dir).join("bwrap");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Assemble the namespace configuration.
fn build_args(s: &Session) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let push = |a: &mut Vec<String>, v: &str| a.push(v.to_string());

    // Every namespace we can drop, dropped. `--unshare-net` is what makes
    // egress impossible rather than merely filtered.
    push(&mut a, "--unshare-user");
    push(&mut a, "--unshare-pid");
    push(&mut a, "--unshare-ipc");
    push(&mut a, "--unshare-uts");
    push(&mut a, "--unshare-cgroup-try");
    // Unconditional. This used to be gated on `s.net.allow.is_empty()`, which meant a non-empty
    // allowlist *dropped* the network namespace and handed the payload the host's whole network —
    // the loopback services, the LAN, the cloud metadata endpoint — while reading like a
    // restriction. `NetPolicy::enforceable` now refuses such a policy at `create`, so this branch
    // could not be reached either way; it is gone so that a future caller cannot resurrect it by
    // deleting the check.
    push(&mut a, "--unshare-net");
    // The jail must not outlive us; an orphaned sandbox is an escape of a
    // different kind.
    push(&mut a, "--die-with-parent");
    push(&mut a, "--new-session");

    // Nothing is visible unless bound. This is the allowlist macOS could not
    // express.
    for p in SYSTEM_RO {
        let path = Path::new(p);
        if !path.exists() {
            continue;
        }
        match std::fs::symlink_metadata(path) {
            // A merged-/usr symlink: recreate the link, do not bind it.
            Ok(meta) if meta.file_type().is_symlink() => {
                if let Ok(target) = std::fs::read_link(path) {
                    a.push("--symlink".into());
                    a.push(target.display().to_string());
                    a.push((*p).into());
                }
            }
            _ => {
                a.push("--ro-bind".into());
                a.push((*p).into());
                a.push((*p).into());
            }
        }
    }
    push(&mut a, "--proc");
    push(&mut a, "/proc");
    push(&mut a, "--dev");
    push(&mut a, "/dev");
    push(&mut a, "--tmpfs");
    push(&mut a, "/tmp");

    // The workspace, read-write. The only writable path in the jail.
    a.push("--bind".into());
    a.push(s.workspace.display().to_string());
    a.push(s.workspace.display().to_string());

    // Staged inputs: readable, never writable (docs/14 §workspace lifecycle).
    for p in &s.staged_ro {
        a.push("--ro-bind".into());
        a.push(p.display().to_string());
        a.push(p.display().to_string());
    }

    a.push("--chdir".into());
    a.push(s.workspace.display().to_string());

    // Secrets are never inherited (docs/20 T4); injection is explicit.
    push(&mut a, "--clearenv");
    a.push("--setenv".into());
    a.push("PATH".into());
    a.push("/usr/bin:/bin:/usr/sbin:/sbin".into());
    a.push("--setenv".into());
    a.push("HOME".into());
    a.push(s.workspace.display().to_string());
    a.push("--setenv".into());
    a.push("TMPDIR".into());
    a.push("/tmp".into());
    for (k, v) in &s.env {
        a.push("--setenv".into());
        a.push(k.clone());
        a.push(v.clone());
    }

    a
}

#[async_trait::async_trait]
impl Sandbox for T2LinuxSandbox {
    async fn create(&self, spec: SessionSpec) -> Result<SandboxHandle, SandboxError> {
        // Before anything is spawned: a policy no tier can enforce is refused, not approximated.
        spec.policy.net.enforceable()?;
        if spec.tier != SandboxTier::T2OsJail {
            return Err(SandboxError::Unsupported(spec.tier));
        }
        if !Self::available() {
            return Err(SandboxError::Internal(
                "T2 Linux needs bubblewrap (`bwrap`) on PATH; install it or use T0".into(),
            ));
        }

        let SandboxPolicy {
            fs,
            net: _,
            limits,
            env: injected_env,
        } = spec.policy;
        let FsPolicy {
            workspace_rw,
            staged_ro,
        } = fs;

        let workspace = std::fs::canonicalize(&workspace_rw).map_err(|e| {
            SandboxError::PolicyViolation(format!(
                "workspace {} is unusable: {e}",
                workspace_rw.display()
            ))
        })?;
        let mut staged = Vec::new();
        for p in staged_ro {
            staged.push(std::fs::canonicalize(&p).map_err(|e| {
                SandboxError::PolicyViolation(format!(
                    "staged input {} is unusable: {e}",
                    p.display()
                ))
            })?);
        }

        let id = format!(
            "t2l-{}",
            SESSION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        self.sessions.lock().unwrap().insert(
            id.clone(),
            Session {
                workspace,
                staged_ro: staged,
                limits,
                env: injected_env,
            },
        );

        Ok(SandboxHandle {
            id,
            tier: SandboxTier::T2OsJail,
        })
    }

    async fn exec(&self, h: &SandboxHandle, cmd: ExecSpec) -> Result<ExecStream, SandboxError> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .get(&h.id)
            .cloned()
            .ok_or_else(|| SandboxError::Internal(format!("no such session {}", h.id)))?;

        if cmd.cmd.is_empty() {
            return Err(SandboxError::PolicyViolation("empty command".into()));
        }

        let bwrap = which_bwrap()
            .ok_or_else(|| SandboxError::Internal("bwrap disappeared after create".into()))?;

        let mut command = tokio::process::Command::new(bwrap);
        command.args(build_args(&session));
        command.arg("--");
        command.args(&cmd.cmd);
        command.env_clear();

        crate::exec_stream::spawn_and_stream(command, session.limits.wall_clock_ms)
    }

    async fn put(
        &self,
        h: &SandboxHandle,
        path: PathBuf,
        data: Vec<u8>,
    ) -> Result<(), SandboxError> {
        let target = self.scoped(h, &path)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SandboxError::Internal(format!("mkdir: {e}")))?;
        }
        std::fs::write(&target, data).map_err(|e| SandboxError::Internal(format!("write: {e}")))
    }

    async fn get(&self, h: &SandboxHandle, path: PathBuf) -> Result<Vec<u8>, SandboxError> {
        let target = self.scoped(h, &path)?;
        std::fs::read(&target).map_err(|e| SandboxError::Internal(format!("read: {e}")))
    }

    async fn snapshot(&self, _h: &SandboxHandle) -> Result<SnapshotRef, SandboxError> {
        Err(SandboxError::Unsupported(SandboxTier::T2OsJail))
    }

    async fn destroy(&self, h: SandboxHandle) -> Result<(), SandboxError> {
        self.sessions.lock().unwrap().remove(&h.id);
        Ok(())
    }
}

impl T2LinuxSandbox {
    /// `put`/`get` run in OUR process, outside the jail, so they get the same
    /// in-code check T0 uses.
    fn scoped(&self, h: &SandboxHandle, path: &Path) -> Result<PathBuf, SandboxError> {
        let workspace = self
            .sessions
            .lock()
            .unwrap()
            .get(&h.id)
            .map(|s| s.workspace.clone())
            .ok_or_else(|| SandboxError::Internal("no such session".into()))?;

        crate::path::resolve_within(&workspace, &[&workspace], path, "access")
    }
}
