//! Installed models on disk: `list`, `pull`, `verify`, `rm` (docs/18 §Model management, M18.2).
//!
//! The store is a directory of GGUF files named by catalog id. Flat and boring on purpose: a
//! human has to be able to see what is installed with `ls`, delete one with `rm`, and copy the
//! directory onto an air-gapped machine (docs/17 §enterprise) without a tool to interpret it.
//!
//! **A download is verified while it streams, not after.** Hashing afterwards means writing a file
//! that might be wrong, and then trusting a second read of the same disk to tell you. Hashing as
//! the bytes arrive costs nothing extra and fails before anything is named.
//!
//! **Nothing lands under its real name until it is verified.** The download goes to a `.partial`
//! file and is renamed only after the digest matches — so an interrupted or corrupted pull leaves
//! no file that `list` would report as installed.

use crate::catalog::{CatalogError, Mirror, ModelArtifact};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("catalog: {0}")]
    Catalog(#[from] CatalogError),
    #[error("io: {0}")]
    Io(String),
    #[error("download {url}: {detail}")]
    Download { url: String, detail: String },
    #[error(
        "`{model}` is not the file the catalog signed: expected sha256 {expected}, got {actual}"
    )]
    DigestMismatch {
        model: String,
        expected: String,
        actual: String,
    },
    #[error("`{0}` is not installed")]
    NotInstalled(String),
}

/// Whether a machine with `available_mb` of RAM can be expected to run this model.
///
/// A judgement the *caller* makes, because probing physical memory portably means another
/// dependency and this crate's whole point is that it runs anywhere. The catalog's estimate is
/// surfaced by `models list` and `models pull`, so the person with the machine can apply it.
pub fn fits(artifact: &ModelArtifact, available_mb: u64) -> bool {
    artifact.ram_estimate_mb <= available_mb
}

/// One installed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub id: String,
    pub path: PathBuf,
    pub size_bytes: u64,
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `~/.panday/models`, or `$PANDAY_MODEL_DIR`.
    ///
    /// Under the user's home rather than a system path: `panday local` must work for somebody with
    /// no administrator on the machine, which is most of the people the offline tier is for.
    pub fn default_location() -> PathBuf {
        if let Ok(dir) = std::env::var("PANDAY_MODEL_DIR") {
            if !dir.trim().is_empty() {
                return PathBuf::from(dir);
            }
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Path::new(&home).join(".panday").join("models")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_for(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.gguf"))
    }

    /// What is on disk, sorted. Not what the catalog offers — that is a different question, and
    /// answering both at once is how `list` starts lying when the catalog moves.
    pub fn list(&self) -> Vec<Installed> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out: Vec<Installed> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let id = path.file_stem()?.to_string_lossy().to_string();
                if path.extension()? != "gguf" {
                    return None;
                }
                Some(Installed {
                    id,
                    size_bytes: entry.metadata().ok()?.len(),
                    path,
                })
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    pub fn is_installed(&self, id: &str) -> bool {
        self.path_for(id).is_file()
    }

    /// Re-hash an installed file against the catalog (`panday models verify`).
    ///
    /// Reads the whole file, which for a 4GB model is not free — and is the point. Verification
    /// that trusts a recorded hash verifies its own bookkeeping, not the disk.
    pub fn verify(&self, artifact: &ModelArtifact) -> Result<(), ModelError> {
        let path = self.path_for(&artifact.id);
        if !path.is_file() {
            return Err(ModelError::NotInstalled(artifact.id.clone()));
        }
        let actual = digest_file(&path)?;
        if !actual.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(ModelError::DigestMismatch {
                model: artifact.id.clone(),
                expected: artifact.sha256.clone(),
                actual,
            });
        }
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<(), ModelError> {
        let path = self.path_for(id);
        if !path.is_file() {
            return Err(ModelError::NotInstalled(id.to_string()));
        }
        std::fs::remove_file(&path).map_err(|e| ModelError::Io(e.to_string()))
    }

    /// Download an artifact, verifying as it streams.
    ///
    /// Already-installed models are re-verified rather than re-downloaded: `pull` twice should be
    /// cheap and should still tell you if the file rotted.
    pub async fn pull(
        &self,
        artifact: &ModelArtifact,
        mirror: Option<&Mirror>,
    ) -> Result<Installed, ModelError> {
        if self.is_installed(&artifact.id) {
            self.verify(artifact)?;
            return Ok(Installed {
                id: artifact.id.clone(),
                path: self.path_for(&artifact.id),
                size_bytes: artifact.size_bytes,
            });
        }

        std::fs::create_dir_all(&self.root).map_err(|e| ModelError::Io(e.to_string()))?;
        let url = match mirror {
            Some(m) => m.rewrite(&artifact.url),
            None => artifact.url.clone(),
        };

        let mut response = reqwest::get(&url).await.map_err(|e| ModelError::Download {
            url: url.clone(),
            detail: e.to_string(),
        })?;
        if !response.status().is_success() {
            return Err(ModelError::Download {
                url: url.clone(),
                detail: format!("HTTP {}", response.status()),
            });
        }

        // `.partial`, so an interrupted download is visibly incomplete rather than a short file
        // wearing the right name.
        let partial = self.root.join(format!("{}.gguf.partial", artifact.id));
        let mut file =
            std::fs::File::create(&partial).map_err(|e| ModelError::Io(e.to_string()))?;
        let mut hasher = Sha256::new();
        let mut written: u64 = 0;

        while let Some(chunk) = response.chunk().await.map_err(|e| ModelError::Download {
            url: url.clone(),
            detail: e.to_string(),
        })? {
            use std::io::Write;
            hasher.update(&chunk);
            file.write_all(&chunk)
                .map_err(|e| ModelError::Io(e.to_string()))?;
            written += chunk.len() as u64;
        }
        drop(file);

        let actual = hex(&hasher.finalize());
        if !actual.eq_ignore_ascii_case(&artifact.sha256) {
            // Removed, not left for a human to find: a rejected file that stays on disk is one
            // somebody eventually renames.
            let _ = std::fs::remove_file(&partial);
            return Err(ModelError::DigestMismatch {
                model: artifact.id.clone(),
                expected: artifact.sha256.clone(),
                actual,
            });
        }

        let final_path = self.path_for(&artifact.id);
        std::fs::rename(&partial, &final_path).map_err(|e| ModelError::Io(e.to_string()))?;
        Ok(Installed {
            id: artifact.id.clone(),
            path: final_path,
            size_bytes: written,
        })
    }
}

fn digest_file(path: &Path) -> Result<String, ModelError> {
    let mut file = std::fs::File::open(path).map_err(|e| ModelError::Io(e.to_string()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| ModelError::Io(e.to_string()))?;
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
