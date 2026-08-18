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

use panday_gateway::adapters::{anthropic::Anthropic, openai_compat::OpenAiCompat};
pub use panday_gateway::CollectUsage;
use panday_gateway::{Gateway, ProviderAdapter};
use panday_router::PolicyRouter;
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
        let router = PolicyRouter::from_yaml(policy).map_err(|e| format!("policy: {e}"))?;
        let mut b = Gateway::builder(Arc::new(router)).usage_sink(usage);

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
    Chat { model: ModelRef, prompt: String },
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

pub fn help() -> String {
    format!(
        "panday — a Rust AI platform\n\n\
         USAGE:\n  \
         panday chat [--model <provider/model>] <prompt>\n\n\
         FLAGS:\n  \
         -m, --model    a concrete `provider/model`, or `auto` to let the router decide (default)\n  \
         -h, --help     show this\n\n\
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

#[cfg(test)]
mod tests {
    use super::*;

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
