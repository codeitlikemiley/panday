//! `panday local` — one binary, zero egress (docs/18, ADR-010, M18.1).
//!
//! ```text
//! panday-local [--base-url http://127.0.0.1:8080] [--model local/qwen3.5-4b]
//!              [--workspace .] [--profile dev] <prompt>
//! ```
//!
//! Argument parsing is hand-rolled for the same reason `panday`'s is (docs/02: `clap` is
//! not in the dependency table): this is four flags and a prompt.

use panday_local::{Local, LocalConfig};
use std::process::ExitCode;

/// The port `panday local` was told to talk to, so a supervised server is started on the port the
/// gateway will actually call rather than on a default that happens to match.
fn port_of(base_url: &str) -> Option<u16> {
    base_url
        .rsplit(':')
        .next()?
        .trim_end_matches('/')
        .split('/')
        .next()?
        .parse()
        .ok()
}

fn main() -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("panday-local: cannot start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(run())
}

async fn run() -> ExitCode {
    if let Err(e) = panday_sdk::telemetry::init("panday-local") {
        eprintln!("panday-local: {e}");
        return ExitCode::FAILURE;
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    }

    let mut config = LocalConfig::new(std::env::current_dir().unwrap_or_else(|_| ".".into()));
    let mut prompt: Vec<String> = Vec::new();
    let mut sync_url: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut next = || it.next().cloned();
        match arg.as_str() {
            "--base-url" => match next() {
                Some(v) => config = config.base_url(v),
                None => return fail("--base-url needs a URL"),
            },
            "--model" => match next() {
                Some(v) => config = config.model(v),
                None => return fail("--model needs a `local/...` id"),
            },
            "--workspace" => match next() {
                Some(v) => {
                    config = LocalConfig::new(v)
                        .base_url(config.base_url.clone())
                        .model(config.model.0.clone())
                        .profile(config.profile)
                }
                None => return fail("--workspace needs a path"),
            },
            "--profile" => match next() {
                Some(v) => {
                    config = config.profile(match v.as_str() {
                        "read_only" => panday_harness::Profile::ReadOnly,
                        "unleashed" => panday_harness::Profile::Unleashed,
                        "dev" => panday_harness::Profile::Dev,
                        other => return fail(&format!("unknown profile `{other}`")),
                    })
                }
                None => return fail("--profile needs read_only|dev|unleashed"),
            },
            "--log" => match next() {
                Some(v) => config = config.log(v),
                None => return fail("--log needs a path"),
            },
            // M18.6. Sync and exit: a session that is finished is the thing worth pushing, and
            // mixing "run a turn" with "upload" in one invocation would make a failed upload look
            // like a failed turn.
            "--sync" => match next() {
                Some(url) => sync_url = Some(url),
                None => return fail("--sync needs a base URL, e.g. https://api.panday.dev"),
            },
            // M17.6. Two flags rather than a baked-in key, for the same reason the model catalog
            // has none: we have published no key, and a placeholder would teach people to trust
            // one nobody holds.
            "--entitlement" => match next() {
                Some(path) => {
                    let key = std::env::var("PANDAY_ENTITLEMENT_KEY").unwrap_or_default();
                    if key.trim().is_empty() {
                        return fail("--entitlement needs PANDAY_ENTITLEMENT_KEY (hex public key)");
                    }
                    config.entitlement = Some((std::path::PathBuf::from(path), key));
                }
                None => return fail("--entitlement needs a path to the licence file"),
            },
            // M18.2: start and supervise the inference server ourselves. Without this, `panday
            // local` attaches to whatever is already listening — which is the right default,
            // because killing a server the user started would be a surprise.
            "--serve" => match next() {
                Some(gguf) => {
                    let port = port_of(&config.base_url).unwrap_or(8081);
                    let mut server = panday_local::supervisor::ServerConfig::llama_server(
                        std::path::Path::new(&gguf),
                        port,
                    );
                    if let Ok(binary) = std::env::var("PANDAY_LOCAL_SERVER_BIN") {
                        if !binary.trim().is_empty() {
                            server.binary = binary;
                        }
                    }
                    config.serve = Some(server);
                }
                None => return fail("--serve needs a path to a .gguf"),
            },
            other if other.starts_with('-') => {
                return fail(&format!("unknown flag `{other}`"));
            }
            other => prompt.push(other.to_string()),
        }
    }

    if let Some(url) = sync_url {
        let Ok(key) = std::env::var("PANDAY_API_KEY") else {
            return fail("--sync needs PANDAY_API_KEY (a `sessions`-scoped key from the platform)");
        };
        return match panday_local::sync_log(&config.log, &url, &key).await {
            Ok(report) => {
                println!(
                    "session {} synced — {} new events, {} already there, {} ledger entr{} \
                     ({} in / {} out tokens)",
                    report.session_id,
                    report.stored,
                    report.already_present,
                    report.ledger_entries,
                    if report.ledger_entries == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                    report.input_tokens,
                    report.output_tokens
                );
                // Said out loud, because a log the server could not interpret syncs
                // "successfully" and reconciles to nothing.
                if report.unknown_events > 0 {
                    println!(
                        "note: {} event(s) were stored but not understood by the server — \
                         they are preserved, but they counted for nothing",
                        report.unknown_events
                    );
                }
                ExitCode::SUCCESS
            }
            // Retrying is running the command again: the push is idempotent, so there is nothing to
            // clean up and no partial state to reason about.
            Err(e) => fail(&format!(
                "{e}\n\nThe push is idempotent — run it again when the \
                                     connection is back."
            )),
        };
    }

    if prompt.is_empty() {
        return fail("nothing to do — give a prompt");
    }

    let mut local = match Local::boot(config).await {
        Ok(l) => l,
        Err(e) => return fail(&e.to_string()),
    };
    println!(
        "session {} · log {}",
        local.session().0,
        local.log_path().display()
    );
    // Printed every run when there is anything to say. A licence warning that appears once, on the
    // day it expires, is a licence warning nobody sees.
    if let Some(line) = local.licence_line() {
        println!("{line}");
    }

    match local.turn(&prompt.join(" ")).await {
        Ok(rendered) => {
            print!("{rendered}");
            for record in local.usage() {
                // Real token counts with no money: docs/18 meters offline usage so it can
                // sync later, and a free tier that reports nothing is a free tier nobody
                // can reason about.
                println!(
                    "[usage] {} — in {} (cache read {}) · out {} · $0.00 (local)",
                    record.model.0,
                    record.usage.input_tokens,
                    record.usage.cache_read_tokens,
                    record.usage.output_tokens
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e.to_string()),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("panday-local: {message}\n\n{}", usage());
    ExitCode::FAILURE
}

fn usage() -> String {
    "panday local — the offline tier (docs/18)\n\n\
     USAGE:\n  \
     panday-local [flags] <prompt>\n\n\
     FLAGS:\n  \
     --base-url   an OpenAI-compatible server on loopback (default http://127.0.0.1:8080)\n  \
     --model      a `local/...` model id\n  \
     --workspace  the directory tools are scoped to (default: cwd)\n  \
     --profile    read_only | dev | unleashed (default dev)\n  \
     --log        where the event log goes (default <workspace>/.panday/session.jsonl)\n  \
     --sync <url>   push this session's log to a cloud account and exit; needs $PANDAY_API_KEY\n  \
     --entitlement  an offline licence file (with .sig beside it); needs\n                  \
     $PANDAY_ENTITLEMENT_KEY. Absent = community tier, which is a complete product\n  \
     --serve      start and supervise llama-server on this .gguf, instead of attaching to a\n               \
     running one (binary from $PANDAY_LOCAL_SERVER_BIN, default `llama-server`)\n\n\
     Loopback only: a remote base URL is refused, because the offline tier's promise is\n\
     that nothing leaves the machine."
        .to_string()
}
