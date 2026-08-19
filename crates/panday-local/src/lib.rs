//! `panday local` — the offline composition (M18.1, docs/18).
//!
//! > "`panday local` boots harness+gateway-lite+CLI against an already-running
//! > llama-server; end-to-end turn with tools, no network."
//!
//! One process: harness, the same gateway library with only the `local` adapter, the
//! router with the file collapsed to `local-only`, native tools in a T2 jail, and a
//! single-file event log. ADR-010 and the vision's "offline is a deployment target".
//!
//! ## Zero egress is enforced, not intended
//!
//! `LocalConfig::boot` **refuses a non-loopback base URL**. Everything else in the
//! design points the same way — one adapter, one pool, a policy file with nowhere else
//! to go — but all of that is configuration, and configuration is what gets changed by
//! someone in a hurry. A loopback check is the one part of "zero egress" that cannot be
//! reconfigured into egress by editing a YAML file.
//!
//! ## What "gateway-lite" means here
//!
//! The same `panday-gateway`, built with one adapter. Not a smaller reimplementation:
//! docs/18 calls the offline tier "the forcing function that keeps every interface
//! honest", and a second gateway would be the first thing to drift. The metering,
//! routing, cache and breaker paths are the cloud's, exercised locally.

pub mod sqlite;

use panday_harness::native::{register_native, Workspace};
use panday_harness::tools::ToolRegistry;
use panday_harness::{
    JsonlStore, PermissionEngine, Profile, ReplayOptions, SessionActor, TurnBudget, TurnOutcome,
};
use panday_sandbox::{
    FsPolicy, Limits, NetPolicy, Sandbox, SandboxPolicy, SandboxTier, SessionSpec,
};
use panday_types::model::{ModelRef, StopReason};
use panday_types::{AccountId, SessionId};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The offline system prompt.
///
/// Short: every token here is paid on every turn of a session running on a machine whose
/// window is measured in thousands rather than hundreds of thousands. The capability
/// constraints are appended by `ContextBuilder::with_capabilities`, so this says what the
/// agent is *for* and the profile says what it can do.
const SYSTEM_PROMPT: &str = "You are Panday, a coding agent running locally on the user's \
machine. You have a jailed workspace and no network. Work in small steps, check your work \
with the tools you have, and say when something is beyond what you can do here.";

/// The policy with nowhere to go but local.
pub const LOCAL_POLICY: &str = include_str!("../../panday-router/policy/local.yaml");

#[derive(Debug, Clone)]
pub struct LocalConfig {
    /// An OpenAI-compatible server on loopback: llama-server, mistral.rs, anything.
    /// docs/18: "the gateway's `local` adapter doesn't care which".
    pub base_url: String,
    pub model: ModelRef,
    pub workspace: PathBuf,
    /// Single-file append-only log. SQLite is M18.3; this is the same shape and needs no
    /// schema (`panday_harness::JsonlStore`).
    pub log: PathBuf,
    pub profile: Profile,
    /// What the local model can do (docs/18 §degraded-capability honesty, M18.4).
    ///
    /// Declared, not measured — M19.2 measures them, and the system prompt says
    /// "(estimated)" until it does.
    pub capabilities: panday_types::CapabilityProfile,
}

impl LocalConfig {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            base_url: "http://127.0.0.1:8080".into(),
            model: ModelRef("local/qwen3.5-4b".into()),
            log: workspace.join(".panday/session.jsonl"),
            workspace,
            // A local session is still gated: docs/13's profiles are about what a human
            // is willing to have happen, and running a model on your own laptop does not
            // make `rm -rf` welcome.
            profile: Profile::Dev,
            capabilities: panday_types::CapabilityProfile::small_local(),
        }
    }

    /// Override the capability profile — what a bigger local model gets.
    pub fn capabilities(mut self, profile: panday_types::CapabilityProfile) -> Self {
        self.capabilities = profile;
        self
    }

    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = ModelRef(model.into());
        self
    }

    pub fn profile(mut self, profile: Profile) -> Self {
        self.profile = profile;
        self
    }

    pub fn log(mut self, path: impl Into<PathBuf>) -> Self {
        self.log = path.into();
        self
    }
}

/// Whether a URL points at this machine.
///
/// Host-based rather than DNS-resolving: resolving would be a network call, and a name
/// that resolves to loopback today can resolve elsewhere tomorrow — which is exactly the
/// trick this is here to refuse.
pub fn is_loopback(url: &str) -> bool {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    let host = rest
        .split('/')
        .next()
        .unwrap_or_default()
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or_else(|| rest.split('/').next().unwrap_or_default());
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "0.0.0.0"
}

#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("`panday local` will not talk to `{0}`: the offline tier is loopback-only, and a remote base URL is egress by another name")]
    NotLoopback(String),
    #[error("workspace {0}: {1}")]
    Workspace(String, String),
    #[error("sandbox: {0}")]
    Sandbox(String),
    #[error("policy: {0}")]
    Policy(String),
    #[error("log {0}: {1}")]
    Log(String, String),
    #[error("turn: {0}")]
    Turn(String),
}

/// A booted offline session.
pub struct Local {
    actor: SessionActor,
    store: Arc<JsonlStore>,
    usage: Arc<panday_gateway::CollectUsage>,
    renderer: panday_harness::replay::Renderer,
    /// How many events have already been rendered, so a second turn does not reprint the
    /// first. The log is the source of truth for what to render — the same fold a replay
    /// does (ADR-002).
    rendered: usize,
}

impl Local {
    /// Boot the whole composition.
    pub async fn boot(config: LocalConfig) -> Result<Self, LocalError> {
        if !is_loopback(&config.base_url) {
            return Err(LocalError::NotLoopback(config.base_url));
        }

        let workspace = config.workspace.canonicalize().map_err(|e| {
            LocalError::Workspace(config.workspace.display().to_string(), e.to_string())
        })?;

        if let Some(parent) = config.log.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| LocalError::Log(parent.display().to_string(), e.to_string()))?;
        }
        let store = Arc::new(
            JsonlStore::open(&config.log)
                .map_err(|e| LocalError::Log(config.log.display().to_string(), e.to_string()))?,
        );

        // Adopt the session the log already holds. A laptop closes mid-turn; reopening the
        // file *is* the recovery story (ADR-002), and a fresh session id would start at
        // seq 1 in a file that already has one — which the store rejects as a
        // single-writer violation, correctly. One file, one session.
        let existing = panday_harness::read_log(&config.log)
            .map_err(|e| LocalError::Log(config.log.display().to_string(), e.to_string()))?;
        let session = existing
            .first()
            .map(|e| e.session_id)
            .unwrap_or_else(SessionId::new);

        // gateway-lite: the same gateway, one adapter, one pool.
        let router = panday_router::PolicyRouter::from_yaml(LOCAL_POLICY)
            .map_err(|e| LocalError::Policy(e.to_string()))?;
        let usage = Arc::new(panday_gateway::CollectUsage::new());
        let gateway = panday_gateway::Gateway::builder(Arc::new(router))
            .usage_sink(usage.clone())
            .adapter(
                "local",
                Arc::new(
                    panday_gateway::adapters::openai_compat::OpenAiCompat::local(
                        config.base_url.clone(),
                    ),
                ) as Arc<dyn panday_gateway::ProviderAdapter>,
            )
            .build();

        let tools = native_tools(&workspace).await?;

        // The adaptations docs/18 asks for, applied where they belong: the stable band
        // (system prompt, tool set, window). The reducer's aggressive mode is chosen from
        // the same profile below.
        let context = panday_harness::context::ContextBuilder::new(
            SYSTEM_PROMPT,
            tools
                .specs()
                .into_iter()
                .map(|s| panday_types::model::ToolDef {
                    name: s.name,
                    description: s.description,
                    parameters: s.parameters,
                })
                .collect(),
        )
        .with_capabilities(config.capabilities);

        let mut actor = SessionActor::new(
            session,
            AccountId::new(),
            config.model.clone(),
            store.clone(),
            Arc::new(gateway),
            tools,
            PermissionEngine::new(config.profile),
            // Aggressive locally: docs/18 §degraded-capability honesty. A local model has
            // less context to spend, so the reducer spends it more carefully.
            Box::new(panday_reducer::SpillingReducer::new(
                panday_reducer::StructuralReducer::new(panday_reducer::GenericReducer::default()),
                Arc::new(panday_reducer::MemoryArtifactStore::default()),
            )),
            TurnBudget::default(),
        )
        .with_context(context);
        // Fold the log back into memory, then finish anything that was in flight when the
        // process died — `resume_pending` refuses to replay what is not replay-safe
        // (docs/13), which is why this is safe to do on every boot.
        actor
            .resume()
            .await
            .map_err(|e| LocalError::Turn(e.to_string()))?;
        actor
            .resume_pending()
            .await
            .map_err(|e| LocalError::Turn(e.to_string()))?;

        Ok(Self {
            actor,
            store,
            usage,
            renderer: panday_harness::replay::Renderer::new(ReplayOptions {
                costs: true,
                ..Default::default()
            }),
            // Everything already in the log belongs to a previous run; this boot renders
            // only what it does itself.
            rendered: existing.len(),
        })
    }

    pub fn session(&self) -> SessionId {
        self.actor.session()
    }

    pub fn log_path(&self) -> &Path {
        self.store.path()
    }

    /// Run one turn and return it rendered the way `panday replay` would show it.
    ///
    /// The same `Renderer` the CLI and the replay use, so an offline session, a hosted
    /// session and a replay are one format. docs/18's "the client cannot tell" is easier
    /// to keep true when there is one renderer than when there are three.
    pub async fn turn(&mut self, prompt: &str) -> Result<String, LocalError> {
        let before = self.store.path().to_path_buf();
        let outcome = self
            .actor
            .handle_user_input(prompt)
            .await
            .map_err(|e| LocalError::Turn(e.to_string()))?;

        let events = panday_harness::read_log(&before)
            .map_err(|e| LocalError::Log(before.display().to_string(), e.to_string()))?;
        let mut out = String::new();
        for envelope in &events[self.rendered..] {
            out.push_str(&self.renderer.push(envelope));
        }
        self.rendered = events.len();

        if let TurnOutcome::AwaitingPermission(_) = outcome {
            out.push_str("\n  (waiting for a decision — answer with `allow` or `deny`)\n");
        }
        Ok(out)
    }

    /// What this session spent. Zero in money terms locally, which is the point —
    /// docs/18 still meters it ("usage syncs later"), and reporting real token counts
    /// with a zero price is how the free tier stays honest rather than unmeasured.
    pub fn usage(&self) -> Vec<panday_gateway::UsageRecord> {
        self.usage.take()
    }

    /// Answer a parked permission request.
    pub async fn decide(&mut self, allow: bool) -> Result<String, LocalError> {
        use panday_types::event::{Actor, PermDecision};
        let parked = self.actor.parked_calls();
        let mut out = String::new();
        for call in parked {
            self.actor
                .decide(
                    call,
                    if allow {
                        PermDecision::Allow
                    } else {
                        PermDecision::Deny
                    },
                    Actor::User,
                )
                .await
                .map_err(|e| LocalError::Turn(e.to_string()))?;
        }
        let events = panday_harness::read_log(self.store.path())
            .map_err(|e| LocalError::Log(self.store.path().display().to_string(), e.to_string()))?;
        for envelope in &events[self.rendered..] {
            out.push_str(&self.renderer.push(envelope));
        }
        self.rendered = events.len();
        Ok(out)
    }
}

/// The native tool set, scoped to the workspace and jailed at T2.
async fn native_tools(workspace: &Path) -> Result<ToolRegistry, LocalError> {
    #[cfg(target_os = "macos")]
    let sandbox = Arc::new(panday_sandbox::T2MacosSandbox::new());
    #[cfg(target_os = "linux")]
    let sandbox = Arc::new(panday_sandbox::T2LinuxSandbox::new());
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err(LocalError::Sandbox("no T2 sandbox on this platform".into()));

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let handle = sandbox
            .create(SessionSpec {
                tier: SandboxTier::T2OsJail,
                policy: SandboxPolicy {
                    fs: FsPolicy {
                        workspace_rw: workspace.to_path_buf(),
                        staged_ro: vec![],
                    },
                    // Deny by default, and locally there is nothing to allow: the model
                    // is on loopback and the tools have no reason to leave the machine.
                    net: NetPolicy::default(),
                    limits: Limits {
                        wall_clock_ms: 180_000,
                        ..Default::default()
                    },
                    env: toolchain_env(),
                },
            })
            .await
            .map_err(|e| LocalError::Sandbox(e.to_string()))?;

        let mut registry = ToolRegistry::default();
        register_native(
            &mut registry,
            Workspace::new(sandbox as Arc<dyn Sandbox>, handle, workspace.to_path_buf()),
        );
        Ok(registry)
    }
}

/// Toolchain variables only — never the parent environment (docs/20 T4).
fn toolchain_env() -> Vec<(String, String)> {
    ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

/// How a turn ended, for a caller that wants the reason rather than the rendering.
pub fn stop_reason(rendered: &str) -> Option<StopReason> {
    for (needle, reason) in [
        ("EndTurn", StopReason::EndTurn),
        ("MaxSteps", StopReason::MaxSteps),
        ("BudgetExceeded", StopReason::BudgetExceeded),
        ("Cancelled", StopReason::Cancelled),
    ] {
        if rendered.contains(needle) {
            return Some(reason);
        }
    }
    None
}
