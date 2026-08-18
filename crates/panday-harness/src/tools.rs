//! The Tool trait and registry types (docs/13 §tools). Native, plugin-WASM,
//! and MCP-mounted tools all implement this — the loop cannot tell them
//! apart, which is the point.

use async_trait::async_trait;
use panday_sandbox::SandboxTier;
use panday_types::{AccountId, Json, SessionId, TurnId};
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

/// Whether resume may re-run a call whose result never made it to the log.
///
/// **Separate from [`SideEffects`] on purpose.** That field drives *consent*
/// — which profiles prompt — and the two answers genuinely differ for
/// `bash`: docs/13's profile table says `dev` allows running tests, so the
/// tool cannot be `Irreversible` (that would force Ask in every profile and
/// make an unattended run impossible), yet replaying an arbitrary shell
/// command after a crash is plainly unsafe. Collapsing both decisions into
/// one field forces a wrong answer to one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Replay {
    /// Re-running reaches the same end state: reads, and writes of known
    /// content.
    Safe,
    /// May have already taken effect, and re-running could compound it.
    /// Resume refuses these and surfaces the call's arguments, since the tool
    /// may or may not have run (docs/13 §persist-before-proceed).
    Unsafe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolReq {
    pub sandbox_tier: SandboxTier,
    pub side_effects: SideEffects,
    /// May run in parallel with other independent tools this step.
    pub independent: bool,
    /// Whether crash-resume may re-run this call. See [`Replay`].
    #[serde(default = "default_replay")]
    pub replay: Replay,
}

/// Refusing is the safe default: a tool that has not thought about replay
/// should not be silently re-run.
fn default_replay() -> Replay {
    Replay::Unsafe
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
