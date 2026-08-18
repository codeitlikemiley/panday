//! T2 on macOS — Seatbelt profile generation (docs/14, M14.3).
//!
//! docs/14 names the mechanism: "macOS = `sandbox-exec` Seatbelt profiles",
//! the same shape Anthropic's sandbox-runtime validated. We generate an SBPL
//! profile per session and run each command under it.
//!
//! ## What this tier actually guarantees on macOS
//!
//! Measured, not assumed — each is covered by the escape suite:
//!
//! | Guarantee | Enforced by | Status |
//! |---|---|---|
//! | No write outside the workspace | Seatbelt `file-write*` allowlist | **strict** |
//! | No network egress | Seatbelt `(deny network*)` | **strict** |
//! | Wall-clock ceiling | us (SIGKILL after the deadline) | **strict** |
//! | No read of sensitive paths | Seatbelt `file-read*` **deny**list | *partial* |
//! | Memory / pid ceilings | — | **not enforced** |
//!
//! ### Why reads are a denylist here, and Linux's are not
//!
//! A strict read *allowlist* is not achievable through Seatbelt in practice:
//! the dynamic loader and the shared cache touch paths that vary by macOS
//! version and by APFS firmlink layout, and enumerating them is a losing
//! game — every profile we tried that listed top-level directories aborted
//! `/bin/echo` with SIGABRT before `main`. Only `(subpath "/")` — i.e. no
//! scoping at all — reliably lets a binary start.
//!
//! So this tier allows broad reads and *denies* the paths that actually
//! matter (credentials, keys, shell history), which is weaker than Linux T2's
//! mount-namespace scoping and is stated as such rather than papered over.
//! The strong guarantees above are the ones this tier is trusted for; a
//! deployment that needs read confinement wants T3 (ADR-004).
//!
//! macOS is also the tier where **the user is the trust anchor** (docs/14):
//! this confines the user's own agent on the user's own machine. Strangers'
//! code belongs in T3, which is Linux-only.

use crate::{
    ExecChunk, ExecSpec, ExecStream, FsPolicy, Limits, NetPolicy, Sandbox, SandboxError,
    SandboxHandle, SandboxPolicy, SandboxTier, SessionSpec, SnapshotRef,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Paths denied for reading, relative to the user's home directory.
///
/// Not exhaustive — a denylist never is — but it covers the credentials an
/// agent could plausibly exfiltrate through tool output (docs/20 T4).
const SENSITIVE_HOME_SUBPATHS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".config/gcloud",
    ".config/gh",
    ".cargo/credentials.toml",
    "Library/Keychains",
    "Library/Application Support/Google/Chrome",
    ".bash_history",
    ".zsh_history",
];

const SENSITIVE_ABSOLUTE: &[&str] = &["/etc/master.passwd", "/etc/shadow", "/etc/sudoers"];

/// Escape a path for inclusion in an SBPL string literal.
///
/// Profile text is generated from paths we do not fully control (a workspace
/// under a directory the user named). A path containing a quote could close
/// the literal and inject directives — `(allow default)` would do nicely — so
/// quotes and backslashes are escaped and control characters are refused
/// outright rather than escaped into something ambiguous.
fn sbpl_string(path: &Path) -> Result<String, SandboxError> {
    let s = path.to_str().ok_or_else(|| {
        SandboxError::PolicyViolation(format!("path is not valid UTF-8: {}", path.display()))
    })?;

    if s.chars().any(|c| c.is_control()) {
        return Err(SandboxError::PolicyViolation(format!(
            "path contains control characters and cannot be expressed in a profile: {s:?}"
        )));
    }

    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

/// A generated Seatbelt profile.
#[derive(Debug, Clone, PartialEq)]
pub struct SeatbeltProfile(pub String);

impl SeatbeltProfile {
    /// Build a profile from the uniform policy (docs/14 §policy).
    pub fn from_policy(fs: &FsPolicy, net: &NetPolicy) -> Result<Self, SandboxError> {
        let mut p = String::new();
        p.push_str("(version 1)\n");
        // Everything not explicitly permitted below is refused.
        p.push_str("(deny default)\n");

        // Enough to let a process start and run children.
        p.push_str("(allow process-exec*)\n");
        p.push_str("(allow process-fork)\n");
        p.push_str("(allow sysctl-read)\n");
        p.push_str("(allow signal (target self))\n");
        p.push_str("(allow mach-lookup)\n");
        p.push_str("(allow ipc-posix-shm)\n");

        // Broad read — see the module note on why this cannot be an allowlist.
        p.push_str("(allow file-read*)\n");

        // ...then claw back the paths worth protecting. Later rules win in
        // SBPL, so these override the blanket allow above.
        for abs in SENSITIVE_ABSOLUTE {
            p.push_str(&format!(
                "(deny file-read* (literal {}))\n",
                sbpl_string(Path::new(abs))?
            ));
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            for rel in SENSITIVE_HOME_SUBPATHS {
                let full = home.join(rel);
                p.push_str(&format!(
                    "(deny file-read* (subpath {}))\n",
                    sbpl_string(&full)?
                ));
            }
        }

        // Writes: the workspace only. This is the tier's strongest guarantee.
        p.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            sbpl_string(&fs.workspace_rw)?
        ));
        // Staged inputs are deliberately absent from the write set — readable,
        // never writable (docs/14 §workspace lifecycle).

        // Shell redirection and terminal output would otherwise fail.
        for dev in [
            "/dev/null",
            "/dev/stdout",
            "/dev/stderr",
            "/dev/tty",
            "/dev/dtracehelper",
        ] {
            p.push_str(&format!(
                "(allow file-write* (literal {}))\n",
                sbpl_string(Path::new(dev))?
            ));
        }
        p.push_str("(allow file-ioctl (literal \"/dev/tty\"))\n");

        // Network: default-deny (docs/14 §policy). An allowlist entry cannot
        // be expressed per-domain in Seatbelt — DNS names are resolved before
        // the syscall — so a non-empty allowlist means "egress permitted, and
        // the proxy does the filtering" (the proxy is M14.2's component).
        if net.allow.is_empty() {
            p.push_str("(deny network*)\n");
        } else {
            p.push_str("(allow network-outbound)\n");
            p.push_str("(allow network-bind)\n");
        }

        Ok(SeatbeltProfile(p))
    }
}

#[derive(Debug, Clone)]
struct Session {
    workspace: PathBuf,
    profile_path: PathBuf,
    limits: Limits,
    /// Explicitly injected environment (docs/14 §policy: "env is scrubbed;
    /// injection is explicit"). Nothing is inherited from our process.
    env: Vec<(String, String)>,
}

/// Session ids must be unique across the whole PROCESS, not per instance.
///
/// A per-instance counter looked fine and was not: every `T2MacosSandbox`
/// starts at 0, so two instances in one process both produce `t2m-0` and
/// therefore the same profile path. The second `create` overwrites the
/// first's profile, and the first session then executes under the *other*
/// session's workspace scoping — writes to its own workspace start failing,
/// and worse, its jail is defined by someone else's policy.
///
/// `panday-harnessd` hosts many sessions per process, so this is a
/// production-shaped bug; the concurrent escape suite is what surfaced it.
static SESSION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// T2 for macOS.
#[derive(Default)]
pub struct T2MacosSandbox {
    sessions: Mutex<HashMap<String, Session>>,
}

impl T2MacosSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when this host can actually run the tier.
    pub fn available() -> bool {
        cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists()
    }

    /// The generated profile for a live session — exposed so the escape suite
    /// can assert on what was actually written, not on what we meant.
    pub fn profile_text(&self, h: &SandboxHandle) -> Option<String> {
        let path = self
            .sessions
            .lock()
            .unwrap()
            .get(&h.id)?
            .profile_path
            .clone();
        std::fs::read_to_string(path).ok()
    }
}

#[async_trait::async_trait]
impl Sandbox for T2MacosSandbox {
    async fn create(&self, spec: SessionSpec) -> Result<SandboxHandle, SandboxError> {
        if spec.tier != SandboxTier::T2OsJail {
            return Err(SandboxError::Unsupported(spec.tier));
        }
        if !Self::available() {
            return Err(SandboxError::Internal(
                "T2 macOS needs /usr/bin/sandbox-exec; use T2 Linux or T0 on this host".into(),
            ));
        }

        let SandboxPolicy {
            fs,
            net,
            limits,
            env: injected_env,
        } = spec.policy;
        let workspace = std::fs::canonicalize(&fs.workspace_rw).map_err(|e| {
            SandboxError::PolicyViolation(format!(
                "workspace {} is unusable: {e}",
                fs.workspace_rw.display()
            ))
        })?;

        let mut staged = Vec::new();
        for p in &fs.staged_ro {
            staged.push(std::fs::canonicalize(p).map_err(|e| {
                SandboxError::PolicyViolation(format!(
                    "staged input {} is unusable: {e}",
                    p.display()
                ))
            })?);
        }

        let resolved_fs = FsPolicy {
            workspace_rw: workspace.clone(),
            staged_ro: staged,
        };
        let profile = SeatbeltProfile::from_policy(&resolved_fs, &net)?;

        let id = format!(
            "t2m-{}",
            SESSION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        // `sandbox-exec -p` takes the profile on the command line, where it
        // would be visible in `ps` and subject to argv length limits; a file
        // avoids both.
        let profile_path =
            std::env::temp_dir().join(format!("panday-{}-{}.sb", std::process::id(), id));
        std::fs::write(&profile_path, &profile.0)
            .map_err(|e| SandboxError::Internal(format!("write profile: {e}")))?;

        self.sessions.lock().unwrap().insert(
            id.clone(),
            Session {
                workspace,
                profile_path,
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
        use tokio::io::AsyncReadExt;

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

        let cwd = cmd.cwd.unwrap_or_else(|| session.workspace.clone());

        let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-f")
            .arg(&session.profile_path)
            .args(&cmd.cmd)
            .current_dir(&cwd)
            // Secrets are never inherited (docs/20 T4): the child gets a
            // deliberately minimal environment, not ours.
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", &session.workspace)
            .env("TMPDIR", &session.workspace);

        // Explicit injection last, so a policy may deliberately widen PATH
        // (a toolchain lives outside the workspace) without the jail ever
        // inheriting our environment.
        for (k, v) in &session.env {
            command.env(k, v);
        }

        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|e| SandboxError::Internal(format!("spawn sandbox-exec: {e}")))?;

        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let wall_ms = session.limits.wall_clock_ms;

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ExecChunk, SandboxError>>(64);

        tokio::spawn(async move {
            let began = std::time::Instant::now();
            let mut out_buf = [0u8; 8192];
            let mut err_buf = [0u8; 8192];
            let mut out_done = false;
            let mut err_done = false;

            let deadline = tokio::time::sleep(std::time::Duration::from_millis(wall_ms));
            tokio::pin!(deadline);

            loop {
                tokio::select! {
                    // Biased so output already buffered is drained before the
                    // deadline arm can fire — otherwise a command that
                    // finishes exactly at the limit loses its last chunk.
                    biased;

                    n = stdout.read(&mut out_buf), if !out_done => match n {
                        Ok(0) => out_done = true,
                        Ok(n) => {
                            // A send failure means the consumer dropped the
                            // stream — i.e. the caller cancelled. Killing the
                            // child here is what makes cancellation actually
                            // stop work rather than merely stop listening to
                            // it (docs/13 §cancellation).
                            if tx.send(Ok(ExecChunk::Stdout(out_buf[..n].to_vec()))).await.is_err() {
                                let _ = child.kill().await;
                                return;
                            }
                        }
                        Err(_) => out_done = true,
                    },
                    n = stderr.read(&mut err_buf), if !err_done => match n {
                        Ok(0) => err_done = true,
                        Ok(n) => {
                            if tx.send(Ok(ExecChunk::Stderr(err_buf[..n].to_vec()))).await.is_err() {
                                let _ = child.kill().await;
                                return;
                            }
                        }
                        Err(_) => err_done = true,
                    },
                    // A command that produces no output would otherwise never
                    // notice the consumer is gone, so poll for closure too.
                    _ = tx.closed() => {
                        let _ = child.kill().await;
                        return;
                    }
                    _ = &mut deadline => {
                        // docs/13 §cancellation: SIGKILL is the backstop. The
                        // graceful SIGTERM path belongs with cancellation
                        // (M13.3); a wall-clock breach is already a failure.
                        let _ = child.kill().await;
                        let _ = tx.send(Err(SandboxError::LimitExceeded(format!(
                            "wall clock exceeded {wall_ms}ms"
                        )))).await;
                        return;
                    }
                    status = child.wait(), if out_done && err_done => {
                        let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
                        let _ = tx.send(Ok(ExecChunk::Exit {
                            code,
                            wall_ms: began.elapsed().as_millis() as u64,
                        })).await;
                        return;
                    }
                }
            }
        });

        Ok(Box::pin(futures_util::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|item| (item, rx)) },
        )))
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
        if let Some(s) = self.sessions.lock().unwrap().remove(&h.id) {
            let _ = std::fs::remove_file(s.profile_path);
        }
        Ok(())
    }
}

impl T2MacosSandbox {
    /// `put`/`get` run in OUR process, outside the jail, so the Seatbelt
    /// profile does not cover them — they get the same in-code check T0 uses.
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
