//! Artifact spill and range expansion (docs/15, M15.1).
//!
//! > "The raw output is never lost — it spills to object storage; the context
//! > gets the reduced form plus a handle. A follow-up tool
//! > `expand_artifact(ref, range)` lets the model pull exact ranges —
//! > **reduction is reversible on demand**, which is what makes aggressive
//! > defaults safe."
//!
//! That last clause is the whole point of this module. Every aggressive
//! default elsewhere in the reducer is only defensible because nothing is
//! actually destroyed.

use panday_types::id::ArtifactRef;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("no such artifact: {0}")]
    NotFound(String),
    #[error("artifact store: {0}")]
    Io(String),
}

/// Content-addressed blob storage. S3/MinIO in cloud, a directory in
/// `panday local`, memory in tests (docs/03 §big payloads).
pub trait ArtifactStore: Send + Sync {
    fn put(&self, bytes: &[u8], media_type: Option<String>) -> Result<ArtifactRef, ArtifactError>;
    fn get(&self, r: &ArtifactRef) -> Result<Vec<u8>, ArtifactError>;
}

/// sha256 of content, hex — the encoding docs/03 §Identifiers specifies.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    panday_types::hex(h.finalize())
}

/// In-memory store.
#[derive(Default)]
pub struct MemoryArtifactStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.blobs.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ArtifactStore for MemoryArtifactStore {
    fn put(&self, bytes: &[u8], media_type: Option<String>) -> Result<ArtifactRef, ArtifactError> {
        let hash = content_hash(bytes);
        // Content-addressed: storing the same bytes twice is one blob. This
        // is why re-running an identical command costs no extra storage.
        self.blobs
            .lock()
            .unwrap()
            .insert(hash.clone(), bytes.to_vec());
        Ok(ArtifactRef {
            hash,
            size: bytes.len() as u64,
            media_type,
        })
    }

    fn get(&self, r: &ArtifactRef) -> Result<Vec<u8>, ArtifactError> {
        self.blobs
            .lock()
            .unwrap()
            .get(&r.hash)
            .cloned()
            .ok_or_else(|| ArtifactError::NotFound(r.hash.clone()))
    }
}

/// A half-open line range, 0-indexed: `120..180` is lines 120 through 179.
///
/// Matches the marker the generic reducer writes into elided output, so the
/// model can copy the numbers straight out of what it was shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl LineRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Parse `"120..180"`, tolerating the punctuation the elision marker
    /// wraps it in.
    ///
    /// The marker reads `expand_artifact(raw_ref, 30..470)`, so a model
    /// copying the range out of it hands us `30..470)`. Rejecting that would
    /// make the escape hatch fail on the exact string we told it to use.
    pub fn parse(s: &str) -> Option<Self> {
        let trimmed = s.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        let (a, b) = trimmed.split_once("..")?;
        Some(Self::new(a.trim().parse().ok()?, b.trim().parse().ok()?))
    }
}

/// Pull a line range out of a stored artifact.
///
/// Out-of-range requests are **clamped, not rejected**: the model is working
/// from an elision marker and may well ask for more than exists. Returning an
/// error would spend a whole turn teaching it arithmetic; returning the
/// overlap answers the actual question.
pub fn expand(
    store: &dyn ArtifactStore,
    r: &ArtifactRef,
    range: LineRange,
) -> Result<String, ArtifactError> {
    let bytes = store.get(r)?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();

    let start = range.start.min(lines.len());
    let end = range.end.clamp(start, lines.len());

    let mut out = String::new();
    if start > 0 {
        out.push_str(&format!("[… {start} earlier lines …]\n"));
    }
    out.push_str(&lines[start..end].join("\n"));
    if end < lines.len() {
        out.push_str(&format!("\n[… {} later lines …]", lines.len() - end));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(text: &str) -> (MemoryArtifactStore, ArtifactRef) {
        let s = MemoryArtifactStore::new();
        let r = s.put(text.as_bytes(), Some("text/plain".into())).unwrap();
        (s, r)
    }

    #[test]
    fn hashes_are_sha256_hex_as_docs_03_specifies() {
        // Known vector: sha256("") — pins the algorithm, not just "some hash".
        assert_eq!(
            content_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(content_hash(b"abc").len(), 64);
    }

    #[test]
    fn identical_content_is_stored_once() {
        let s = MemoryArtifactStore::new();
        let a = s.put(b"same bytes", None).unwrap();
        let b = s.put(b"same bytes", None).unwrap();
        assert_eq!(a.hash, b.hash);
        assert_eq!(s.len(), 1, "content addressing must deduplicate");
    }

    #[test]
    fn the_ref_records_the_real_size() {
        let s = MemoryArtifactStore::new();
        let r = s.put(b"12345", None).unwrap();
        assert_eq!(r.size, 5);
    }

    #[test]
    fn round_trips_exact_bytes() {
        let (s, r) = store_with("line one\nline two");
        assert_eq!(s.get(&r).unwrap(), b"line one\nline two");
    }

    #[test]
    fn a_missing_artifact_is_not_found_rather_than_empty() {
        let s = MemoryArtifactStore::new();
        let err = s
            .get(&ArtifactRef {
                hash: "deadbeef".into(),
                size: 0,
                media_type: None,
            })
            .unwrap_err();
        assert!(matches!(err, ArtifactError::NotFound(_)));
    }

    #[test]
    fn expands_the_requested_range_and_says_what_it_omitted() {
        let text = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (s, r) = store_with(&text);

        let got = expand(&s, &r, LineRange::new(10, 13)).unwrap();
        assert!(got.contains("line 10"));
        assert!(got.contains("line 12"));
        assert!(!got.contains("line 13"), "end is exclusive");
        // The model must be able to tell there is more, or it cannot decide
        // whether to ask again.
        assert!(got.contains("10 earlier lines"));
        assert!(got.contains("87 later lines"));
    }

    #[test]
    fn an_out_of_range_request_is_clamped_not_rejected() {
        // The model is reading numbers off an elision marker; an error here
        // would spend a turn teaching it arithmetic instead of answering.
        let (s, r) = store_with("a\nb\nc");
        let got = expand(&s, &r, LineRange::new(1, 9999)).unwrap();
        assert!(got.contains('b') && got.contains('c'));

        let past_end = expand(&s, &r, LineRange::new(500, 600)).unwrap();
        assert!(past_end.contains("3 earlier lines"));
    }

    #[test]
    fn an_inverted_range_yields_nothing_rather_than_panicking() {
        let (s, r) = store_with("alpha\nbravo\ncharlie");
        let got = expand(&s, &r, LineRange::new(2, 1)).unwrap();
        assert!(!got.contains("alpha"), "{got}");
        assert!(!got.contains("bravo"), "{got}");
    }

    #[test]
    fn parses_a_range_lifted_straight_out_of_the_elision_marker() {
        // Exactly what a model copies from the text it was shown.
        assert_eq!(LineRange::parse("30..470)"), Some(LineRange::new(30, 470)));
        assert_eq!(
            LineRange::parse("(120..180)"),
            Some(LineRange::new(120, 180))
        );
    }

    #[test]
    fn parses_the_range_syntax_the_elision_marker_prints() {
        assert_eq!(LineRange::parse("120..180"), Some(LineRange::new(120, 180)));
        assert_eq!(LineRange::parse(" 3 .. 9 "), Some(LineRange::new(3, 9)));
        assert_eq!(LineRange::parse("nonsense"), None);
        assert_eq!(LineRange::parse("1..x"), None);
    }

    #[test]
    fn invalid_utf8_does_not_lose_the_artifact() {
        // Tool output is arbitrary bytes; a stray 0xFF must not make the
        // whole result unreadable.
        let s = MemoryArtifactStore::new();
        let r = s.put(&[b'o', b'k', 0xFF, b'\n', b'x'], None).unwrap();
        let got = expand(&s, &r, LineRange::new(0, 10)).unwrap();
        assert!(got.contains("ok"));
        assert!(got.contains('x'));
    }
}
