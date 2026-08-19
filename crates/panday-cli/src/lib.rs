//! `panday` — the CLI (ADR-012: one codebase, TUI + ACP server).
//!
//! M0.1 ships exactly one subcommand, `chat`, because it is the Phase 0 exit
//! criterion: "`panday-cli chat` streams through your gateway from two
//! providers and a local llama-server, with usage recorded per call."
//! The ratatui TUI and the ACP server come later (docs/16 M16.5).
//!
//! ## Why the gateway runs in-process
//!
//! docs/01 §Deployment shapes calls this the dev/solo composition —
//! "everything in one process". The gateway's HTTP ingress is M11.5, so
//! there is nothing to connect to yet; embedding the library is not a
//! shortcut around the "only door" rule (ADR-006) but the same door, linked
//! rather than dialled. When M11.5 lands, this swaps to
//! `panday_sdk::gateway::connect()` and nothing else changes.

pub mod acp;
pub mod acp_server;
pub mod plugin_install;

use panday_gateway::adapters::{anthropic::Anthropic, openai_compat::OpenAiCompat};
pub use panday_gateway::CollectUsage;
use panday_gateway::{Gateway, ProviderAdapter};
use panday_harness::ReplayOptions;
use panday_router::{ModelCatalog, PolicyRouter};
use panday_sdk::{ModelClient, PandayError};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StreamItem,
};
use std::sync::Arc;

pub const DEFAULT_POLICY: &str = include_str!("../../panday-router/policy/dev.yaml");

/// Where each provider's credentials and base URL come from.
///
/// Environment variables rather than a config file: Phase 0 needs exactly
/// three values, and a config format is a product decision (schema,
/// discovery, precedence) that deserves its own milestone rather than being
/// invented in passing.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub anthropic_key: Option<String>,
    pub compat_base_url: Option<String>,
    pub compat_key: Option<String>,
    pub local_base_url: String,
}

impl Config {
    pub const LOCAL_DEFAULT: &'static str = "http://127.0.0.1:8080";

    pub fn from_env() -> Self {
        Self {
            anthropic_key: non_empty("ANTHROPIC_API_KEY"),
            compat_base_url: non_empty("PANDAY_COMPAT_BASE_URL"),
            compat_key: non_empty("PANDAY_COMPAT_API_KEY"),
            local_base_url: non_empty("PANDAY_LOCAL_BASE_URL")
                .unwrap_or_else(|| Self::LOCAL_DEFAULT.to_string()),
        }
    }

    /// The `ModelRef` provider prefix each configured adapter answers to.
    pub fn build_gateway(&self, policy: &str, usage: Arc<CollectUsage>) -> Result<Gateway, String> {
        // The catalog is what turns the policy's pool patterns into models that exist, and it
        // carries the prices the per-call summary is priced with (M12.2). Shipped rather than
        // configurable for now: a deployment that needs different models edits one file, and a
        // deployment that needs different *prices* is doing billing, which is the platform's job.
        let catalog = ModelCatalog::shipped();
        let prices = catalog.price_table();
        let router = PolicyRouter::from_yaml(policy)
            .map_err(|e| format!("policy: {e}"))?
            .with_catalog(catalog);
        let mut b = Gateway::builder(Arc::new(router))
            .usage_sink(usage)
            .costs(Arc::new(prices));

        if let Some(key) = &self.anthropic_key {
            b = b.adapter(
                "anthropic",
                Arc::new(Anthropic::new(key.clone())) as Arc<dyn ProviderAdapter>,
            );
        }
        if let Some(base) = &self.compat_base_url {
            // Registered under `together` to match the dev policy's pools;
            // one adapter type, many bases (docs/11).
            b = b.adapter(
                "together",
                Arc::new(OpenAiCompat::new(base.clone(), self.compat_key.clone()))
                    as Arc<dyn ProviderAdapter>,
            );
        }
        // The local tier is always registered: it needs no credentials, and a
        // llama-server that is not running fails at connect with a clear
        // error rather than being invisible here.
        b = b.adapter(
            "local",
            Arc::new(OpenAiCompat::local(self.local_base_url.clone())) as Arc<dyn ProviderAdapter>,
        );

        Ok(b.build())
    }

    /// Human-readable summary of what is reachable, for `--help` and errors.
    pub fn describe(&self) -> String {
        let mut lines = vec![format!("  local        {}", self.local_base_url)];
        lines.push(match &self.anthropic_key {
            Some(_) => "  anthropic    ANTHROPIC_API_KEY set".into(),
            None => "  anthropic    (unset: ANTHROPIC_API_KEY)".to_string(),
        });
        lines.push(match &self.compat_base_url {
            Some(b) => format!("  together     {b}"),
            None => "  together     (unset: PANDAY_COMPAT_BASE_URL)".to_string(),
        });
        lines.join("\n")
    }
}

fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Parsed command line.
#[derive(Debug, PartialEq)]
pub enum Command {
    Chat {
        model: ModelRef,
        prompt: String,
    },
    /// docs/21 §The replay tool. Specified as `panday replay <session_id>`,
    /// which needs a store to look the session up in; Postgres is M3.5 and
    /// SQLite M18.1, so v1 takes the log file directly (`JsonlStore`). The
    /// spec is amended to match rather than the divergence buried here.
    Replay {
        log: String,
        at_seq: Option<u64>,
        costs: bool,
        verbose: bool,
        diff_against: Option<String>,
        summary: bool,
    },
    /// docs/10 M10.3: "used by panday-cli (dogfood — the CLI has no private
    /// APIs)". This subcommand talks to a `panday-harnessd` through
    /// `panday_sdk::sessions` and nothing else — if the public client cannot do
    /// something, neither can we.
    Session {
        url: String,
        prompt: String,
        /// Join an existing session instead of creating one.
        session: Option<String>,
        /// Resume from a `seq` — proves the resume path from a shell.
        after_seq: Option<u64>,
    },
    /// docs/16 §ACP bridge, ADR-012: the server thirteen editors can drive.
    Acp {
        /// Workspace root the session's tools are scoped to.
        workspace: std::path::PathBuf,
        profile: String,
    },
    /// `panday plugin install <name>@<version>` (M16.6).
    PluginInstall {
        spec: String,
        registry_url: String,
        trust: Option<String>,
        dir: Option<std::path::PathBuf>,
        yes: bool,
    },
    Help,
    Version,
}

/// Hand-rolled because it parses one subcommand and two flags.
///
/// `clap` is not in the docs/02 dependency table, and adding it is a real
/// decision to make when the CLI grows a surface worth it (TUI, ACP, replay),
/// not a thing to slip in for `chat`.
pub fn parse_args<I, S>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    let mut it = args.iter();

    let Some(sub) = it.next() else {
        return Ok(Command::Help);
    };

    match sub.as_str() {
        "help" | "--help" | "-h" => return Ok(Command::Help),
        "version" | "--version" | "-V" => return Ok(Command::Version),
        "chat" => {}
        "replay" => return parse_replay(it),
        "session" => return parse_session(it),
        "acp" => return parse_acp(it),
        "plugin" => return parse_plugin(it),
        other => return Err(format!("unknown command `{other}` (try `panday help`)")),
    }

    let mut model = ModelRef::auto();
    let mut words: Vec<String> = Vec::new();

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--model" | "-m" => {
                let v = it.next().ok_or_else(|| {
                    "--model needs a value, e.g. --model local/qwen3.5-4b".to_string()
                })?;
                model = ModelRef(v.clone());
            }
            "--help" | "-h" => return Ok(Command::Help),
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}` (try `panday help`)"))
            }
            other => words.push(other.to_string()),
        }
    }

    Ok(Command::Chat {
        model,
        prompt: words.join(" "),
    })
}

fn parse_replay<'a, I: Iterator<Item = &'a String>>(mut it: I) -> Result<Command, String> {
    let mut log: Option<String> = None;
    let mut at_seq = None;
    let mut costs = false;
    let mut verbose = false;
    let mut diff_against = None;
    let mut summary = false;

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--at" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--at needs a seq, e.g. --at 42".to_string())?;
                at_seq = Some(
                    v.parse::<u64>()
                        .map_err(|_| format!("--at wants a seq number, got `{v}`"))?,
                );
            }
            "--diff" => {
                diff_against = Some(
                    it.next()
                        .ok_or_else(|| "--diff needs a second log to compare against".to_string())?
                        .clone(),
                );
            }
            "--costs" => costs = true,
            "--verbose" | "-v" => verbose = true,
            "--summary" => summary = true,
            "--help" | "-h" => return Ok(Command::Help),
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}` (try `panday help`)"))
            }
            other if log.is_none() => log = Some(other.to_string()),
            other => return Err(format!("replay takes one log, also got `{other}`")),
        }
    }

    Ok(Command::Replay {
        log: log.ok_or_else(|| {
            "replay needs a log file, e.g. `panday replay ./session.jsonl`".to_string()
        })?,
        at_seq,
        costs,
        verbose,
        diff_against,
        summary,
    })
}

fn parse_plugin<'a, I: Iterator<Item = &'a String>>(mut it: I) -> Result<Command, String> {
    match it.next().map(String::as_str) {
        Some("install") => {}
        Some(other) => {
            return Err(format!(
                "unknown plugin subcommand `{other}`; only `install` exists so far"
            ))
        }
        None => return Err("`panday plugin install <name>@<version>`".into()),
    }

    let mut spec = None;
    let mut registry_url = std::env::var("PANDAY_REGISTRY_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8084".to_string());
    let mut trust = None;
    let mut dir = None;
    let mut yes = false;

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--registry" => {
                registry_url = it
                    .next()
                    .ok_or_else(|| "--registry needs a URL".to_string())?
                    .clone()
            }
            "--trust" => {
                trust = Some(
                    it.next()
                        .ok_or_else(|| "--trust needs a hex public key".to_string())?
                        .clone(),
                )
            }
            "--dir" => {
                dir = Some(
                    it.next()
                        .ok_or_else(|| "--dir needs a path".to_string())?
                        .into(),
                )
            }
            "--yes" | "-y" => yes = true,
            "--help" | "-h" => return Ok(Command::Help),
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}` (try `panday help`)"))
            }
            other if spec.is_none() => spec = Some(other.to_string()),
            other => return Err(format!("install takes one plugin, also got `{other}`")),
        }
    }

    Ok(Command::PluginInstall {
        spec: spec
            .ok_or_else(|| "which plugin? `panday plugin install linty@0.1.0`".to_string())?,
        registry_url,
        trust,
        dir,
        yes,
    })
}

fn parse_acp<'a, I: Iterator<Item = &'a String>>(mut it: I) -> Result<Command, String> {
    let mut workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
    // `dev` rather than `unleashed`: an editor session has a human in it, and the
    // point of the gate is that they see the question (docs/13 §profiles).
    let mut profile = "dev".to_string();

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--workspace" => {
                workspace = it
                    .next()
                    .ok_or_else(|| "--workspace needs a path".to_string())?
                    .into()
            }
            "--profile" => {
                profile = it
                    .next()
                    .ok_or_else(|| "--profile needs one of read_only|dev|unleashed".to_string())?
                    .clone()
            }
            "--help" | "-h" => return Ok(Command::Help),
            other => return Err(format!("unknown flag `{other}` (try `panday help`)")),
        }
    }
    if !["read_only", "dev", "unleashed"].contains(&profile.as_str()) {
        return Err(format!(
            "unknown profile `{profile}`; expected read_only, dev or unleashed"
        ));
    }
    Ok(Command::Acp { workspace, profile })
}

fn parse_session<'a, I: Iterator<Item = &'a String>>(mut it: I) -> Result<Command, String> {
    let mut url = "http://127.0.0.1:8082".to_string();
    let mut session = None;
    let mut after_seq = None;
    let mut words: Vec<String> = Vec::new();

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--url" => {
                url = it
                    .next()
                    .ok_or_else(|| "--url needs a base URL".to_string())?
                    .clone()
            }
            "--session" => {
                session = Some(
                    it.next()
                        .ok_or_else(|| "--session needs a session id".to_string())?
                        .clone(),
                )
            }
            "--after-seq" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--after-seq needs a seq".to_string())?;
                after_seq = Some(
                    v.parse::<u64>()
                        .map_err(|_| format!("--after-seq wants a number, got `{v}`"))?,
                );
            }
            "--help" | "-h" => return Ok(Command::Help),
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}` (try `panday help`)"))
            }
            other => words.push(other.to_string()),
        }
    }
    if after_seq.is_some() && session.is_none() {
        // Resuming into a session that was just created has nothing to resume.
        return Err("--after-seq needs --session: a resume point belongs to a session".into());
    }
    Ok(Command::Session {
        url,
        prompt: words.join(" "),
        session,
        after_seq,
    })
}

/// Run a session through the public SDK client (M10.3).
///
/// Events are rendered with the *same* renderer `panday replay` uses
/// (`panday_harness::replay::Renderer`), so a live session and its replay look
/// identical — which is the property that makes a replay trustworthy rather than a
/// second rendering of the same facts.
pub async fn run_session(cmd: &Command, out: &mut dyn Output) -> Result<u64, String> {
    let Command::Session {
        url,
        prompt,
        session,
        after_seq,
    } = cmd
    else {
        return Err("not a session".into());
    };

    let client = panday_sdk::sessions::SessionsClient::new(
        url.clone(),
        std::env::var("PANDAY_API_KEY").ok(),
    );
    let id = match session {
        Some(raw) => panday_types::SessionId(
            uuid::Uuid::parse_str(raw).map_err(|e| format!("session id: {e}"))?,
        ),
        None => client.create().await.map_err(|e| e.to_string())?,
    };
    out.line(&format!("session {id}", id = id.0));

    let after = match after_seq {
        Some(n) => panday_sdk::sessions::After::Seq(*n),
        None => panday_sdk::sessions::After::Beginning,
    };
    let mut stream = client
        .subscribe(id, after)
        .await
        .map_err(|e| e.to_string())?;

    if !prompt.trim().is_empty() {
        stream
            .send(panday_sdk::sessions::ClientMessage::text(prompt.clone()))
            .await
            .map_err(|e| e.to_string())?;
    }

    let mut renderer = panday_harness::replay::Renderer::new(ReplayOptions {
        costs: true,
        ..Default::default()
    });
    while let Some(item) = stream.next_event().await {
        let envelope = item.map_err(|e| e.to_string())?;
        let text = renderer.push(&envelope);
        if !text.is_empty() {
            out.text(&text);
        }
        // One turn per invocation: the CLI is a shell command, not a chat UI. The
        // resume point is printed so the next invocation can pick it up, which is
        // how `--after-seq` gets used for real.
        if matches!(
            envelope.event,
            panday_types::event::Event::TurnFinished { .. }
        ) {
            break;
        }
    }
    out.line(&format!(
        "\nresume with: --session {} --after-seq {}",
        id.0,
        stream.resume_point()
    ));
    Ok(stream.resume_point())
}

/// Render a replay, or a diff between two of them.
///
/// docs/21's stated use for `--diff` is "before/after a reducer change", so the
/// comparison is between two *renderings*: that is the artifact a person reads,
/// and it is what changes when a reducer changes.
pub fn run_replay(cmd: &Command) -> Result<String, String> {
    let Command::Replay {
        log,
        at_seq,
        costs,
        verbose,
        diff_against,
        summary,
    } = cmd
    else {
        return Err("not a replay".into());
    };

    let opts = ReplayOptions {
        at_seq: *at_seq,
        costs: *costs,
        verbose: *verbose,
    };
    let events = panday_harness::read_log(log).map_err(|e| e.to_string())?;

    if let Some(other) = diff_against {
        let theirs = panday_harness::read_log(other).map_err(|e| e.to_string())?;
        return Ok(panday_harness::replay::diff(
            &panday_harness::render(&events, opts),
            &panday_harness::render(&theirs, opts),
        ));
    }
    if *summary {
        return Ok(panday_harness::replay::summarize(&events));
    }
    Ok(panday_harness::render(&events, opts))
}

pub fn help() -> String {
    format!(
        "panday — a Rust AI platform\n\n\
         USAGE:\n  \
         panday chat [--model <provider/model>] <prompt>\n  \
         panday replay <log.jsonl> [--at <seq>] [--costs] [--verbose] [--summary] [--diff <other.jsonl>]\n  \
         panday session [--url <base>] [--session <id>] [--after-seq <n>] <prompt>\n  \
         panday acp [--workspace <dir>] [--profile <name>]   (an editor spawns this)\n  \
         panday plugin install <name>@<version> [--registry <url>] [--trust <key>] [--yes]\n\n\
         FLAGS:\n  \
         -m, --model    a concrete `provider/model`, or `auto` to let the router decide (default)\n  \
         -h, --help     show this\n\n\
         SESSION FLAGS:\n  \
         --url          a panday-harnessd base URL (default http://127.0.0.1:8082)\n  \
         --session      join an existing session instead of creating one\n  \
         --after-seq    resume that session from this seq\n\n\
         REPLAY FLAGS:\n  \
         --at <seq>     render the session as it stood at that seq (time travel)\n  \
         --costs        per-turn usage and dollar overlay\n  \
         --verbose      full tool arguments and observations, unelided\n  \
         --summary      one line: turns, tools, tokens, how it ended\n  \
         --diff <log>   diff this replay against another log's (e.g. before/after a reducer change)\n\n\
         ENVIRONMENT:\n  \
         ANTHROPIC_API_KEY        enables the `anthropic` provider\n  \
         PANDAY_COMPAT_BASE_URL   enables the `together` provider (any OpenAI-compatible base)\n  \
         PANDAY_COMPAT_API_KEY    its key, if the base needs one\n  \
         PANDAY_LOCAL_BASE_URL    llama-server base (default {})\n\n\
         Every call goes through the gateway: routed by policy, metered per call.",
        Config::LOCAL_DEFAULT
    )
}

/// Build the one-shot request `chat` sends.
pub fn chat_request(model: ModelRef, prompt: &str) -> ChatRequest {
    ChatRequest {
        model,
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: prompt.to_string(),
            }],
            call_id: None,
            provider_call_id: None,
        }],
        tools: vec![],
        sampling: Sampling::default(),
        cache: Default::default(),
        stream: true,
        metadata: CallMeta {
            // Phase 0 has no accounts (M17.1); a per-run id keeps the usage
            // record shaped correctly so the ledger can adopt it unchanged.
            account: AccountId::new(),
            request: RequestId::new(),
            session: None,
            turn: None,
            task: None,
        },
    }
}

/// Anything the chat loop writes to. Abstracted so the loop is testable
/// without capturing a process's stdout.
pub trait Output {
    fn text(&mut self, s: &str);
    fn line(&mut self, s: &str);
}

pub struct Stdout;

impl Output for Stdout {
    fn text(&mut self, s: &str) {
        use std::io::Write;
        print!("{s}");
        // Streaming is the point: an unflushed delta is an invisible one.
        let _ = std::io::stdout().flush();
    }
    fn line(&mut self, s: &str) {
        println!("{s}");
    }
}

/// Stream one turn and report what it cost.
///
/// Returns the assistant text so callers (and tests) can assert on it.
pub async fn run_chat(
    gateway: &Gateway,
    usage: &CollectUsage,
    req: ChatRequest,
    out: &mut dyn Output,
) -> Result<String, PandayError> {
    use futures_util::StreamExt;

    let mut stream = gateway.chat(req).await?;
    let mut text = String::new();

    while let Some(item) = stream.next().await {
        match item? {
            StreamItem::Delta { text: t } => {
                out.text(&t);
                text.push_str(&t);
            }
            StreamItem::ToolCallStart { name, .. } => {
                out.line(&format!("\n[tool: {name}]"));
            }
            // Usage is reported from the sink below, not from the raw frame:
            // the gateway is the component that owns metering (ADR-006), and
            // reading the frame here would bypass it.
            StreamItem::Usage { .. } | StreamItem::ToolCallDelta { .. } => {}
            StreamItem::Done { reason } => {
                out.line(&format!("\n\n[{reason:?}]"));
            }
        }
    }

    for r in usage.take() {
        out.line(&format!(
            "[usage] {} via {} — in {} (cache read {}, write {}) · out {}",
            r.model.0,
            r.provider,
            r.usage.input_tokens,
            r.usage.cache_read_tokens,
            r.usage.cache_write_tokens + r.usage.cache_write_1h_tokens,
            r.usage.output_tokens,
        ));
    }

    Ok(text)
}

/// `panday acp` — serve the ACP bridge on stdio (M16.5).
///
/// The gateway runs in-process for the same reason `chat` does (ADR-012, docs/01's
/// dev/solo composition): an editor session on a laptop should not require a hosted
/// service, and the harness is a library precisely so it can run either way.
pub async fn run_acp(workspace: std::path::PathBuf, profile: &str) -> Result<(), String> {
    use panday_harness::native::{register_native, Workspace};
    use panday_sandbox::{
        FsPolicy, Limits, NetPolicy, Sandbox, SandboxPolicy, SandboxTier, SessionSpec,
    };

    let workspace = workspace
        .canonicalize()
        .map_err(|e| format!("workspace {}: {e}", workspace.display()))?;

    let config = Config::from_env();
    let usage = std::sync::Arc::new(CollectUsage::new());
    let gateway = std::sync::Arc::new(config.build_gateway(DEFAULT_POLICY, usage)?);

    // One sandbox per process, one handle per workspace: an editor opens sessions in the
    // project it was started in, and a second workspace means a second `panday acp`.
    #[cfg(target_os = "macos")]
    let sandbox = std::sync::Arc::new(panday_sandbox::T2MacosSandbox::new());
    #[cfg(target_os = "linux")]
    let sandbox = std::sync::Arc::new(panday_sandbox::T2LinuxSandbox::new());
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err("no T2 sandbox on this platform; `panday acp` needs one".into());

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let handle = sandbox
            .create(SessionSpec {
                tier: SandboxTier::T2OsJail,
                policy: SandboxPolicy {
                    fs: FsPolicy {
                        workspace_rw: workspace.clone(),
                        staged_ro: vec![],
                    },
                    net: NetPolicy::default(),
                    limits: Limits {
                        wall_clock_ms: 180_000,
                        ..Default::default()
                    },
                    // Inherited toolchain paths only — an editor's session runs the
                    // project's own build, and a jail with no PATH cannot.
                    env: toolchain_env(),
                },
            })
            .await
            .map_err(|e| format!("sandbox: {e}"))?;

        let ws = Workspace::new(
            sandbox.clone() as std::sync::Arc<dyn Sandbox>,
            handle,
            workspace,
        );

        let profile = match profile {
            "read_only" => panday_harness::Profile::ReadOnly,
            "unleashed" => panday_harness::Profile::Unleashed,
            _ => panday_harness::Profile::Dev,
        };

        acp_server::serve_stdio(acp_server::AcpDeps {
            model: gateway,
            model_ref: ModelRef::auto(),
            tools: std::sync::Arc::new(move || {
                let mut registry = panday_harness::tools::ToolRegistry::default();
                register_native(&mut registry, ws.clone());
                registry
            }),
            profile,
            account: AccountId::new(),
        })
        .await
    }
}

/// The few environment variables a project's build actually needs inside the jail.
///
/// Not the parent environment: docs/20 T4 forbids inheriting it, and a jail that
/// inherits `AWS_SECRET_ACCESS_KEY` because the developer happened to export it is the
/// exact leak the tier exists to prevent.
fn toolchain_env() -> Vec<(String, String)> {
    ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_install_needs_a_pinned_version() {
        // `install linty` would mean "whatever is newest", which is a different plugin
        // tomorrow — and consent given today would cover code nobody has seen.
        let cmd = parse_args(["plugin", "install", "linty@0.1.0", "--yes"]).unwrap();
        match cmd {
            Command::PluginInstall { spec, yes, .. } => {
                assert_eq!(spec, "linty@0.1.0");
                assert!(yes);
            }
            other => panic!("{other:?}"),
        }
        let err = parse_args(["plugin", "install"]).unwrap_err();
        assert!(err.contains("which plugin"), "{err}");
    }

    #[test]
    fn an_unknown_plugin_subcommand_says_what_exists() {
        let err = parse_args(["plugin", "publish", "x"]).unwrap_err();
        assert!(err.contains("only `install` exists"), "{err}");
    }

    #[test]
    fn help_documents_plugin_install() {
        assert!(help().contains("panday plugin install"), "{}", help());
    }

    #[test]
    fn parses_acp_with_a_workspace_and_profile() {
        let cmd =
            parse_args(["acp", "--workspace", "/work/repo", "--profile", "read_only"]).unwrap();
        assert_eq!(
            cmd,
            Command::Acp {
                workspace: std::path::PathBuf::from("/work/repo"),
                profile: "read_only".into(),
            }
        );
    }

    #[test]
    fn acp_defaults_to_dev_not_unleashed() {
        // An editor session has a human in it, and the point of the gate is that they
        // see the question (docs/13 §profiles).
        match parse_args(["acp"]).unwrap() {
            Command::Acp { profile, .. } => assert_eq!(profile, "dev"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unknown_profile_is_rejected() {
        let err = parse_args(["acp", "--profile", "yolo"]).unwrap_err();
        assert!(err.contains("unknown profile"), "{err}");
    }

    #[test]
    fn the_toolchain_env_never_forwards_a_secret() {
        // docs/20 T4: a jail that inherits `AWS_SECRET_ACCESS_KEY` because the developer
        // exported it is the leak the tier exists to prevent.
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "should-not-be-forwarded");
        std::env::set_var("ANTHROPIC_API_KEY", "sk-should-not-be-forwarded");
        let env = toolchain_env();
        assert!(
            env.iter().all(
                |(k, _)| ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "LANG"]
                    .contains(&k.as_str())
            ),
            "{env:?}"
        );
        assert!(
            !format!("{env:?}").contains("should-not-be-forwarded"),
            "{env:?}"
        );
    }

    #[test]
    fn parses_a_session_with_a_resume_point() {
        let cmd = parse_args([
            "session",
            "--url",
            "http://h:8082",
            "--session",
            "01930000-0000-7000-8000-000000000001",
            "--after-seq",
            "7",
            "fix",
            "it",
        ])
        .unwrap();
        assert_eq!(
            cmd,
            Command::Session {
                url: "http://h:8082".into(),
                prompt: "fix it".into(),
                session: Some("01930000-0000-7000-8000-000000000001".into()),
                after_seq: Some(7),
            }
        );
    }

    #[test]
    fn a_resume_point_without_a_session_is_rejected() {
        // Resuming a session that is about to be created has nothing to resume, and
        // silently ignoring the flag would look like it worked.
        let err = parse_args(["session", "--after-seq", "3", "hi"]).unwrap_err();
        assert!(err.contains("needs --session"), "{err}");
    }

    #[test]
    fn help_documents_session() {
        let h = help();
        assert!(h.contains("panday session"), "{h}");
        assert!(h.contains("--after-seq"), "{h}");
    }

    #[test]
    fn parses_a_replay_with_flags() {
        let cmd =
            parse_args(["replay", "./s.jsonl", "--at", "42", "--costs", "--verbose"]).unwrap();
        assert_eq!(
            cmd,
            Command::Replay {
                log: "./s.jsonl".into(),
                at_seq: Some(42),
                costs: true,
                verbose: true,
                diff_against: None,
                summary: false,
            }
        );
    }

    #[test]
    fn replay_needs_a_log_and_says_so() {
        let err = parse_args(["replay"]).unwrap_err();
        assert!(err.contains("log file"), "{err}");
    }

    #[test]
    fn a_non_numeric_at_is_rejected_rather_than_ignored() {
        // Silently treating `--at head` as "no cut" would render the whole
        // session and look like it worked.
        let err = parse_args(["replay", "s.jsonl", "--at", "head"]).unwrap_err();
        assert!(err.contains("seq number"), "{err}");
    }

    #[test]
    fn diff_takes_a_second_log() {
        let cmd = parse_args(["replay", "a.jsonl", "--diff", "b.jsonl"]).unwrap();
        match cmd {
            Command::Replay { diff_against, .. } => {
                assert_eq!(diff_against.as_deref(), Some("b.jsonl"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn help_documents_replay() {
        // A subcommand missing from `--help` is a subcommand nobody finds.
        let h = help();
        assert!(h.contains("panday replay"), "{h}");
        assert!(h.contains("--at"), "{h}");
        assert!(h.contains("--costs"), "{h}");
    }

    #[test]
    fn replay_renders_a_log_from_disk() {
        let path =
            std::env::temp_dir().join(format!("panday-cli-replay-{}.jsonl", std::process::id()));
        std::fs::write(
            &path,
            concat!(
                r#"{"v":1,"session_id":"01930000-0000-7000-8000-000000000001","seq":1,"at":"2026-01-15T12:00:00Z","event":"user_message","source":"cli","content":[{"type":"text","text":"hello there"}]}"#,
                "\n",
            ),
        )
        .unwrap();

        let cmd = parse_args(["replay", path.to_str().unwrap()]).unwrap();
        let text = run_replay(&cmd).unwrap();
        assert!(text.contains("hello there"), "{text}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replay_renders_a_log_containing_an_event_from_a_newer_version() {
        // M3.4: unknown-event tolerance, in the CLI specifically. A client that
        // failed here would break the moment the server shipped ahead of it —
        // and `panday replay` is the tool someone reaches for at exactly that
        // moment.
        let path =
            std::env::temp_dir().join(format!("panday-cli-unknown-{}.jsonl", std::process::id()));
        std::fs::write(
            &path,
            concat!(
                r#"{"v":1,"session_id":"01930000-0000-7000-8000-000000000001","seq":1,"at":"2026-01-15T12:00:00Z","event":"user_message","source":"cli","content":[{"type":"text","text":"hello"}]}"#,
                "\n",
                r#"{"v":1,"session_id":"01930000-0000-7000-8000-000000000001","seq":2,"at":"2026-01-15T12:00:01Z","event":"plan_updated","steps":["a"]}"#,
                "\n",
            ),
        )
        .unwrap();

        let text = run_replay(&parse_args(["replay", path.to_str().unwrap()]).unwrap()).unwrap();
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("unknown event"), "{text}");
        assert!(text.contains("plan_updated"), "the kind is named: {text}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_log_is_an_error_not_an_empty_replay() {
        let cmd = parse_args(["replay", "/nonexistent/nope.jsonl"]).unwrap();
        let err = run_replay(&cmd).unwrap_err();
        assert!(err.contains("nope.jsonl"), "{err}");
    }

    #[test]
    fn parses_a_chat_with_a_multi_word_prompt() {
        let cmd = parse_args(["chat", "fix", "the", "failing", "test"]).unwrap();
        assert_eq!(
            cmd,
            Command::Chat {
                model: ModelRef::auto(),
                prompt: "fix the failing test".into()
            }
        );
    }

    #[test]
    fn model_flag_pins_the_target() {
        let cmd = parse_args(["chat", "--model", "local/qwen3.5-4b", "hi"]).unwrap();
        match cmd {
            Command::Chat { model, prompt } => {
                assert_eq!(model.0, "local/qwen3.5-4b");
                assert_eq!(prompt, "hi");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn model_flag_without_a_value_is_an_error_not_a_silent_auto() {
        let err = parse_args(["chat", "--model"]).unwrap_err();
        assert!(err.contains("--model needs a value"), "{err}");
    }

    #[test]
    fn unknown_commands_and_flags_are_rejected() {
        assert!(parse_args(["frobnicate"]).is_err());
        assert!(parse_args(["chat", "--wat"]).is_err());
    }

    #[test]
    fn bare_invocation_shows_help() {
        assert_eq!(parse_args(Vec::<String>::new()).unwrap(), Command::Help);
        assert_eq!(parse_args(["help"]).unwrap(), Command::Help);
    }

    #[test]
    fn a_prompt_that_looks_like_a_flag_value_is_kept() {
        // "-" alone is a word, not a flag.
        let cmd = parse_args(["chat", "--model", "auto", "why", "did", "it", "fail"]).unwrap();
        match cmd {
            Command::Chat { prompt, .. } => assert_eq!(prompt, "why did it fail"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn help_names_every_environment_variable_the_config_reads() {
        // Drift here strands users with a provider that silently never loads.
        let h = help();
        for key in [
            "ANTHROPIC_API_KEY",
            "PANDAY_COMPAT_BASE_URL",
            "PANDAY_COMPAT_API_KEY",
            "PANDAY_LOCAL_BASE_URL",
        ] {
            assert!(h.contains(key), "help does not mention {key}");
        }
    }

    #[test]
    fn the_local_tier_is_always_available_without_credentials() {
        let cfg = Config {
            local_base_url: Config::LOCAL_DEFAULT.into(),
            ..Default::default()
        };
        let g = cfg
            .build_gateway(DEFAULT_POLICY, Arc::new(CollectUsage::new()))
            .expect("must build with no keys at all");
        assert_eq!(g.providers(), vec!["local"]);
    }

    #[test]
    fn configured_keys_register_their_providers() {
        let cfg = Config {
            anthropic_key: Some("sk-test".into()),
            compat_base_url: Some("https://api.together.xyz".into()),
            compat_key: Some("k".into()),
            local_base_url: Config::LOCAL_DEFAULT.into(),
        };
        let g = cfg
            .build_gateway(DEFAULT_POLICY, Arc::new(CollectUsage::new()))
            .unwrap();
        let mut p = g.providers();
        p.sort();
        assert_eq!(p, vec!["anthropic", "local", "together"]);
    }

    #[test]
    fn the_bundled_policy_is_valid() {
        // Shipping a CLI whose default policy does not parse is a broken build.
        assert!(panday_router::Policy::from_yaml(DEFAULT_POLICY).is_ok());
    }

    #[test]
    fn describe_flags_unset_providers_rather_than_hiding_them() {
        let d = Config {
            local_base_url: Config::LOCAL_DEFAULT.into(),
            ..Default::default()
        }
        .describe();
        assert!(d.contains("ANTHROPIC_API_KEY"), "{d}");
        assert!(d.contains("PANDAY_COMPAT_BASE_URL"), "{d}");
    }
}
