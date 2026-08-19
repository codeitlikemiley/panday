//! `panday plugin install` (M16.6, docs/16 §the package).
//!
//! Fetch, verify, consent, extract — in that order, and the order is the design:
//!
//! - **Verify before reading the manifest.** The manifest is what the consent prompt is built
//!   from, so an unverified manifest would mean consenting to text an attacker chose.
//! - **Consent before extracting.** docs/16 says "install-time consent"; a prompt shown after
//!   the files are on disk is a notification.
//! - **Extract into a fresh directory.** `panday_plugins::archive::extract_to` refuses to
//!   overwrite, so an install cannot be used to replace a file that is already there.
//!
//! ## Trust on first use, stated out loud
//!
//! With `--trust <pubkey>` the signature must match that key: the strong case, and what a
//! locked-down environment uses. Without it, the key is accepted *and printed*, which is what
//! TOFU means — the first install cannot be verified against anything, and the honest thing is
//! to say so and record the key so the next one can be. A registry tier is not a substitute:
//! `unlisted` means nobody reviewed it (docs/16).

use panday_plugins::archive;
use panday_plugins::signature::verify_archive;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub name: String,
    pub version: String,
    pub registry_url: String,
    /// Where plugins live. `~/.panday/plugins` by default.
    pub root: PathBuf,
    /// Require this exact publisher key.
    pub trust: Option<String>,
    /// Consent, given non-interactively. Without it the install stops after printing what it
    /// would grant — a CLI has no prompt loop, and pretending consent was given because the
    /// command was typed would make the consent model decorative.
    pub yes: bool,
}

impl InstallRequest {
    pub fn parse(spec: &str, registry_url: String, root: PathBuf) -> Result<Self, String> {
        let (name, version) = spec.split_once('@').ok_or_else(|| {
            format!("`{spec}` needs a version: install `name@version`, not a floating latest")
        })?;
        if name.is_empty() || version.is_empty() {
            return Err(format!("`{spec}` is not a `name@version`"));
        }
        Ok(Self {
            name: name.to_string(),
            version: version.to_string(),
            registry_url: registry_url.trim_end_matches('/').to_string(),
            root,
            trust: None,
            yes: false,
        })
    }
}

/// What an install did, or would have done.
#[derive(Debug, Clone)]
pub struct Installed {
    pub name: String,
    pub version: String,
    pub public_key: String,
    pub tier: String,
    pub dir: PathBuf,
    pub files: Vec<PathBuf>,
    /// The consent text that was shown. Returned so a caller can log exactly what a human saw.
    pub consent: String,
    /// False when `--yes` was absent: nothing was written.
    pub committed: bool,
}

pub async fn install(
    request: InstallRequest,
    out: &mut dyn crate::Output,
) -> Result<Installed, String> {
    let url = format!(
        "{}/v1/plugins/{}/{}",
        request.registry_url, request.name, request.version
    );
    let response = reqwest::get(&url)
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    let status = response.status();
    let signature = header(&response, "x-panday-signature");
    let public_key = header(&response, "x-panday-public-key");
    let tier = {
        let t = header(&response, "x-panday-tier");
        if t.is_empty() {
            "unknown".to_string()
        } else {
            t
        }
    };
    let archive_bytes = response
        .bytes()
        .await
        .map_err(|e| format!("{url}: {e}"))?
        .to_vec();
    if !status.is_success() {
        return Err(format!(
            "{url}: HTTP {status}: {}",
            String::from_utf8_lossy(&archive_bytes)
        ));
    }
    if signature.is_empty() || public_key.is_empty() {
        return Err(format!(
            "{url} served an archive with no signature; refusing to install unsigned code"
        ));
    }

    // Verified first: the manifest below is what the consent prompt quotes, so an unverified
    // one would mean consenting to text somebody else chose.
    match &request.trust {
        Some(trusted) => {
            panday_plugins::signature::verify_from_trusted_key(&archive_bytes, &signature, trusted)
                .map_err(|e| {
                    format!("signature does not match the key you trusted ({trusted}): {e}")
                })?;
        }
        None => {
            verify_archive(&archive_bytes, &signature, &public_key)
                .map_err(|e| format!("signature does not verify: {e}"))?;
        }
    }

    let manifest = archive::read_manifest(&archive_bytes).map_err(|e| e.to_string())?;
    if manifest.name != request.name || manifest.version != request.version {
        return Err(format!(
            "the archive declares {}@{} but was fetched as {}@{}",
            manifest.name, manifest.version, request.name, request.version
        ));
    }

    let mut consent = manifest.consent_summary();
    consent.push_str(&format!("\n  signed by: {public_key}"));
    consent.push_str(&format!("\n  registry tier: {tier}"));
    if request.trust.is_none() {
        // TOFU, said out loud. The first install cannot be verified against anything, and a
        // silent accept would let the user believe it was.
        consent.push_str(
            "\n  first use of this key: it was accepted on trust. Pass \
             --trust <key> to require it next time.",
        );
    }
    if tier == "unlisted" {
        consent.push_str("\n  `unlisted` means nobody reviewed this plugin (docs/16).");
    }
    out.line(&consent);

    let dir = request.root.join(&request.name).join(&request.version);

    if !request.yes {
        out.line("\nNothing was installed. Re-run with --yes to accept the grants above.");
        return Ok(Installed {
            name: request.name,
            version: request.version,
            public_key,
            tier,
            dir,
            files: Vec::new(),
            consent,
            committed: false,
        });
    }

    if dir.exists() {
        return Err(format!(
            "{} already exists; remove it to reinstall (a published version is immutable, so \
             the bytes there are the bytes you consented to)",
            dir.display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let files = archive::extract_to(&archive_bytes, &dir).map_err(|e| {
        // Leave nothing half-written: a partially extracted plugin is one the loader might
        // still find.
        let _ = std::fs::remove_dir_all(&dir);
        e.to_string()
    })?;

    // The key goes next to the plugin, so `--trust` on the next version has something to point
    // at and a reviewer can see who signed what is on disk.
    std::fs::write(dir.join(".publisher-key"), &public_key)
        .map_err(|e| format!("{}: {e}", dir.display()))?;

    out.line(&format!(
        "installed {}@{} to {} ({} files)",
        request.name,
        request.version,
        dir.display(),
        files.len()
    ));

    Ok(Installed {
        name: request.name,
        version: request.version,
        public_key,
        tier,
        dir,
        files,
        consent,
        committed: true,
    })
}

fn header(response: &reqwest::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// `~/.panday/plugins`, or a relative fallback when there is no home directory.
pub fn default_root() -> PathBuf {
    std::env::var_os("PANDAY_PLUGIN_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".panday/plugins")))
        .unwrap_or_else(|| PathBuf::from(".panday/plugins"))
}
