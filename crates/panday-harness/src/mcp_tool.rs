//! Mounted MCP tools as loop tools (M16.3, docs/16 §MCP host).
//!
//! > "Mounted MCP tools appear in the ToolRegistry as `mcp:{server}:{tool}` behind
//! > the same `Tool` trait — the loop can't tell them from native tools; the
//! > *permission engine* can (MCP tools default to `Ask` until granted)."
//!
//! Both halves are here. The loop cannot tell, because this is a `Tool` like any
//! other. The permission engine can, because of what `requirements()` reports:
//! `SideEffects::Irreversible`, which forces `Ask` in every profile.
//!
//! ## Why Irreversible is the honest default
//!
//! We do not know what a third party's tool does. `github:create_issue` is not
//! reversible; `github:list_issues` is a pure read. MCP's `annotations` carry hints
//! (`readOnlyHint`, `destructiveHint`) but they are *the server's* claims about
//! itself, and treating an unverified claim as a permission grant is how a
//! consent model becomes decorative. So the default is the strictest reading, and a
//! grant is something a human gives per tool — docs/16's "Ask-at-first-use".

use crate::tools::{Replay, SideEffects, Tool, ToolCtx, ToolOutcome, ToolReq, ToolSpec};
use panday_plugins::mcp::{MountedServer, MountedTool};
use panday_sandbox::SandboxTier;
use panday_types::Json;
use std::sync::Arc;

/// One mounted MCP tool.
pub struct McpTool {
    server: Arc<MountedServer>,
    tool: MountedTool,
    /// Set once a human has granted this specific tool (docs/16
    /// §Ask-at-first-use). Per tool, not per server: consenting to
    /// `github:list_issues` is not consenting to `github:delete_repo`.
    granted: bool,
}

impl McpTool {
    pub fn new(server: Arc<MountedServer>, tool: MountedTool) -> Self {
        Self {
            server,
            tool,
            granted: false,
        }
    }

    /// Every tool a mounted server exposes, ready for the registry.
    pub fn all(server: Arc<MountedServer>) -> Vec<Box<dyn Tool>> {
        server
            .tools()
            .iter()
            .cloned()
            .map(|t| Box::new(McpTool::new(server.clone(), t)) as Box<dyn Tool>)
            .collect()
    }

    /// Record that a human granted this tool. Only downgrades the *gate*, never the
    /// sandbox tier — a granted MCP tool still runs as a child process under T2.
    pub fn granted(mut self) -> Self {
        self.granted = true;
        self
    }

    pub fn id(&self) -> &str {
        &self.tool.id
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.tool.id.clone(),
            description: self.tool.description.clone(),
            parameters: self.tool.input_schema.clone(),
        }
    }

    fn requirements(&self) -> ToolReq {
        ToolReq {
            // The server is a child process, so the tier that describes it is T2 —
            // and that is what the sandbox-seconds metric will attribute it to.
            sandbox_tier: SandboxTier::T2OsJail,
            side_effects: if self.granted {
                // Still not `None`: a granted tool is one a human said yes to, not
                // one we have established is a pure read.
                SideEffects::Idempotent
            } else {
                SideEffects::Irreversible
            },
            // Two MCP calls can overlap: the server is a separate process and the
            // protocol is request/response with ids.
            independent: true,
            // Never replayed on resume. We cannot know whether a third party's tool
            // was a read or a write, and re-running a `create_issue` after a crash
            // is the kind of mistake that reaches a human's inbox.
            replay: Replay::Unsafe,
        }
    }

    async fn call(&self, _ctx: ToolCtx, args: Json) -> ToolOutcome {
        match self.server.call(&self.tool.id, args).await {
            Ok((text, is_error)) => ToolOutcome {
                raw: text,
                is_error,
            },
            // A server that died or timed out is a tool error, not a harness error:
            // the loop reports it to the model and carries on, exactly as it would
            // for a failed native tool.
            Err(e) => ToolOutcome {
                raw: format!("mcp call failed: {e}"),
                is_error: true,
            },
        }
    }
}
