//! # panday-plugins
//!
//! Packaging + trust wrapper around open standards: SKILL.md skills, MCP
//! tools, WASM hooks (docs/16-plugins.md). This seed defines the manifest
//! and capability model; loaders land in M16.1+.

pub mod archive;
pub mod entitlement;
pub mod mcp;
pub mod signature;
pub mod skill;

pub use signature::{verify_archive, SignatureError, SigningKeyPair};
pub use skill::{discover, index, Skill, SkillError, SkillFrontmatter};

use serde::{Deserialize, Serialize};

/// `plugin.toml` — identity + requested capabilities (consent at install,
/// enforced by sandbox tiers).
///
/// `deny_unknown_fields` on purpose. A manifest is a consent document, and a
/// key we do not recognise is either a typo or a capability request from a
/// newer format — both of which must be an error rather than silence. The
/// concrete failure this prevents: writing `hooks = [...]` *after* the
/// `[capabilities]` table makes it a key of that table, and without this the
/// request is dropped and the plugin installs with no hooks and no complaint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub skills: Vec<String>, // paths to SKILL.md dirs
    #[serde(default)]
    pub mcp_servers: Vec<McpServerDef>,
    #[serde(default)]
    pub hooks: Vec<HookPoint>,
}

/// Filesystem access a plugin may request.
///
/// A typed enum rather than the seed's free string: `fs = "workspac-ro"` is a
/// typo that a string field accepts and then silently treats as "no access
/// requested", which reads at install time as a *less* dangerous plugin than it
/// is. Deserialisation now rejects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FsCapability {
    #[default]
    None,
    WorkspaceRo,
    WorkspaceRw,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    #[serde(default)]
    pub fs: FsCapability,
    /// Domain allowlist for egress; empty = no network.
    #[serde(default)]
    pub net: Vec<String>,
    /// Secret names the plugin may receive (user consents at install).
    #[serde(default)]
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpServerDef {
    /// Child process under T2 policy.
    Stdio { command: String, args: Vec<String> },
    /// Streamable HTTP.
    Http { url: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    PreTurn,
    PreModel,
    PostModel,
    PreTool,
    PostTool,
    OnCompaction,
    OnStop,
}

impl HookPoint {
    /// The name as written in `plugin.toml`.
    ///
    /// A consent prompt must echo the author's own spelling: showing `PreTool`
    /// where the manifest says `pre_tool` makes a user check whether they are
    /// looking at the same thing.
    pub fn wire_name(self) -> &'static str {
        match self {
            HookPoint::PreTurn => "pre_turn",
            HookPoint::PreModel => "pre_model",
            HookPoint::PostModel => "post_model",
            HookPoint::PreTool => "pre_tool",
            HookPoint::PostTool => "post_tool",
            HookPoint::OnCompaction => "on_compaction",
            HookPoint::OnStop => "on_stop",
        }
    }
}

impl std::fmt::Display for HookPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// SKILL.md frontmatter (compatible with the existing ecosystem on purpose).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub triggers: Vec<String>,
}

// ---------------------------------------------------------------------------
// Manifest loading (M16.1)
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("plugin.toml io: {0}")]
    Io(String),
    #[error("plugin.toml is not valid TOML: {0}")]
    Toml(String),
    #[error("plugin.toml: `name` must be non-empty and contain no path separators (got {0:?})")]
    BadName(String),
    #[error("plugin.toml: `version` must be non-empty")]
    MissingVersion,
    #[error("plugin.toml: `description` must be non-empty — it is what a user consents against")]
    MissingDescription,
    #[error(
        "plugin.toml: net capability {0:?} is not a domain; wildcards and URLs are not allowed"
    )]
    BadDomain(String),
    #[error("plugin.toml: secret name {0:?} is not a plain environment-variable name")]
    BadSecretName(String),
}

impl PluginManifest {
    /// Parse and validate a `plugin.toml`.
    ///
    /// Validation is not politeness. A manifest is what a human consents to at
    /// install time, so anything ambiguous in it becomes a consent the user did
    /// not actually give.
    pub fn parse(raw: &str) -> Result<Self, ManifestError> {
        let manifest: PluginManifest =
            toml::from_str(raw).map_err(|e| ManifestError::Toml(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, ManifestError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| ManifestError::Io(format!("{}: {e}", path.display())))?;
        Self::parse(&raw)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        // A name reaches the filesystem (install directory) and the registry, so
        // a separator or `..` in it is a path-traversal primitive.
        if self.name.trim().is_empty()
            || self.name.contains('/')
            || self.name.contains('\\')
            || self.name.contains("..")
            // A control character in a directory name is never intentional and
            // is unpleasant everywhere it is later printed or joined.
            || self.name.chars().any(|c| c.is_control())
        {
            return Err(ManifestError::BadName(self.name.clone()));
        }
        if self.version.trim().is_empty() {
            return Err(ManifestError::MissingVersion);
        }
        if self.description.trim().is_empty() {
            return Err(ManifestError::MissingDescription);
        }

        for domain in &self.capabilities.net {
            if !is_plain_domain(domain) {
                return Err(ManifestError::BadDomain(domain.clone()));
            }
        }
        for secret in &self.capabilities.secrets {
            if !is_env_name(secret) {
                return Err(ManifestError::BadSecretName(secret.clone()));
            }
        }
        Ok(())
    }

    /// A human-readable consent prompt.
    ///
    /// docs/16: "Install-time consent". The text has to state what is being
    /// granted in terms a person can refuse — "net: [api.github.com]" is
    /// reviewable, "requests network access" is not.
    pub fn consent_summary(&self) -> String {
        let mut lines = vec![format!(
            "{} {} — {}",
            self.name, self.version, self.description
        )];

        lines.push(match self.capabilities.fs {
            FsCapability::None => "  filesystem: no access".into(),
            FsCapability::WorkspaceRo => "  filesystem: READ the workspace".into(),
            FsCapability::WorkspaceRw => "  filesystem: READ AND WRITE the workspace".into(),
        });

        if self.capabilities.net.is_empty() {
            lines.push("  network: none".into());
        } else {
            lines.push(format!("  network: {}", self.capabilities.net.join(", ")));
        }

        if self.capabilities.secrets.is_empty() {
            lines.push("  secrets: none".into());
        } else {
            // Named individually and never elided: a user consenting to a
            // secret needs to see which one.
            lines.push(format!(
                "  secrets: {}",
                self.capabilities.secrets.join(", ")
            ));
        }

        if !self.hooks.is_empty() {
            lines.push(format!(
                "  hooks: {} (runs inside every turn)",
                self.hooks
                    .iter()
                    .map(|h| h.wire_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines.join("\n")
    }

    /// The sandbox policy this manifest's capabilities imply.
    ///
    /// The manifest *requests*; the sandbox *enforces* (docs/16). Deriving the
    /// policy here keeps the two from drifting — a capability that grants
    /// nothing in the sandbox is a lie told at the consent prompt.
    pub fn requested_fs_writable(&self) -> bool {
        self.capabilities.fs == FsCapability::WorkspaceRw
    }

    pub fn requests_network(&self) -> bool {
        !self.capabilities.net.is_empty()
    }
}

/// A bare hostname: letters, digits, dots and hyphens.
///
/// Wildcards are refused rather than expanded. `*.example.com` reads as a
/// narrow grant and is in fact a grant to anything anyone can register under
/// that domain, which is not something a user can meaningfully consent to.
fn is_plain_domain(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('*')
        && !s.contains('/')
        && !s.contains(':')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && !s.starts_with('.')
        && !s.ends_with('.')
}

fn is_env_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}
