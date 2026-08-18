//! # ferrum-plugins
//!
//! Packaging + trust wrapper around open standards: SKILL.md skills, MCP
//! tools, WASM hooks (docs/16-plugins.md). This seed defines the manifest
//! and capability model; loaders land in M16.1+.

use serde::{Deserialize, Serialize};

/// `plugin.toml` — identity + requested capabilities (consent at install,
/// enforced by sandbox tiers).
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Capabilities {
    /// "none" | "workspace-ro" | "workspace-rw"
    #[serde(default)]
    pub fs: String,
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

/// SKILL.md frontmatter (compatible with the existing ecosystem on purpose).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub triggers: Vec<String>,
}
