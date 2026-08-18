//! The permission engine (docs/13 §permissions). Decisions are events, so
//! grants are auditable and replayable like everything else.

use crate::tools::{SideEffects, ToolReq};
use panday_types::event::PermDecision;
use panday_types::Json;
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

/// A rule matched against a call, e.g. `bash(rm -rf*) → ask` even in
/// `Unleashed` (docs/13 §permissions).
///
/// Patterns are `tool(arg-glob)`, or a bare `tool` to match any arguments.
/// The argument side is matched against the call's **canonical rendering**
/// (see [`render_call`]) rather than raw JSON, so a rule does not depend on
/// key order or whitespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub pattern: String,
    pub gate: String, // "allow" | "deny" | "ask"
}

impl Rule {
    pub fn ask(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            gate: "ask".into(),
        }
    }
    pub fn deny(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            gate: "deny".into(),
        }
    }
    pub fn allow(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            gate: "allow".into(),
        }
    }

    fn verdict(&self) -> Gate {
        match self.gate.as_str() {
            "allow" => Gate::Allow,
            "deny" => Gate::Deny,
            _ => Gate::Ask,
        }
    }

    /// Split `tool(arg-glob)` into its parts.
    fn parts(&self) -> (&str, Option<&str>) {
        match self.pattern.split_once('(') {
            Some((tool, rest)) => (tool.trim(), Some(rest.trim_end_matches(')'))),
            None => (self.pattern.trim(), None),
        }
    }

    fn matches(&self, tool: &str, rendered_args: &str) -> bool {
        let (pat_tool, pat_args) = self.parts();
        if pat_tool != tool && pat_tool != "*" {
            return false;
        }
        match pat_args {
            None => true,
            Some(glob) => glob_match(glob, rendered_args),
        }
    }
}

/// `*` matches any run of characters; everything else is literal.
///
/// Deliberately not a regex: these rules are written by hand in config by
/// people protecting themselves, and a mis-typed regex that silently matches
/// nothing is a security failure that looks like success.
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut idx = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match text[idx..].find(part) {
            Some(found) => {
                // A leading literal must match at the very start.
                if i == 0 && found != 0 {
                    return false;
                }
                idx += found + part.len();
            }
            None => return false,
        }
    }
    // A trailing literal must reach the end.
    match parts.last() {
        Some(last) if !last.is_empty() => text.ends_with(last),
        _ => true,
    }
}

/// Canonical `arg-summary` a rule is matched against.
///
/// Values are joined in key order so a rule cannot be evaded by reordering
/// JSON keys, and the whole thing is lowercased so `RM -RF` cannot slip past
/// a rule written in lower case.
pub fn render_call(args: &Json) -> String {
    let mut parts: Vec<String> = Vec::new();
    match args {
        Json::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                parts.push(match &map[k] {
                    Json::String(s) => s.clone(),
                    other => other.to_string(),
                });
            }
        }
        Json::String(s) => parts.push(s.clone()),
        other => parts.push(other.to_string()),
    }
    parts.join(" ").to_lowercase()
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

    /// Add a rule (docs/13 §permissions).
    pub fn with_rule(mut self, rule: Rule) -> Self {
        self.overrides.push(rule);
        self
    }

    /// Decide the gate for a call.
    ///
    /// Order matters and is the security model:
    ///
    /// 1. **Explicit `deny` rules** — nothing overrides an operator's "never".
    /// 2. **Irreversible** forces Ask, non-negotiable (docs/13).
    /// 3. **Remaining rules** (`ask` / `allow`) — e.g. `bash(rm -rf*) → ask`.
    /// 4. **Remembered grants** from an earlier `AllowRemember`.
    /// 5. **Profile default.**
    ///
    /// Denies are checked before everything, including remembered grants: a
    /// grant remembered for `bash` must not silently authorise
    /// `bash(rm -rf /)` later in the same session.
    pub fn gate(&self, tool_name: &str, req: &ToolReq, args: &Json) -> Gate {
        let rendered = render_call(args);

        for r in self.overrides.iter().filter(|r| r.verdict() == Gate::Deny) {
            if r.matches(tool_name, &rendered) {
                return Gate::Deny;
            }
        }

        if req.side_effects == SideEffects::Irreversible {
            return Gate::Ask; // non-negotiable (docs/13)
        }

        for r in self.overrides.iter().filter(|r| r.verdict() != Gate::Deny) {
            if r.matches(tool_name, &rendered) {
                return r.verdict();
            }
        }

        if self.remembered.iter().any(|g| g == tool_name) {
            return Gate::Allow;
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
    use panday_sandbox::SandboxTier;

    fn req(se: SideEffects) -> ToolReq {
        ToolReq {
            sandbox_tier: SandboxTier::T2OsJail,
            side_effects: se,
            independent: true,
            replay: crate::tools::Replay::Unsafe,
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
        e.remember("bash", panday_types::event::PermDecision::AllowRemember);
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
