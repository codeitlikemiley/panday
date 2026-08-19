//! Secrets: the vault and the env-injection policy (M20.2, docs/20 T4).
//!
//! > "Secrets live in a vault table (or OS keychain locally), injected into sandbox
//! > env **only** when a tool's manifest declares the need and the permission
//! > engine approves; never rendered into model context."
//!
//! Three rules, and the third is the one that needs code rather than discipline:
//!
//! 1. **Declared.** A tool gets a secret only if its manifest asked for it — the
//!    `secrets:` grant a user consented to at install (docs/16).
//! 2. **Approved.** The permission engine decides; this module takes the decision
//!    as an input rather than making it, so there is one gate and not two.
//! 3. **Never in context.** A secret reaches a tool through the sandbox's
//!    environment, never through an argument or a prompt — and if a command prints
//!    one, `ScrubSecrets` removes it from the observation *before* the reducer, so
//!    it never enters the log either. That ordering is the whole trick: reduction is
//!    lossy and irreversible, so scrubbing after it would leave the secret in the
//!    artifact store.

use crate::hooks::Hook;
use panday_types::event::ReducedOutput;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Where secret values live. The PG vault table implements this at M17.x; a local
/// keychain-backed implementation is a `panday local` concern (docs/18).
pub trait SecretVault: Send + Sync {
    /// The value for a name, if this vault holds it.
    fn get(&self, name: &str) -> Option<String>;
    /// Names this vault can serve, for a consent prompt. **Not values** — a
    /// listing API that returned values would make every audit log a leak.
    fn names(&self) -> Vec<String>;
}

/// In-memory vault. What tests use, and what `panday local` starts from before a
/// keychain is wired.
#[derive(Default)]
pub struct MemoryVault {
    entries: Mutex<BTreeMap<String, String>>,
}

impl MemoryVault {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(self, name: &str, value: &str) -> Self {
        self.entries
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
        self
    }

    /// Load named variables from the process environment.
    ///
    /// Explicit names only — never "everything in the environment". A vault that
    /// swept the environment would hand a tool every credential the developer
    /// happened to have exported, which is the inheritance docs/20 forbids.
    pub fn from_env(names: &[&str]) -> Self {
        let vault = Self::new();
        for name in names {
            if let Ok(value) = std::env::var(name) {
                vault
                    .entries
                    .lock()
                    .unwrap()
                    .insert(name.to_string(), value);
            }
        }
        vault
    }
}

impl SecretVault for MemoryVault {
    fn get(&self, name: &str) -> Option<String> {
        self.entries.lock().unwrap().get(name).cloned()
    }
    fn names(&self) -> Vec<String> {
        self.entries.lock().unwrap().keys().cloned().collect()
    }
}

/// Why a requested secret was not injected. Returned rather than logged so the
/// caller can put it in front of a human — "the tool asked for a secret it was not
/// granted" is a consent problem, and a silent omission looks like a broken tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotDeclared { name: String },
    NotApproved { name: String },
    NotInVault { name: String },
}

/// What a tool's sandbox environment should contain.
///
/// `declared` is the manifest's `secrets:` list, `approved` is the gate's answer.
/// Both are required: a declaration without approval is a request, and an approval
/// without a declaration is a grant nobody consented to at install time.
pub fn env_for_tool(
    vault: &dyn SecretVault,
    declared: &[String],
    approved: &[String],
    requested: &[String],
) -> (Vec<(String, String)>, Vec<Refusal>) {
    let mut env = Vec::new();
    let mut refusals = Vec::new();

    for name in requested {
        if !declared.contains(name) {
            refusals.push(Refusal::NotDeclared { name: name.clone() });
            continue;
        }
        if !approved.contains(name) {
            refusals.push(Refusal::NotApproved { name: name.clone() });
            continue;
        }
        match vault.get(name) {
            Some(value) => env.push((name.clone(), value)),
            None => refusals.push(Refusal::NotInVault { name: name.clone() }),
        }
    }
    (env, refusals)
}

/// Replaces secret *values* in tool output before it reaches the reducer.
///
/// docs/20 T4 calls this the belt (the gateway's DLP rules being the suspenders).
/// It works on values, not patterns: a pattern list guesses at what a credential
/// looks like and misses the one that does not match, while the vault knows exactly
/// which strings are secret. Patterns still have a place — a secret we were never
/// told about — but that belongs to the gateway's rules, where a false positive
/// costs a redaction rather than a broken turn.
pub struct ScrubSecrets {
    vault: Arc<dyn SecretVault>,
    /// Below this length a "secret" is not distinctive enough to replace: a
    /// two-character value would rewrite half the output.
    min_len: usize,
}

pub const REDACTION: &str = "[redacted:secret]";

impl ScrubSecrets {
    pub fn new(vault: Arc<dyn SecretVault>) -> Self {
        Self { vault, min_len: 8 }
    }

    /// Scrub every known secret value out of `text`.
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for name in self.vault.names() {
            let Some(value) = self.vault.get(&name) else {
                continue;
            };
            if value.len() < self.min_len {
                continue;
            }
            if out.contains(&value) {
                out = out.replace(&value, REDACTION);
            }
        }
        out
    }
}

impl Hook for ScrubSecrets {
    fn name(&self) -> &str {
        "scrub_secrets"
    }

    /// `post_tool` sees the *reduced* output, which is too late to matter for the
    /// log — so this hook exists for the case where reduction kept the secret and
    /// something downstream (a subscriber, a UI) is about to see it. The
    /// pre-reduction scrub is `SessionActor`'s, applied to the raw observation.
    ///
    /// A hook cannot mutate `post_tool`'s argument, so all this can do is notice.
    /// Noticing is still worth it: it means a leak past the raw scrub shows up in
    /// the log as a warning rather than only in the output.
    fn post_tool(&self, tool: &str, output: &ReducedOutput) {
        if self.scrub(&output.text) != output.text {
            tracing::warn!(
                tool,
                "a secret value survived into reduced tool output — the raw scrub \
                 did not run, or the vault changed mid-turn"
            );
        }
    }
}
