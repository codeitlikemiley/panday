//! MCP host — mounting a server's tools (M16.3, docs/16 §MCP host).
//!
//! > "Via `rmcp` (official SDK): stdio (child process under T2 policy) and streamable
//! > HTTP transports; OAuth flows for the servers that need them. Mounted MCP tools
//! > appear in the ToolRegistry as `mcp:{server}:{tool}` behind the same `Tool` trait
//! > — the loop can't tell them from native tools; the *permission engine* can (MCP
//! > tools default to `Ask` until granted)."
//!
//! **stdio only, deliberately.** `rmcp` is compiled with `transport-child-process`
//! and nothing that opens a socket, so this host cannot reach the network even by
//! mistake. The streamable-HTTP transport and OAuth are a separate decision with a
//! separate threat model (a remote MCP server is a third party reading your prompts),
//! and they are not wired.
//!
//! ## What this module is and is not
//!
//! It is the *client*: spawn, handshake, list, call. It does not know what a
//! `panday-harness` tool is — that adapter lives in the harness, which is what keeps
//! `panday-plugins` free of the loop and testable on its own.
//!
//! ## Schema hygiene
//!
//! docs/16: "MCP tool schemas can be enormous; the registry minifies descriptions
//! into the stable prefix and lazy-loads full schemas on first use (the ToolSearch
//! pattern) when a server exposes >N tools." A tool schema lands in the *stable*
//! cached prefix (ADR-008), so a 40kB schema is 40kB paid on every turn of the
//! session. `MountedTool::brief()` is the short form; the full schema stays available
//! for the call.

use rmcp::model::CallToolRequestParams;
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::TokioChildProcess;
use rmcp::RoleClient;
use std::sync::Arc;

/// How to start a server. stdio only — see the module note.
#[derive(Debug, Clone)]
pub struct StdioServer {
    /// The name this server is mounted under. Part of every tool id, so it is part
    /// of the tool's provenance (`Origin::for_tool` decodes `mcp:{server}:{tool}`).
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Environment for the child. Explicit, never inherited: docs/20 T4 keeps
    /// secrets out of a process that did not declare them, and an MCP server is
    /// third-party code.
    pub env: Vec<(String, String)>,
    pub cwd: Option<std::path::PathBuf>,
}

impl StdioServer {
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("mcp server `{server}` did not start: {detail}")]
    Spawn { server: String, detail: String },
    #[error("mcp server `{server}` failed the handshake: {detail}")]
    Handshake { server: String, detail: String },
    #[error("mcp server `{server}`: {detail}")]
    Call { server: String, detail: String },
    #[error("no such tool: {0}")]
    NoSuchTool(String),
}

/// One tool a mounted server exposes.
#[derive(Debug, Clone)]
pub struct MountedTool {
    /// `mcp:{server}:{tool}` — the id docs/16 specifies, and the one
    /// `Origin::for_tool` decodes provenance from.
    pub id: String,
    /// The name to send back to the server, unqualified.
    pub remote_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl MountedTool {
    /// The stable-prefix form: name plus a one-line description, no schema.
    ///
    /// docs/16's schema hygiene rule. The full schema is still sent for tools the
    /// model is likely to use; this is what makes a 40-tool server affordable, since
    /// everything here is paid for on every turn of the session (ADR-008).
    pub fn brief(&self) -> String {
        let one_line = self
            .description
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(160)
            .collect::<String>();
        format!("{}: {}", self.id, one_line)
    }
}

/// A live MCP server and the tools it offered at mount time.
///
/// Tools are captured at mount rather than re-listed per call, because the tool set
/// is part of the stable prompt prefix: a server that changes its tools mid-session
/// is a cache break, and docs/13 makes that "a deliberate, logged act" rather than
/// something a third party can do to us silently.
pub struct MountedServer {
    name: String,
    tools: Vec<MountedTool>,
    service: Arc<RunningService<RoleClient, ()>>,
}

impl std::fmt::Debug for MountedServer {
    // A running service has no useful debug form; the mount point is what a caller
    // needs when a `Result<MountedServer, _>` is unwrapped.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MountedServer")
            .field("name", &self.name)
            .field("tools", &self.tools.len())
            .finish()
    }
}

impl MountedServer {
    /// Spawn the server, handshake, and list its tools.
    pub async fn mount(spec: &StdioServer) -> Result<Self, McpError> {
        let mut command = tokio::process::Command::new(&spec.command);
        command.args(&spec.args);
        // Cleared, then repopulated from `spec.env` only: the same rule T2 enforces
        // with `--clearenv` (docs/14). An MCP server that needs a token gets it
        // because a manifest declared it, not because we happened to have it.
        command.env_clear();
        for (k, v) in &spec.env {
            command.env(k, v);
        }
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }

        let transport = TokioChildProcess::new(command).map_err(|e| McpError::Spawn {
            server: spec.name.clone(),
            detail: e.to_string(),
        })?;

        let service = ().serve(transport).await.map_err(|e| McpError::Handshake {
            server: spec.name.clone(),
            detail: e.to_string(),
        })?;

        let listed = service
            .list_tools(None)
            .await
            .map_err(|e| McpError::Handshake {
                server: spec.name.clone(),
                detail: format!("tools/list: {e}"),
            })?;

        let tools = listed
            .tools
            .into_iter()
            .map(|t| MountedTool {
                id: format!("mcp:{}:{}", spec.name, t.name),
                remote_name: t.name.to_string(),
                description: t.description.map(|d| d.to_string()).unwrap_or_default(),
                input_schema: serde_json::Value::Object((*t.input_schema).clone()),
            })
            .collect();

        Ok(Self {
            name: spec.name.clone(),
            tools,
            service: Arc::new(service),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn tools(&self) -> &[MountedTool] {
        &self.tools
    }

    /// The index line for every tool, for the stable prefix.
    pub fn brief_index(&self) -> String {
        self.tools
            .iter()
            .map(|t| t.brief())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Call one tool by its mounted id.
    ///
    /// Returns the text content joined, which is what a `ToolOutcome` carries — an
    /// MCP result can hold images and resources, and a tool result that silently
    /// dropped them would be worse than one that says it did. Non-text content is
    /// reported as a line naming its type, so the model knows something was there.
    pub async fn call(
        &self,
        id: &str,
        args: serde_json::Value,
    ) -> Result<(String, bool), McpError> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| McpError::NoSuchTool(id.to_string()))?;

        let arguments = match args {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            // A non-object argument is not a valid MCP call; saying so is better
            // than sending it and getting a server-specific error back.
            other => {
                return Err(McpError::Call {
                    server: self.name.clone(),
                    detail: format!("arguments must be a JSON object, got {other}"),
                })
            }
        };

        let mut params = CallToolRequestParams::new(tool.remote_name.clone());
        if let Some(map) = arguments {
            params = params.with_arguments(map);
        }
        let result = self
            .service
            .call_tool(params)
            .await
            .map_err(|e| McpError::Call {
                server: self.name.clone(),
                detail: e.to_string(),
            })?;

        let mut text = Vec::new();
        for content in result.content.iter() {
            match content.as_text() {
                Some(t) => text.push(t.text.clone()),
                None => text.push(format!(
                    "[{} content omitted — this tool returned something other than text]",
                    content_kind(content)
                )),
            }
        }
        // `is_error` is the server's own judgement. Trusting it matters: an MCP tool
        // that reports failure should reach the model as a tool error so it can
        // correct, not as a success containing an error message.
        Ok((text.join("\n"), result.is_error.unwrap_or(false)))
    }

    /// Stop the server.
    ///
    /// Explicit rather than only on drop: a child process outliving its session is
    /// how a machine ends up with forty orphaned servers.
    pub async fn shutdown(self) -> Result<(), McpError> {
        match Arc::try_unwrap(self.service) {
            Ok(service) => service
                .cancel()
                .await
                .map(|_| ())
                .map_err(|e| McpError::Call {
                    server: self.name,
                    detail: e.to_string(),
                }),
            // Someone still holds a handle; dropping ours is all we can do, and the
            // process dies when the last one goes.
            Err(_) => Ok(()),
        }
    }
}

fn content_kind(content: &rmcp::model::ContentBlock) -> &'static str {
    if content.as_image().is_some() {
        "image"
    } else if content.as_resource().is_some() {
        "resource"
    } else {
        "unknown"
    }
}
