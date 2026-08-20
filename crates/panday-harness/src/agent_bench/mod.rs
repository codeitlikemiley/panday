//! `agent-bench` — repo tasks with verifiable rewards (docs/19 M19.6).
//!
//! > "agent-bench (50 verifiable repo tasks in T3) doubling as GRPO environment"
//!
//! # Why this is the second attempt
//!
//! The first version ran each task's verifier as an ordinary child process in a temp directory. One
//! task's subject was the bug class "shell script deletes the root when a variable is unset", and
//! its verifier ran the broken script with the variable empty to prove the bug. The shell expanded
//! that into a recursive delete of every top-level directory, and the suite executed it on a
//! developer's machine. Eighteen application bundles, the ssh-agent socket and the running terminal
//! did not survive.
//!
//! Two rules follow, and they are structural rather than careful:
//!
//! 1. **Verifiers run inside the T2 jail, never as a bare child process.** The sandbox this project
//!    already ships confines writes to one directory (docs/14). A runaway command inside it costs a
//!    temp directory; the same command outside it costs a machine. Care was what failed the first
//!    time — a comment about `env_clear()` felt like isolation and was not — so the containment is
//!    now a property of *where* the command runs and not of how carefully the fixture was written.
//! 2. **No task may name an absolute path or a destructive verb.** Enforced by
//!    [`corpus::audit`] and, repo-wide, by `panday-sandbox`'s `no_destructive_fixtures` lint. The
//!    bug classes that motivated the dangerous fixture are simply not in the corpus: a benchmark of
//!    agent repair does not need `rm -rf` to be interesting, and choosing it was a mistake of taste
//!    before it was a mistake of engineering.
//!
//! # What a task is
//!
//! A broken tree, a verifier whose exit code is the reward, and a reference patch the agent never
//! sees. The suite uses the reference to prove two things about every task — the verifier fails
//! before the fix and passes after it — because a task that already passes rewards nothing and a
//! task nobody can solve poisons the signal. Grading is the exit code and never a diff against the
//! reference: an agent that fixes the bug differently has fixed the bug.
//!
//! `materialise` is `reset()`, an attempt is `step()`, `verify` is the reward — the same three
//! pieces a GRPO environment needs, with no dependency on the agent that produced the attempt.

use std::path::{Path, PathBuf};

pub mod corpus;

pub use corpus::{audit, corpus, AuditFinding};

/// What a task's verifier needs in order to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toolchain {
    /// `python3`, present on every machine and CI runner this project targets.
    Python,
    /// `sh`. For tasks about text and exit codes rather than a language.
    Shell,
}

impl Toolchain {
    pub fn interpreter(&self) -> &'static str {
        match self {
            Toolchain::Python => "python3",
            Toolchain::Shell => "sh",
        }
    }

    pub fn available(&self) -> bool {
        std::process::Command::new(self.interpreter())
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// One file in a task's tree. `path` is always relative — [`audit`] enforces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: &'static str,
    pub contents: &'static str,
}

/// A repo task with a verifiable reward.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: &'static str,
    /// What the agent is told: the *symptom*, never the fix. An instruction naming the line to
    /// change measures instruction-following rather than repair.
    pub prompt: &'static str,
    pub toolchain: Toolchain,
    pub files: Vec<File>,
    /// Run from the task directory. Relative, so it cannot name anything outside.
    pub verify: &'static [&'static str],
    /// The file the reference patch replaces. Suite-only; never shown to an agent.
    pub reference: File,
}

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("io: {0}")]
    Io(String),
    #[error("sandbox: {0}")]
    Sandbox(String),
    #[error("this machine has no T2 jail, so no task may be run here")]
    NoJail,
}

/// The outcome of one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reward {
    pub passed: bool,
    /// The verifier's own words, capped. A reward signal is a number; a *debuggable* one is a number
    /// with the failure beside it.
    pub detail: String,
}

impl Task {
    /// Write the broken tree into `dir`. The GRPO `reset()`.
    pub fn materialise(&self, dir: &Path) -> Result<(), BenchError> {
        for file in &self.files {
            write_relative(dir, file)?;
        }
        Ok(())
    }

    /// Apply the reference patch. Suite-only.
    pub fn apply_reference(&self, dir: &Path) -> Result<(), BenchError> {
        write_relative(dir, &self.reference)
    }

    /// The command a verifier runs, as an argv. Relative by construction.
    pub fn verify_argv(&self) -> Vec<String> {
        self.verify.iter().map(|a| (*a).to_string()).collect()
    }
}

/// Write one file, refusing anything that could land outside `dir`.
///
/// Belt to [`audit`]'s braces: the audit checks the corpus at test time, and this checks again at
/// the moment of writing, because the corpus is not the only thing that could ever construct a
/// `Task`.
fn write_relative(dir: &Path, file: &File) -> Result<(), BenchError> {
    let relative = Path::new(file.path);
    if relative.is_absolute() || file.path.contains("..") {
        return Err(BenchError::Io(format!(
            "`{}` is not a relative path inside the task directory",
            file.path
        )));
    }
    let path = dir.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| BenchError::Io(e.to_string()))?;
    }
    std::fs::write(&path, file.contents).map_err(|e| BenchError::Io(e.to_string()))
}

/// A scratch directory that removes itself.
///
/// `remove_dir_all` on a path this type created, and nothing else — the incident's lesson applied to
/// the cleanup as well as to the fixtures.
pub struct Workspace(PathBuf);

impl Workspace {
    pub fn new(tag: &str) -> Result<Self, BenchError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "panday-bench-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).map_err(|e| BenchError::Io(e.to_string()))?;
        Ok(Self(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // Only ever the directory this handle created.
        if self.0.starts_with(std::env::temp_dir()) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

// ── Running a task: only ever inside the jail ────────────────────────────────

/// Run a task's verifier inside a T2 sandbox scoped to its own directory.
///
/// The sandbox is the containment. The verifier gets read-write access to the task directory and
/// nothing else, so the worst outcome of any command it runs — including one nobody intended — is a
/// lost temp directory. This is the single most important line of this module: it is what makes the
/// difference between the first version of agent-bench and this one.
///
/// Returns [`BenchError::NoJail`] where no T2 tier exists rather than silently running the command
/// unconfined. A benchmark that degrades to "run it directly" on an unsupported platform is the
/// first version again.
pub async fn verify_in_jail(task: &Task, dir: &Path) -> Result<Reward, BenchError> {
    use futures_util::StreamExt;
    use panday_sandbox::{
        ExecChunk, ExecSpec, FsPolicy, Limits, NetPolicy, SandboxPolicy, SandboxTier, SessionSpec,
    };

    let sandbox = jail().ok_or(BenchError::NoJail)?;
    let handle = sandbox
        .create(SessionSpec {
            tier: SandboxTier::T2OsJail,
            policy: SandboxPolicy {
                fs: FsPolicy {
                    // The one writable path. Everything else is denied by the tier, not by our care.
                    workspace_rw: dir.to_path_buf(),
                    staged_ro: vec![],
                },
                // Default-deny egress: a verifier has nothing to say to the network, and a task that
                // reached one would be measuring the network.
                net: NetPolicy::default(),
                limits: Limits {
                    // A verifier that hangs is a failed task, not a stuck suite.
                    wall_clock_ms: 30_000,
                    ..Default::default()
                },
                ..Default::default()
            },
        })
        .await
        .map_err(|e| BenchError::Sandbox(e.to_string()))?;

    let mut stream = sandbox
        .exec(
            &handle,
            ExecSpec {
                cmd: task.verify_argv(),
                cwd: None,
                pty: false,
                stdin: None,
            },
        )
        .await
        .map_err(|e| BenchError::Sandbox(e.to_string()))?;

    let mut detail = String::new();
    let mut code = None;
    let mut limit_hit = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(ExecChunk::Stdout(bytes)) | Ok(ExecChunk::Stderr(bytes)) => {
                detail.push_str(&String::from_utf8_lossy(&bytes))
            }
            Ok(ExecChunk::Exit { code: c, .. }) => code = Some(c),
            Err(panday_sandbox::SandboxError::LimitExceeded(_)) => {
                limit_hit = true;
                break;
            }
            Err(e) => return Err(BenchError::Sandbox(e.to_string())),
        }
    }
    let _ = sandbox.destroy(handle).await;

    detail.truncate(400);
    if limit_hit {
        detail.push_str("\n[verifier exceeded its time limit]");
    }
    Ok(Reward {
        passed: !limit_hit && code == Some(0),
        detail,
    })
}

/// This machine's T2 tier, if it has one.
#[cfg(target_os = "macos")]
fn jail() -> Option<Box<dyn panday_sandbox::Sandbox>> {
    panday_sandbox::t2_macos::T2MacosSandbox::available().then(|| {
        Box::new(panday_sandbox::t2_macos::T2MacosSandbox::new())
            as Box<dyn panday_sandbox::Sandbox>
    })
}

#[cfg(target_os = "linux")]
fn jail() -> Option<Box<dyn panday_sandbox::Sandbox>> {
    panday_sandbox::t2_linux::T2LinuxSandbox::available().then(|| {
        Box::new(panday_sandbox::t2_linux::T2LinuxSandbox::new())
            as Box<dyn panday_sandbox::Sandbox>
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn jail() -> Option<Box<dyn panday_sandbox::Sandbox>> {
    None
}

/// An attempt at one task: given the materialised directory, change something.
pub type Attempt<'a> = &'a (dyn Fn(&Task, &Path) -> Result<(), String> + Sync);

/// Run every runnable task and score it.
///
/// A task whose toolchain is missing, or a machine with no jail, is **skipped and named** rather
/// than counted as a failure — folding either into the denominator would report a worse agent on a
/// thinner machine.
pub async fn score(
    tasks: &[Task],
    attempt: Attempt<'_>,
    at: &str,
    subject: &str,
) -> panday_types::scorecard::Scorecard {
    let mut card = panday_types::scorecard::Scorecard::new("agent-bench", subject, at);
    let mut skipped = Vec::new();

    for task in tasks {
        if !task.toolchain.available() {
            skipped.push(task.id);
            continue;
        }
        let Ok(workspace) = Workspace::new(task.id) else {
            card.record(task.id, false, "could not create a workspace");
            continue;
        };
        if let Err(e) = task.materialise(workspace.path()) {
            card.record(task.id, false, format!("materialise: {e}"));
            continue;
        }
        if let Err(e) = attempt(task, workspace.path()) {
            card.record(task.id, false, format!("attempt: {e}"));
            continue;
        }
        match verify_in_jail(task, workspace.path()).await {
            Ok(reward) => {
                let first = reward.detail.lines().next().unwrap_or("").to_string();
                card.record(task.id, reward.passed, first);
            }
            Err(BenchError::NoJail) => skipped.push(task.id),
            Err(e) => card.record(task.id, false, format!("verify: {e}")),
        }
    }

    card.metric("tasks_skipped", skipped.len() as f64);
    if !skipped.is_empty() {
        card.provenance.notes = Some(format!("skipped (no toolchain or no jail): {skipped:?}"));
    }
    card
}
