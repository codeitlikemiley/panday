//! The permission engine (docs/13 §permissions). Decisions are events, so
//! grants are auditable and replayable like everything else.

use crate::tools::{SideEffects, ToolReq};
use ferrum_types::event::PermDecision;
use ferrum_types::Json;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// Reads only; everything mutating → Deny.
    ReadOnly,
    /// read/edit/test allowed; push/publish/egress → Ask.
    Dev,
    /// Local machines only. Still gates `Irreversible`.
    Unleashed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Allow,
    Deny,
    Ask,
}

/// Pattern rules layered over the profile, e.g. `bash(rm -rf*) → Ask` even
/// in Unleashed. v1 patterns are simple prefix globs on a canonical
/// `tool(arg-summary)` string; a real matcher is M13.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub pattern: String,
    pub gate: String, // "allow" | "deny" | "ask"
}

#[derive(Debug, Clone, Default)]
pub struct PermissionEngine {
    pub profile: Option<Profile>,
    pub overrides: Vec<Rule>,
    /// Remembered per-session/project grants (from AllowRemember decisions).
    pub remembered: Vec<String>,
}

impl PermissionEngine {
    pub fn new(profile: Profile) -> Self {
        Self {
            profile: Some(profile),
            ..Default::default()
        }
    }

    /// Decide the gate for a call. Order: irreversible-forces-Ask →
    /// remembered grants → override rules → profile default.
    pub fn gate(&self, tool_name: &str, req: &ToolReq, _args: &Json) -> Gate {
        if req.side_effects == SideEffects::Irreversible {
            return Gate::Ask; // non-negotiable (docs/13)
        }
        if self.remembered.iter().any(|g| g == tool_name) {
            return Gate::Allow;
        }
        for r in &self.overrides {
            if tool_name.starts_with(r.pattern.trim_end_matches('*')) {
                return match r.gate.as_str() {
                    "allow" => Gate::Allow,
                    "deny" => Gate::Deny,
                    _ => Gate::Ask,
                };
            }
        }
        match self.profile.unwrap_or(Profile::Dev) {
            Profile::ReadOnly => {
                if req.side_effects == SideEffects::None {
                    Gate::Allow
                } else {
                    Gate::Deny
                }
            }
            Profile::Dev => {
                if req.side_effects == SideEffects::None {
                    Gate::Allow
                } else {
                    Gate::Ask
                }
            }
            Profile::Unleashed => Gate::Allow,
        }
    }

    pub fn remember(&mut self, tool_name: &str, decision: PermDecision) {
        if decision == PermDecision::AllowRemember {
            self.remembered.push(tool_name.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrum_sandbox::SandboxTier;

    fn req(se: SideEffects) -> ToolReq {
        ToolReq {
            sandbox_tier: SandboxTier::T2OsJail,
            side_effects: se,
            independent: true,
        }
    }

    #[test]
    fn irreversible_always_asks_even_unleashed() {
        let e = PermissionEngine::new(Profile::Unleashed);
        assert_eq!(
            e.gate(
                "bash",
                &req(SideEffects::Irreversible),
                &serde_json::json!({})
            ),
            Gate::Ask
        );
    }

    #[test]
    fn read_only_denies_mutations_allows_reads() {
        let e = PermissionEngine::new(Profile::ReadOnly);
        assert_eq!(
            e.gate("read_file", &req(SideEffects::None), &serde_json::json!({})),
            Gate::Allow
        );
        assert_eq!(
            e.gate(
                "write_file",
                &req(SideEffects::Idempotent),
                &serde_json::json!({})
            ),
            Gate::Deny
        );
    }

    #[test]
    fn remembered_grant_skips_ask_in_dev() {
        let mut e = PermissionEngine::new(Profile::Dev);
        assert_eq!(
            e.gate(
                "bash",
                &req(SideEffects::Idempotent),
                &serde_json::json!({})
            ),
            Gate::Ask
        );
        e.remember("bash", ferrum_types::event::PermDecision::AllowRemember);
        assert_eq!(
            e.gate(
                "bash",
                &req(SideEffects::Idempotent),
                &serde_json::json!({})
            ),
            Gate::Allow
        );
    }
}
