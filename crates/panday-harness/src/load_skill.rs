//! The `load_skill` tool (docs/16 §skills, M16.2).
//!
//! > "the skills **index** (name + description) lives in the stable prefix; a
//! > skill's **body** loads into `~stable` when triggered — explicitly
//! > (`/deploy-check`), by the model (`load_skill` tool), or by the trigger
//! > classifier — and stays for the session (unloading churns cache, ADR-008)."
//!
//! This is the model-driven path. The tool does not itself mutate context: it
//! returns the body, and the actor appends it to the semi-stable band — because
//! only the actor owns the layout, and a tool reaching into it would be able to
//! break the cache invariant from outside.

use crate::tools::{Replay, SideEffects, Tool, ToolCtx, ToolOutcome, ToolReq, ToolSpec};
use panday_sandbox::SandboxTier;
use panday_types::Json;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Name → body, for the skills available to a session.
pub type SkillBodies = Arc<BTreeMap<String, String>>;

pub struct LoadSkill {
    bodies: SkillBodies,
}

impl LoadSkill {
    pub const NAME: &'static str = "load_skill";

    pub fn new(bodies: SkillBodies) -> Self {
        Self { bodies }
    }
}

#[async_trait::async_trait]
impl Tool for LoadSkill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description:
                "Load a skill's full instructions by name. Use when the skills index suggests \
                 one is relevant."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The skill's name from the index."}
                },
                "required": ["name"]
            }),
        }
    }

    fn requirements(&self) -> ToolReq {
        ToolReq {
            sandbox_tier: SandboxTier::T0InProcess,
            side_effects: SideEffects::None,
            independent: true,
            // Re-reading a body reaches the same state.
            replay: Replay::Safe,
        }
    }

    async fn call(&self, _ctx: ToolCtx, args: Json) -> ToolOutcome {
        let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
            return ToolOutcome {
                raw: "load_skill needs a `name`".into(),
                is_error: true,
            };
        };

        match self.bodies.get(name) {
            Some(body) => ToolOutcome {
                raw: body.clone(),
                is_error: false,
            },
            // Listing what IS available turns a hallucinated name into one
            // recoverable turn instead of repeated guessing.
            None => ToolOutcome {
                raw: format!(
                    "no skill named `{name}`. Available: {}",
                    self.bodies.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
                is_error: true,
            },
        }
    }
}
