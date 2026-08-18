//! T0 — in-process, no exec, path policy enforced in code (docs/14).
//!
//! The tier our own pure tools run in: `read_file`, `grep`, `glob`. There is
//! no process to jail, so the *only* isolation is that every path is resolved
//! and checked before it is touched. That check is therefore the whole
//! security surface of this tier, and it is what the tests below hammer.
//!
//! ## What T0 does and does not defend against
//!
//! T0 constrains **our own** code operating on behalf of a model — a model
//! that asks to read `../../../../etc/shadow` is stopped here. It is *not* a
//! defence against hostile native code, which has no reason to route through
//! this API at all; that is what T2 and T3 exist for (ADR-004).
//!
//! There is an unavoidable TOCTOU window between resolving a path and opening
//! it: an attacker who can create symlinks inside the workspace concurrently
//! could swap a component in between. Closing it needs `openat2(RESOLVE_BENEATH)`
//! or a jail — both T2 concerns (M14.2). Documented rather than hidden.

use crate::{
    ExecSpec, ExecStream, FsPolicy, Limits, Sandbox, SandboxError, SandboxHandle, SandboxPolicy,
    SandboxTier, SessionSpec, SnapshotRef,
};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

/// Read or write — they have different admissible roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// In-process sandbox for pure native tools.
#[derive(Default)]
pub struct T0Sandbox {
    sessions: Mutex<HashMap<String, ResolvedPolicy>>,
    next_id: std::sync::atomic::AtomicU64,
}

/// A policy whose roots have been canonicalised once, at `create`.
///
/// Canonicalising up front matters: comparing against a root that is itself a
/// symlink (macOS `/tmp` → `/private/tmp` is the everyday case) would reject
/// every legitimate path.
#[derive(Debug, Clone)]
struct ResolvedPolicy {
    workspace_rw: PathBuf,
    staged_ro: Vec<PathBuf>,
    limits: Limits,
}

impl T0Sandbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve `path` under the policy, or explain why it is refused.
    ///
    /// Rules, in order:
    /// 1. relative paths are taken against the workspace root;
    /// 2. the path is canonicalised — which **follows symlinks**, so a link
    ///    pointing out of the workspace resolves to its target and is then
    ///    rejected by (3), rather than smuggling access through;
    /// 3. the result must sit under `workspace_rw` (writes) or under
    ///    `workspace_rw`/`staged_ro` (reads).
    pub fn resolve(
        &self,
        handle: &SandboxHandle,
        path: &Path,
        access: Access,
    ) -> Result<PathBuf, SandboxError> {
        let policy = self
            .sessions
            .lock()
            .unwrap()
            .get(&handle.id)
            .cloned()
            .ok_or_else(|| SandboxError::Internal(format!("no such session {}", handle.id)))?;

        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            policy.workspace_rw.join(path)
        };

        let canonical = canonicalize_lexically_then_really(&joined)?;

        let roots: Vec<&PathBuf> = match access {
            Access::Write => vec![&policy.workspace_rw],
            // Staged inputs are readable but never writable (docs/14
            // §workspace lifecycle: "staged inputs mounted RO").
            Access::Read => std::iter::once(&policy.workspace_rw)
                .chain(policy.staged_ro.iter())
                .collect(),
        };

        if roots.iter().any(|root| canonical.starts_with(root)) {
            return Ok(canonical);
        }

        Err(SandboxError::PolicyViolation(format!(
            "{} denied: {} resolves outside the workspace ({})",
            match access {
                Access::Read => "read",
                Access::Write => "write",
            },
            path.display(),
            canonical.display()
        )))
    }

    fn limits(&self, handle: &SandboxHandle) -> Result<Limits, SandboxError> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .get(&handle.id)
            .ok_or_else(|| SandboxError::Internal("no such session".into()))?
            .limits)
    }
}

/// Canonicalise a path that may not exist yet.
///
/// `std::fs::canonicalize` requires the whole path to exist, but a write
/// targets a file that often does not. So: canonicalise the deepest existing
/// ancestor (resolving any symlinks in it), then re-attach the remainder with
/// `..` and `.` folded away lexically. A `..` that survives into the tail
/// would let `a/../../etc` escape, so it is refused outright rather than
/// normalised into something surprising.
fn canonicalize_lexically_then_really(path: &Path) -> Result<PathBuf, SandboxError> {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();

    loop {
        if existing.exists() {
            break;
        }
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                if !existing.pop() {
                    break;
                }
            }
            // No filename component left (root, or a trailing `..`).
            None => break,
        }
    }

    let mut base = std::fs::canonicalize(&existing).map_err(|e| {
        SandboxError::PolicyViolation(format!("cannot resolve {}: {e}", existing.display()))
    })?;

    for part in tail.into_iter().rev() {
        let as_path = PathBuf::from(&part);
        match as_path.components().next() {
            Some(Component::ParentDir) => {
                // Refuse rather than pop: the caller asked for a path whose
                // meaning depends on symlink resolution we cannot redo here.
                return Err(SandboxError::PolicyViolation(format!(
                    "path escapes via `..`: {}",
                    path.display()
                )));
            }
            Some(Component::CurDir) => continue,
            _ => base.push(part),
        }
    }
    Ok(base)
}

#[async_trait::async_trait]
impl Sandbox for T0Sandbox {
    async fn create(&self, spec: SessionSpec) -> Result<SandboxHandle, SandboxError> {
        if spec.tier != SandboxTier::T0InProcess {
            return Err(SandboxError::Unsupported(spec.tier));
        }

        let SandboxPolicy { fs, limits, .. } = spec.policy;
        let FsPolicy {
            workspace_rw,
            staged_ro,
        } = fs;

        // A workspace that does not exist is a configuration error, and
        // silently creating one would hide a typo'd path until something
        // wrote into the wrong place.
        let workspace_rw = std::fs::canonicalize(&workspace_rw).map_err(|e| {
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
            "t0-{}",
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        self.sessions.lock().unwrap().insert(
            id.clone(),
            ResolvedPolicy {
                workspace_rw,
                staged_ro: staged,
                limits,
            },
        );

        Ok(SandboxHandle {
            id,
            tier: SandboxTier::T0InProcess,
        })
    }

    /// T0 has no process to run anything in — that is the definition of the
    /// tier (docs/14: "in-process Rust fn ... no exec"). A tool that needs to
    /// execute must declare T2 or above.
    async fn exec(&self, _h: &SandboxHandle, _cmd: ExecSpec) -> Result<ExecStream, SandboxError> {
        Err(SandboxError::Unsupported(SandboxTier::T0InProcess))
    }

    async fn put(
        &self,
        h: &SandboxHandle,
        path: PathBuf,
        data: Vec<u8>,
    ) -> Result<(), SandboxError> {
        let limits = self.limits(h)?;
        if data.len() as u64 > limits.disk_bytes {
            return Err(SandboxError::LimitExceeded(format!(
                "write of {} bytes exceeds disk limit {}",
                data.len(),
                limits.disk_bytes
            )));
        }

        let target = self.resolve(h, &path, Access::Write)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SandboxError::Internal(format!("mkdir {}: {e}", parent.display())))?;
        }
        std::fs::write(&target, data)
            .map_err(|e| SandboxError::Internal(format!("write {}: {e}", target.display())))
    }

    async fn get(&self, h: &SandboxHandle, path: PathBuf) -> Result<Vec<u8>, SandboxError> {
        let target = self.resolve(h, &path, Access::Read)?;
        std::fs::read(&target)
            .map_err(|e| SandboxError::Internal(format!("read {}: {e}", target.display())))
    }

    async fn snapshot(&self, _h: &SandboxHandle) -> Result<SnapshotRef, SandboxError> {
        Err(SandboxError::Unsupported(SandboxTier::T0InProcess))
    }

    async fn destroy(&self, h: SandboxHandle) -> Result<(), SandboxError> {
        // The workspace is the user's own directory at this tier; dropping the
        // policy is all "destroy" can mean, and deleting their files would be
        // catastrophic rather than tidy.
        self.sessions.lock().unwrap().remove(&h.id);
        Ok(())
    }
}
