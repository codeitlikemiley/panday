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
            other if other.starts_with('-') => {
                return fail(&format!("unknown flag `{other}`"));
            }
            other => prompt.push(other.to_string()),
        }
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
     --log        where the event log goes (default <workspace>/.panday/session.jsonl)\n\n\
     Loopback only: a remote base URL is refused, because the offline tier's promise is\n\
     that nothing leaves the machine."
        .to_string()
}
