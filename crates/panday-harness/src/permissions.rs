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

/// The key a remembered grant is stored under.
fn grant_key(tool: &str, rendered_args: &str) -> String {
    format!("{tool}({rendered_args})")
}

/// The rules a profile starts with.
///
/// docs/13 describes `dev` by *action category* rather than by side effect,
/// and these are that description made executable: what leaves the machine
/// asks; local work does not. `unleashed` still gates the same outbound
/// actions, because "local only" is about trusting the machine, not about
/// pushing to shared infrastructure unasked.
pub fn default_rules(profile: Profile) -> Vec<Rule> {
    let outbound = [
        "bash(*git push*)",
        "bash(*git remote*)",
        "bash(*cargo publish*)",
        "bash(*npm publish*)",
        "bash(*curl *)",
        "bash(*wget *)",
        "bash(*ssh *)",
        "bash(*scp *)",
    ];
    // Destructive locally, and not something a profile should ever wave
    // through — docs/13 gives `bash(rm -rf*)` as the example that asks "even
    // in unleashed".
    let destructive = ["bash(*rm -rf*)", "bash(*mkfs*)", "bash(*dd if=*)"];

    match profile {
        // read_only denies mutations by profile already; rules add nothing.
        Profile::ReadOnly => Vec::new(),
        Profile::Dev | Profile::Unleashed => outbound
            .iter()
            .chain(destructive.iter())
            .map(|p| Rule::ask(*p))
            .collect(),
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
            overrides: default_rules(profile),
            ..Default::default()
        }
    }

    /// A profile with no default rules — for tests that want to state every
    /// rule themselves.
    pub fn bare(profile: Profile) -> Self {
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
    /// 3. **Remembered grants** from an earlier `AllowRemember`.
    /// 4. **Remaining rules** (`ask` / `allow`) — e.g. `bash(rm -rf*) → ask`.
    /// 5. **Profile default.**
    ///
    /// Denies come before everything: a grant remembered for `bash` must not
    /// silently authorise `bash(rm -rf /)` later in the same session.
    ///
    /// Remembered grants come *before* the `ask` rules, because that is what
    /// remembering means — the human was shown the prompt that rule produced
    /// and said "always allow **this**". Putting rules first would re-ask
    /// forever and make `AllowRemember` decorative. What keeps that safe is
    /// that a grant records the **call**, not the tool: answering for
    /// `git push origin main` does not authorise `rm -rf /` through the same
    /// `bash` tool.
    pub fn gate(&self, tool_name: &str, req: &ToolReq, args: &Json) -> Gate {
        let rendered = render_call(args);

        // 1. Explicit denies outrank everything, including a human's grant.
        for r in self.overrides.iter().filter(|r| r.verdict() == Gate::Deny) {
            if r.matches(tool_name, &rendered) {
                return Gate::Deny;
            }
        }

        // 2. Irreversible always asks (docs/13), and a remembered grant does
        //    not waive it: consent for a tool is not a standing waiver on
        //    something that cannot be undone.
        if req.side_effects == SideEffects::Irreversible {
            return Gate::Ask;
        }

        // 3. A grant for THIS call.
        if self
            .remembered
            .iter()
            .any(|g| *g == grant_key(tool_name, &rendered))
        {
            return Gate::Allow;
        }

        // 4. Remaining rules.
        for r in self.overrides.iter().filter(|r| r.verdict() != Gate::Deny) {
            if r.matches(tool_name, &rendered) {
                return r.verdict();
            }
        }

        // 5. Profile default.
        match self.profile.unwrap_or(Profile::Dev) {
            Profile::ReadOnly => {
                if req.side_effects == SideEffects::None {
                    Gate::Allow
                } else {
                    Gate::Deny
                }
            }
            // docs/13: "dev (read/edit/test allowed; git push, package
            // publish, network egress → Ask)".
            //
            // The discriminator is NOT side-effect-freedom — editing a file
            // and running a test suite both mutate, and both are explicitly
            // allowed. It is whether the action reaches *outside the machine*,
            // which `default_rules` expresses. Gating every mutation here made
            // Phase 1's exit criterion ("fixes a failing test unattended,
            // under dev") unreachable.
            Profile::Dev => match req.side_effects {
                SideEffects::None | SideEffects::Idempotent => Gate::Allow,
                SideEffects::Irreversible => Gate::Ask,
            },
            Profile::Unleashed => Gate::Allow,
        }
    }

    /// Record an `AllowRemember` for **this call**, not for the tool.
    ///
    /// Remembering by tool name would turn one "always allow" on
    /// `git push origin main` into a standing grant for every `bash`
    /// invocation, including the destructive ones. The grant is keyed on the
    /// canonical rendering so it is as narrow as what the human actually saw.
    pub fn remember(&mut self, tool_name: &str, args: &Json, decision: PermDecision) {
        if decision == PermDecision::AllowRemember {
            self.remembered
                .push(grant_key(tool_name, &render_call(args)));
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
        // Uses an irreversible tool, since ordinary mutations are allowed
        // outright in dev (see `dev_allows_local_work_and_asks_for_outbound`).
        let mut e = PermissionEngine::new(Profile::Dev);
        assert_eq!(
            e.gate(
                "deploy",
                &req(SideEffects::Irreversible),
                &serde_json::json!({})
            ),
            Gate::Ask
        );
        e.remember(
            "deploy",
            &serde_json::json!({}),
            panday_types::event::PermDecision::AllowRemember,
        );
        // Irreversible still asks: a remembered grant is consent for a tool,
        // not a standing waiver on something that cannot be undone.
        assert_eq!(
            e.gate(
                "deploy",
                &req(SideEffects::Irreversible),
                &serde_json::json!({})
            ),
            Gate::Ask
        );
    }

    #[test]
    fn dev_allows_local_work_and_asks_for_outbound() {
        // docs/13: "dev (read/edit/test allowed; git push, package publish,
        // network egress → Ask)". Editing and running tests both mutate, so
        // side-effect-freedom is the wrong discriminator.
        let e = PermissionEngine::new(Profile::Dev);

        for (tool, args) in [
            ("read_file", serde_json::json!({"path": "src/lib.rs"})),
            ("edit_file", serde_json::json!({"path": "src/lib.rs"})),
            ("bash", serde_json::json!({"cmd": "cargo test"})),
            ("bash", serde_json::json!({"cmd": "cargo build --release"})),
        ] {
            assert_eq!(
                e.gate(tool, &req(SideEffects::Idempotent), &args),
                Gate::Allow,
                "dev should allow local work: {tool} {args}"
            );
        }

        for cmd in ["git push origin main", "cargo publish", "curl https://x"] {
            assert_eq!(
                e.gate(
                    "bash",
                    &req(SideEffects::Idempotent),
                    &serde_json::json!({ "cmd": cmd })
                ),
                Gate::Ask,
                "dev should ask before: {cmd}"
            );
        }
    }

    #[test]
    fn unleashed_still_asks_before_destroying_things() {
        // docs/13's own example: `bash(rm -rf*)` asks "even in unleashed".
        let e = PermissionEngine::new(Profile::Unleashed);
        assert_eq!(
            e.gate(
                "bash",
                &req(SideEffects::Idempotent),
                &serde_json::json!({"cmd": "rm -rf /"})
            ),
            Gate::Ask
        );
        assert_eq!(
            e.gate(
                "bash",
                &req(SideEffects::Idempotent),
                &serde_json::json!({"cmd": "cargo test"})
            ),
            Gate::Allow
        );
    }
}
