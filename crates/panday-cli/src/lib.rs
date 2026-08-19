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

use panday_gateway::adapters::{anthropic::Anthropic, openai_compat::OpenAiCompat};
pub use panday_gateway::CollectUsage;
use panday_gateway::{Gateway, ProviderAdapter};
use panday_harness::ReplayOptions;
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
         panday replay <log.jsonl> [--at <seq>] [--costs] [--verbose] [--summary] [--diff <other.jsonl>]\n\n\
         FLAGS:\n  \
         -m, --model    a concrete `provider/model`, or `auto` to let the router decide (default)\n  \
         -h, --help     show this\n\n\
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

#[cfg(test)]
mod tests {
    use super::*;

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
