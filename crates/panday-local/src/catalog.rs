//! The signed model catalog (docs/18 §Model management, M18.2).
//!
//! > "Catalog = a signed JSON index we publish (model name → GGUF URL, sha256, license, context
//! > length, RAM estimate, capability profile). Enterprise mirrors host the same index internally;
//! > `pull` honors a mirror URL."
//!
//! Two independent checks, and both matter:
//!
//! - **The signature covers the index**, so a mirror — which is, by design, a host we do not
//!   control — cannot add a model, change a URL, or lower a hash. An enterprise mirror is a
//!   convenience, not a trust boundary.
//! - **The sha256 covers the artifact**, so the file that arrives is the file the signed index
//!   named. A signature on an index whose contents nobody checks would authenticate a promise, not
//!   a download.
//!
//! The signature is verified over the bytes *as received*, before parsing. Verifying a
//! re-serialization is the classic canonicalization hole: two JSON documents that parse the same
//! can serialize differently, and the one that was signed is the one that arrived.

use panday_types::capability::CapabilityProfile;
use serde::{Deserialize, Serialize};

/// Licences we can redistribute, and therefore list.
///
/// docs/18: "Licenses are surfaced, not hidden — the catalog refuses to list anything we can't
/// redistribute." A community licence with a use restriction (Llama's, Gemma's) is not on this
/// list, and an entry carrying one is rejected at parse time rather than filtered at display time:
/// a rejected catalog is a bug report, a silently shortened one is a mystery.
pub const REDISTRIBUTABLE: &[&str] = &["Apache-2.0", "MIT", "BSD-3-Clause", "MPL-2.0"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelArtifact {
    /// What a user types: `qwen3.5-4b-q4`.
    pub id: String,
    /// Where the GGUF lives. A mirror rewrites the origin, never the path or the hash.
    pub url: String,
    /// Lowercase hex. The only thing that decides whether a download is the right file.
    pub sha256: String,
    pub license: String,
    pub size_bytes: u64,
    /// What the machine needs to run it, so `pull` can say "this will not fit" before spending an
    /// hour of somebody's bandwidth finding out.
    pub ram_estimate_mb: u64,
    /// Per-model, measured by the eval suite rather than guessed (docs/18 §degraded-capability
    /// honesty; M19.2 is what turns `declared` into `measured`).
    pub profile: CapabilityProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Index {
    pub version: u32,
    pub models: Vec<ModelArtifact>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CatalogError {
    #[error("catalog signature: {0}")]
    Signature(String),
    #[error("catalog parse: {0}")]
    Parse(String),
    #[error("catalog version {0} is not supported")]
    Version(u32),
    #[error("`{model}` is licensed `{license}`, which we cannot redistribute")]
    License { model: String, license: String },
    #[error("`{0}` has a sha256 that is not 64 hex characters")]
    BadDigest(String),
    #[error("`{0}` appears twice")]
    Duplicate(String),
    #[error("no model named `{0}` in the catalog")]
    Unknown(String),
}

impl Index {
    /// Verify, then parse. In that order, and over the same bytes.
    pub fn parse_verified(
        bytes: &[u8],
        signature_hex: &str,
        trusted_key_hex: &str,
    ) -> Result<Self, CatalogError> {
        panday_plugins::signature::verify_from_trusted_key(bytes, signature_hex, trusted_key_hex)
            .map_err(|e| CatalogError::Signature(e.to_string()))?;
        Self::parse_unverified(bytes)
    }

    /// Parse without checking the signature.
    ///
    /// Exists for one caller — the publisher, which has to parse what it is about to sign — and is
    /// named so that using it anywhere else looks wrong in a diff.
    pub fn parse_unverified(bytes: &[u8]) -> Result<Self, CatalogError> {
        let index: Index =
            serde_json::from_slice(bytes).map_err(|e| CatalogError::Parse(e.to_string()))?;
        if index.version != 1 {
            return Err(CatalogError::Version(index.version));
        }

        let mut seen = std::collections::BTreeSet::new();
        for model in &index.models {
            if !REDISTRIBUTABLE.contains(&model.license.as_str()) {
                return Err(CatalogError::License {
                    model: model.id.clone(),
                    license: model.license.clone(),
                });
            }
            if model.sha256.len() != 64 || !model.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(CatalogError::BadDigest(model.id.clone()));
            }
            if !seen.insert(model.id.clone()) {
                // Two entries for one id means two answers to "what should this hash be", decided
                // by iteration order.
                return Err(CatalogError::Duplicate(model.id.clone()));
            }
        }
        Ok(index)
    }

    pub fn get(&self, id: &str) -> Result<&ModelArtifact, CatalogError> {
        self.models
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| CatalogError::Unknown(id.to_string()))
    }
}

/// An internal host serving the same index and the same artifacts (docs/18).
///
/// A mirror replaces the *origin* only. The path, the filename and above all the hash come from the
/// signed index — a mirror that could rewrite a path could serve a different file at the same name,
/// which is exactly what the signature exists to prevent.
#[derive(Debug, Clone)]
pub struct Mirror(String);

impl Mirror {
    pub fn new(base: impl Into<String>) -> Self {
        Self(base.into().trim_end_matches('/').to_string())
    }

    pub fn rewrite(&self, url: &str) -> String {
        // Everything after the third `/` is the path — `https://host/a/b` → `/a/b`.
        let path = url
            .split_once("://")
            .and_then(|(_, rest)| rest.split_once('/'))
            .map(|(_, path)| path)
            .unwrap_or(url);
        format!("{}/{}", self.0, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use panday_plugins::signature::SigningKeyPair;
    use panday_types::capability::Provenance;

    fn artifact(id: &str, license: &str) -> ModelArtifact {
        ModelArtifact {
            id: id.into(),
            url: format!("https://models.panday.dev/{id}.gguf"),
            sha256: "a".repeat(64),
            license: license.into(),
            size_bytes: 2_400_000_000,
            ram_estimate_mb: 6_000,
            profile: CapabilityProfile {
                max_context_tokens: 16_000,
                json_reliability: 0.7,
                tool_reliability: 0.6,
                vision: false,
                max_subagents: 1,
                provenance: Provenance::Declared,
            },
        }
    }

    fn index(models: Vec<ModelArtifact>) -> Vec<u8> {
        serde_json::to_vec(&Index { version: 1, models }).unwrap()
    }

    fn keys() -> SigningKeyPair {
        SigningKeyPair::from_bytes(&[7u8; 32])
    }

    #[test]
    fn a_signed_index_parses() {
        let bytes = index(vec![artifact("qwen3.5-4b-q4", "Apache-2.0")]);
        let k = keys();
        let sig = k.sign_archive(&bytes);
        let parsed = Index::parse_verified(&bytes, &sig, &k.public_key_hex()).unwrap();
        assert_eq!(parsed.models.len(), 1);
    }

    #[test]
    fn a_tampered_index_does_not() {
        // The attack this stops: a mirror that adds a model, or lowers a hash to one it can meet.
        let bytes = index(vec![artifact("qwen3.5-4b-q4", "Apache-2.0")]);
        let k = keys();
        let sig = k.sign_archive(&bytes);

        let mut tampered = index(vec![
            artifact("qwen3.5-4b-q4", "Apache-2.0"),
            artifact("evil-13b", "MIT"),
        ]);
        assert!(matches!(
            Index::parse_verified(&tampered, &sig, &k.public_key_hex()),
            Err(CatalogError::Signature(_))
        ));

        // And a one-byte change to a hash.
        tampered = String::from_utf8(index(vec![artifact("qwen3.5-4b-q4", "Apache-2.0")]))
            .unwrap()
            .replacen("aaaa", "aaab", 1)
            .into_bytes();
        assert!(matches!(
            Index::parse_verified(&tampered, &sig, &k.public_key_hex()),
            Err(CatalogError::Signature(_))
        ));
    }

    #[test]
    fn a_signature_from_another_key_is_refused() {
        // Verifying against whatever key shipped alongside the index proves only that it is
        // internally consistent, which an attacker arranges by signing their own payload.
        let bytes = index(vec![artifact("qwen3.5-4b-q4", "Apache-2.0")]);
        let theirs = SigningKeyPair::from_bytes(&[9u8; 32]);
        let sig = theirs.sign_archive(&bytes);
        assert!(matches!(
            Index::parse_verified(&bytes, &sig, &keys().public_key_hex()),
            Err(CatalogError::Signature(_))
        ));
    }

    #[test]
    fn a_licence_we_cannot_redistribute_is_rejected_not_hidden() {
        // A rejected catalog is a bug report; a silently shortened one is a mystery.
        let bytes = index(vec![artifact("llama-ish-8b", "Llama-3-Community")]);
        assert!(matches!(
            Index::parse_unverified(&bytes),
            Err(CatalogError::License { .. })
        ));
    }

    #[test]
    fn a_digest_that_is_not_a_digest_is_rejected() {
        let mut a = artifact("qwen3.5-4b-q4", "Apache-2.0");
        a.sha256 = "not-a-hash".into();
        assert!(matches!(
            Index::parse_unverified(&index(vec![a])),
            Err(CatalogError::BadDigest(_))
        ));
    }

    #[test]
    fn a_duplicate_entry_is_rejected() {
        let bytes = index(vec![
            artifact("qwen3.5-4b-q4", "Apache-2.0"),
            artifact("qwen3.5-4b-q4", "MIT"),
        ]);
        assert!(matches!(
            Index::parse_unverified(&bytes),
            Err(CatalogError::Duplicate(_))
        ));
    }

    #[test]
    fn a_mirror_rewrites_the_host_and_nothing_else() {
        let mirror = Mirror::new("https://models.internal.example/");
        assert_eq!(
            mirror.rewrite("https://models.panday.dev/gguf/qwen3.5-4b-q4.gguf"),
            "https://models.internal.example/gguf/qwen3.5-4b-q4.gguf"
        );
    }
}
