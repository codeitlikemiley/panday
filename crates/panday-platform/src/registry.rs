//! The plugin registry (M16.6, docs/16 §the package).
//!
//! > "registry tiers `verified | community | unlisted`, with the marketplace being a phase-4
//! > storefront over the same registry API."
//!
//! Publish, fetch, verify. Three properties carry the trust model, and each is a test:
//!
//! 1. **A publisher cannot self-assign a tier.** Publishing always lands `unlisted`;
//!    `verified` is docs/16's "requires review", and a field in a request body is not a
//!    review. Promotion is a store method with no HTTP route, because the route needs
//!    authentication (M17.3) and a promotion endpoint without it would be the whole trust
//!    model undone by a curl.
//! 2. **A published version is immutable.** Re-publishing the same `name@version` is refused:
//!    an installed plugin's bytes must not change under someone who already consented to
//!    them, and "same version, different code" is the supply-chain attack ADR-005's signing
//!    exists to prevent.
//! 3. **The signature is checked on the way in.** A registry that stored unverified bytes
//!    would be a registry whose clients each have to remember to check — and one of them
//!    would not.
//!
//! Storage is a trait with an in-memory implementation; S3 and Postgres arrive with the rest
//! of the platform (M17.1). This is `panday-platform` rather than `panday-plugins` because a
//! registry is a *service*, and `panday-plugins` is a library the harness links — it has no
//! business carrying an HTTP server.

use panday_plugins::signature::verify_archive;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// docs/16's three tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Reviewed by us.
    Verified,
    /// Published by a known account, not reviewed.
    Community,
    /// Anything. The default, and what publishing gets you.
    #[default]
    Unlisted,
}

/// One published version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub name: String,
    pub version: String,
    /// Hex ed25519 public key of the publisher. The client needs it to verify, and it is what
    /// "the same publisher" means across versions.
    pub public_key: String,
    pub signature: String,
    pub tier: Tier,
    /// sha256 of the archive, so a client can tell two fetches apart without diffing bytes.
    pub digest: String,
    pub size: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("no such plugin: {0}")]
    NotFound(String),
    #[error("{0}@{1} is already published; a published version is immutable")]
    AlreadyPublished(String, String),
    #[error("signature does not verify: {0}")]
    BadSignature(String),
    #[error("storage: {0}")]
    Storage(String),
}

/// Where releases live. Postgres + object storage at M17.1.
pub trait RegistryStore: Send + Sync {
    fn put(&self, release: Release, archive: Vec<u8>) -> Result<(), RegistryError>;
    fn get(&self, name: &str, version: &str) -> Option<(Release, Vec<u8>)>;
    fn versions(&self, name: &str) -> Vec<Release>;
    fn list(&self) -> Vec<Release>;
    /// Promote after review (docs/16: `verified` "requires review"). Deliberately not an HTTP
    /// route — see the module note.
    fn promote(&self, name: &str, version: &str, tier: Tier) -> Result<(), RegistryError>;
}

/// `(name, version) -> (metadata, archive)`. Named because the tuple-of-tuples reads badly
/// inline, and because object storage will replace the `Vec<u8>` half without touching the key.
type Stored = BTreeMap<(String, String), (Release, Vec<u8>)>;

#[derive(Default)]
pub struct MemoryRegistry {
    releases: Mutex<Stored>,
}

impl MemoryRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

impl RegistryStore for MemoryRegistry {
    fn put(&self, release: Release, archive: Vec<u8>) -> Result<(), RegistryError> {
        let mut releases = self.releases.lock().unwrap();
        let key = (release.name.clone(), release.version.clone());
        if releases.contains_key(&key) {
            return Err(RegistryError::AlreadyPublished(key.0, key.1));
        }
        releases.insert(key, (release, archive));
        Ok(())
    }

    fn get(&self, name: &str, version: &str) -> Option<(Release, Vec<u8>)> {
        self.releases
            .lock()
            .unwrap()
            .get(&(name.to_string(), version.to_string()))
            .cloned()
    }

    fn versions(&self, name: &str) -> Vec<Release> {
        self.releases
            .lock()
            .unwrap()
            .iter()
            .filter(|((n, _), _)| n == name)
            .map(|(_, (release, _))| release.clone())
            .collect()
    }

    fn list(&self) -> Vec<Release> {
        self.releases
            .lock()
            .unwrap()
            .values()
            .map(|(release, _)| release.clone())
            .collect()
    }

    fn promote(&self, name: &str, version: &str, tier: Tier) -> Result<(), RegistryError> {
        let mut releases = self.releases.lock().unwrap();
        let key = (name.to_string(), version.to_string());
        match releases.get_mut(&key) {
            Some((release, _)) => {
                release.tier = tier;
                Ok(())
            }
            None => Err(RegistryError::NotFound(format!("{name}@{version}"))),
        }
    }
}

/// Publish: verify, then store.
///
/// The tier argument is absent on purpose. A caller cannot pass one, so there is no code path
/// where a publisher's wish becomes a tier.
pub fn publish(
    store: &dyn RegistryStore,
    name: &str,
    version: &str,
    archive: Vec<u8>,
    signature_hex: &str,
    public_key_hex: &str,
) -> Result<Release, RegistryError> {
    // Verified on the way in: a registry that stored unverified bytes would be one whose
    // clients each have to remember to check, and one of them would forget.
    verify_archive(&archive, signature_hex, public_key_hex)
        .map_err(|e| RegistryError::BadSignature(e.to_string()))?;

    // The manifest has to parse, and its name and version have to be the ones being published
    // — otherwise `linty@1.0.0` could ship a manifest calling itself something else, and every
    // consent prompt after that describes a different plugin than the one installed.
    let manifest = panday_plugins::archive::read_manifest(&archive)
        .map_err(|e| RegistryError::BadSignature(format!("archive: {e}")))?;
    if manifest.name != name || manifest.version != version {
        return Err(RegistryError::BadSignature(format!(
            "archive declares {}@{} but is being published as {name}@{version}",
            manifest.name, manifest.version
        )));
    }

    let release = Release {
        name: name.to_string(),
        version: version.to_string(),
        public_key: public_key_hex.to_string(),
        signature: signature_hex.to_string(),
        tier: Tier::Unlisted,
        digest: digest(&archive),
        size: archive.len(),
    };
    store.put(release.clone(), archive)?;
    Ok(release)
}

fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

// ── HTTP surface ─────────────────────────────────────────────────────────────

/// The registry API (docs/16: "the marketplace being a phase-4 storefront over the same
/// registry API").
///
/// Deliberately four routes. Publish is a `PUT` of the archive bytes with the signature in
/// headers rather than a multipart form: the thing being signed is the archive, and a body that
/// is exactly the archive removes the question of what the signature covers.
///
/// **No authentication yet** (M17.3 brings API keys). Which is why publishing lands `unlisted`
/// and why promotion has no route: an unauthenticated registry that could mint `verified` would
/// be worse than no registry.
pub mod http {
    use super::{publish, RegistryError, RegistryStore, Tier};
    use axum::body::Bytes;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::{Json, Router};
    use std::sync::Arc;

    pub const SIGNATURE_HEADER: &str = "x-panday-signature";
    pub const PUBLIC_KEY_HEADER: &str = "x-panday-public-key";
    pub const TIER_HEADER: &str = "x-panday-tier";

    #[derive(Clone)]
    pub struct RegistryState {
        pub store: Arc<dyn RegistryStore>,
        /// Cap on an uploaded archive. A registry with no upload limit is a disk-filling
        /// service for anyone who finds it.
        pub max_archive_bytes: usize,
    }

    impl RegistryState {
        pub fn new(store: Arc<dyn RegistryStore>) -> Self {
            Self {
                store,
                max_archive_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
            }
        }
    }

    pub const DEFAULT_MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;

    pub fn router(state: RegistryState) -> Router {
        Router::new()
            .route("/v1/plugins", get(list))
            .route("/v1/plugins/{name}", get(versions))
            .route("/v1/plugins/{name}/{version}", get(fetch).put(upload))
            .with_state(state)
    }

    async fn list(State(state): State<RegistryState>) -> Response {
        Json(state.store.list()).into_response()
    }

    async fn versions(State(state): State<RegistryState>, Path(name): Path<String>) -> Response {
        let releases = state.store.versions(&name);
        if releases.is_empty() {
            return (StatusCode::NOT_FOUND, format!("no such plugin: {name}")).into_response();
        }
        Json(releases).into_response()
    }

    async fn fetch(
        State(state): State<RegistryState>,
        Path((name, version)): Path<(String, String)>,
    ) -> Response {
        match state.store.get(&name, &version) {
            Some((release, archive)) => (
                [
                    (SIGNATURE_HEADER, release.signature.clone()),
                    (PUBLIC_KEY_HEADER, release.public_key.clone()),
                    (
                        TIER_HEADER,
                        serde_json::to_string(&release.tier)
                            .unwrap_or_default()
                            .trim_matches('"')
                            .to_string(),
                    ),
                    ("content-type", "application/vnd.panday.plugin".to_string()),
                ],
                archive,
            )
                .into_response(),
            None => (
                StatusCode::NOT_FOUND,
                format!("no such release: {name}@{version}"),
            )
                .into_response(),
        }
    }

    async fn upload(
        State(state): State<RegistryState>,
        Path((name, version)): Path<(String, String)>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        if body.len() > state.max_archive_bytes {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("archive is {} bytes", body.len()),
            )
                .into_response();
        }
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        let signature = header(SIGNATURE_HEADER);
        let public_key = header(PUBLIC_KEY_HEADER);
        if signature.is_empty() || public_key.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                format!("{SIGNATURE_HEADER} and {PUBLIC_KEY_HEADER} are required"),
            )
                .into_response();
        }
        // A tier in a request is ignored rather than rejected: a client that sends one is not
        // attacking anything, it is just wrong, and the response says what actually happened.
        let _ignored_tier = header(TIER_HEADER);

        match publish(
            state.store.as_ref(),
            &name,
            &version,
            body.to_vec(),
            &signature,
            &public_key,
        ) {
            Ok(release) => {
                debug_assert_eq!(release.tier, Tier::Unlisted);
                (StatusCode::CREATED, Json(release)).into_response()
            }
            Err(e) => {
                let status = match &e {
                    RegistryError::AlreadyPublished(_, _) => StatusCode::CONFLICT,
                    RegistryError::BadSignature(_) => StatusCode::BAD_REQUEST,
                    RegistryError::NotFound(_) => StatusCode::NOT_FOUND,
                    RegistryError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
                };
                (status, e.to_string()).into_response()
            }
        }
    }
}
