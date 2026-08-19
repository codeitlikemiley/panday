//! `.plugin` archives (M16.6, docs/16 §the package).
//!
//! > "Distribution: `.plugin` archive, ed25519-signed"
//!
//! A gzipped tar with `plugin.toml` at its root. Nothing invented: docs/16's whole extension
//! premise (ADR-005) is that we adopt formats and compete on the runtime, and a bespoke
//! container would be the one place we did the opposite.
//!
//! ## Extraction is written out rather than delegated
//!
//! `tar`'s `unpack` would be one line. It is not used, because an archive from a registry is
//! attacker-controlled input and the interesting failures are all in what an entry is *allowed
//! to be*: a path that escapes the destination, a symlink pointing at `~/.ssh`, a 4GB file
//! that fills the disk, a hundred thousand entries. Each of those is a check below, and each
//! has a test. docs/20 T3 puts plugins in the "hostile code" column before they ever run.

use crate::PluginManifest;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// Caps. Generous for a real plugin, small enough that a malicious archive fails fast.
///
/// A cap that is too tight breaks a legitimate plugin and gets raised without thought; these
/// are set where a plugin that hits one is doing something a reviewer should look at. A WASM
/// tool is a few hundred kB, a skill is a few kB.
pub const MAX_ENTRIES: usize = 4_096;
pub const MAX_ENTRY_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("not a readable .plugin archive: {0}")]
    Unreadable(String),
    #[error("archive has no plugin.toml at its root")]
    NoManifest,
    #[error("plugin.toml: {0}")]
    Manifest(String),
    /// Every rejection names the entry, because "the archive was refused" is not actionable
    /// and "`../../.ssh/authorized_keys` escapes the destination" is.
    #[error("refusing entry `{path}`: {reason}")]
    UnsafeEntry { path: String, reason: String },
    #[error("archive is too large: {0}")]
    TooLarge(String),
    #[error("io: {0}")]
    Io(String),
}

fn decoder(bytes: &[u8]) -> tar::Archive<flate2::read::GzDecoder<&[u8]>> {
    tar::Archive::new(flate2::read::GzDecoder::new(bytes))
}

/// Read the manifest without unpacking anything.
///
/// This is what an install prompt is built from, and it runs *before* a single byte is written
/// to disk: consent comes first (docs/16 §install-time consent), so the manifest has to be
/// readable without committing to the archive.
pub fn read_manifest(bytes: &[u8]) -> Result<PluginManifest, ArchiveError> {
    let mut archive = decoder(bytes);
    let entries = archive
        .entries()
        .map_err(|e| ArchiveError::Unreadable(e.to_string()))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| ArchiveError::Unreadable(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| ArchiveError::Unreadable(e.to_string()))?
            .to_path_buf();
        if path == Path::new("plugin.toml") || path == Path::new("./plugin.toml") {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|e| ArchiveError::Io(e.to_string()))?;
            return PluginManifest::parse(&text).map_err(|e| ArchiveError::Manifest(e.to_string()));
        }
    }
    Err(ArchiveError::NoManifest)
}

/// What an archive contains, for a consent prompt or a listing. Paths only — reading every
/// entry to describe it would mean trusting it first.
pub fn list(bytes: &[u8]) -> Result<Vec<String>, ArchiveError> {
    let mut archive = decoder(bytes);
    let mut out = Vec::new();
    for entry in archive
        .entries()
        .map_err(|e| ArchiveError::Unreadable(e.to_string()))?
    {
        let entry = entry.map_err(|e| ArchiveError::Unreadable(e.to_string()))?;
        out.push(
            entry
                .path()
                .map_err(|e| ArchiveError::Unreadable(e.to_string()))?
                .display()
                .to_string(),
        );
        if out.len() > MAX_ENTRIES {
            return Err(ArchiveError::TooLarge(format!(
                "more than {MAX_ENTRIES} entries"
            )));
        }
    }
    Ok(out)
}

/// Unpack into `dest`, refusing anything an archive should not contain.
///
/// Returns the files written. `dest` must already exist and should be empty — this refuses to
/// overwrite, because an install that silently replaced a file is an install that can be used
/// to replace a file.
pub fn extract_to(bytes: &[u8], dest: &Path) -> Result<Vec<PathBuf>, ArchiveError> {
    let dest = dest
        .canonicalize()
        .map_err(|e| ArchiveError::Io(format!("{}: {e}", dest.display())))?;
    let mut archive = decoder(bytes);
    let mut written = Vec::new();
    let mut total = 0u64;

    for entry in archive
        .entries()
        .map_err(|e| ArchiveError::Unreadable(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| ArchiveError::Unreadable(e.to_string()))?;
        let raw = entry
            .path()
            .map_err(|e| ArchiveError::Unreadable(e.to_string()))?
            .to_path_buf();
        let shown = raw.display().to_string();

        if written.len() >= MAX_ENTRIES {
            return Err(ArchiveError::TooLarge(format!(
                "more than {MAX_ENTRIES} entries"
            )));
        }

        // Only regular files and directories. A symlink is the classic escape — the *link*
        // stays inside the destination while its target does not, so a later write through it
        // lands wherever the attacker chose. Hardlinks, devices and FIFOs have no business in
        // a plugin either.
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(ArchiveError::UnsafeEntry {
                path: shown,
                reason: format!("entry type {kind:?} is not allowed in a plugin archive"),
            });
        }

        let relative = safe_relative(&raw).ok_or_else(|| ArchiveError::UnsafeEntry {
            path: shown.clone(),
            reason: "path is absolute or escapes the destination".into(),
        })?;
        let target = dest.join(&relative);

        if kind.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| ArchiveError::Io(e.to_string()))?;
            continue;
        }

        let size = entry.header().size().unwrap_or(0);
        if size > MAX_ENTRY_BYTES {
            return Err(ArchiveError::TooLarge(format!(
                "{shown} is {size} bytes (cap {MAX_ENTRY_BYTES})"
            )));
        }
        total = total.saturating_add(size);
        if total > MAX_TOTAL_BYTES {
            return Err(ArchiveError::TooLarge(format!(
                "archive expands past {MAX_TOTAL_BYTES} bytes"
            )));
        }

        if target.exists() {
            return Err(ArchiveError::UnsafeEntry {
                path: shown,
                reason: "already exists; refusing to overwrite".into(),
            });
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ArchiveError::Io(e.to_string()))?;
        }

        // Copy through a size-limited reader rather than trusting the header: a tar header can
        // claim one size and the stream deliver another, and the check above only saw the
        // claim.
        let mut file = std::fs::File::create(&target)
            .map_err(|e| ArchiveError::Io(format!("{}: {e}", target.display())))?;
        let copied = std::io::copy(&mut entry.by_ref().take(MAX_ENTRY_BYTES + 1), &mut file)
            .map_err(|e| ArchiveError::Io(e.to_string()))?;
        if copied > MAX_ENTRY_BYTES {
            let _ = std::fs::remove_file(&target);
            return Err(ArchiveError::TooLarge(format!(
                "{shown} streamed more than {MAX_ENTRY_BYTES} bytes"
            )));
        }
        written.push(relative);
    }

    Ok(written)
}

/// A path that stays inside the destination, or `None`.
///
/// Lexical: `..` is refused outright rather than resolved, because "resolves inside today"
/// depends on what else the archive created, and an archive controls that order.
fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                // A NUL or a separator smuggled into a name; also catches Windows drive-ish
                // names that `Component::Normal` accepts on Unix.
                let text = part.to_str()?;
                if text.contains('\0') || text.contains('/') || text.contains('\\') {
                    return None;
                }
                out.push(part);
            }
            Component::CurDir => {}
            // Absolute, `..`, or a Windows prefix.
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Build an archive. Used by tests and by whatever packs a plugin for publication.
pub fn pack(files: &[(&str, &[u8])]) -> Result<Vec<u8>, ArchiveError> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut builder = tar::Builder::new(&mut gz);
        for (path, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header
                .set_path(path)
                .map_err(|e| ArchiveError::Io(e.to_string()))?;
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append(&header, *bytes)
                .map_err(|e| ArchiveError::Io(e.to_string()))?;
        }
        builder
            .finish()
            .map_err(|e| ArchiveError::Io(e.to_string()))?;
    }
    gz.finish().map_err(|e| ArchiveError::Io(e.to_string()))
}
