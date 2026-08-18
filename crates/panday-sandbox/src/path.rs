//! Shared path resolution for the tiers that enforce FS policy in code.
//!
//! T0 and T2's `put`/`get` both run in **our** process, outside any jail, so
//! both need the same check. Having two hand-rolled copies was a mistake that
//! cost a real bug (a stray `join("")` appended a separator and turned every
//! file read into "Not a directory"), so there is exactly one implementation
//! here and both tiers call it.

use crate::SandboxError;
use std::path::{Component, Path, PathBuf};

/// Canonicalise a path that may not exist yet.
///
/// `std::fs::canonicalize` requires the whole path to exist, but a write
/// targets a file that usually does not. So: canonicalise the deepest
/// existing ancestor — resolving any symlinks in it, which is what defeats
/// the symlink-escape bypass — then re-attach the remaining components.
///
/// A `..` surviving into the non-existent tail is refused rather than folded
/// away: its meaning depends on symlink resolution we cannot redo, so
/// normalising it would be guessing at the caller's intent.
pub fn canonicalize_allowing_missing(path: &Path) -> Result<PathBuf, SandboxError> {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();

    while !existing.exists() {
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                if !existing.pop() {
                    break;
                }
            }
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

/// Resolve `path` (absolute, or relative to `workspace`) and require the
/// result to sit under one of `roots`.
pub fn resolve_within(
    workspace: &Path,
    roots: &[&PathBuf],
    path: &Path,
    what: &str,
) -> Result<PathBuf, SandboxError> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };

    let canonical = canonicalize_allowing_missing(&joined)?;

    if roots.iter().any(|root| canonical.starts_with(root)) {
        Ok(canonical)
    } else {
        Err(SandboxError::PolicyViolation(format!(
            "{what} denied: {} resolves outside the workspace ({})",
            path.display(),
            canonical.display()
        )))
    }
}
