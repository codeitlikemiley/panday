//! The Tool trait and registry types (docs/13 §tools). Native, plugin-WASM,
//! and MCP-mounted tools all implement this — the loop cannot tell them
//! apart, which is the point.

use async_trait::async_trait;
use ferrum_sandbox::SandboxTier;
use ferrum_types::{AccountId, Json, SessionId, TurnId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for arguments.
    pub parameters: Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffects {
    /// Pure read; safe to replay after a crash.
    None,
    /// Mutating but idempotent (same args → same end state).
    Idempotent,
    /// Cannot be safely replayed → forces `Ask` regardless of profile
    /// (docs/13 §persist-before-proceed).
    Irreversible,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolReq {
    pub sandbox_tier: SandboxTier,
    pub side_effects: SideEffects,
    /// May run in parallel with other independent tools this step.
    pub independent: bool,
}

#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub account: AccountId,
    pub session: SessionId,
    pub turn: TurnId,
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    /// Raw output — the harness runs it through the reducer before context.
    pub raw: String,
    pub is_error: bool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn requirements(&self) -> ToolReq;
    async fn call(&self, ctx: ToolCtx, args: Json) -> ToolOutcome;
}

/// Enumerable, versioned registry. Tool schemas are part of the stable
/// prompt prefix — mutating the set mid-session is a cache break and
/// therefore a deliberate, logged act (ADR-008).
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|b| b.as_ref())
    }
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }
}
