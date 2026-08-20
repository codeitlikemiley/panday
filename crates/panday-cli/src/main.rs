//! Thin entrypoint (docs/02: "lib.rs + thin main.rs so every service is
//! testable in-process"). All behaviour lives in the library.

use panday_cli::{chat_request, help, parse_args, CollectUsage, Command, Config, Stdout};
use std::process::ExitCode;
use std::sync::Arc;

fn main() -> ExitCode {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("panday: cannot start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(run())
}

async fn run() -> ExitCode {
    // docs/21: JSON tracing to stdout, OTLP when an endpoint is configured.
    // A failure here is fatal on purpose — the one error it returns is
    // "PANDAY_DEBUG_CONTENT is set in production", and booting anyway would
    // mean logging prompt content into a production log.
    if let Err(e) = panday_sdk::telemetry::init("panday-cli") {
        eprintln!("panday-cli: {e}");
        std::process::exit(1);
    }

    let cmd = match parse_args(std::env::args().skip(1)) {
        Ok(cmd) => cmd,
        Err(e) => {
            eprintln!("panday: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (model, prompt) = match cmd {
        Command::Help => {
            println!("{}", help());
            return ExitCode::SUCCESS;
        }
        Command::Version => {
            println!("panday {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        // Replay reads a log and writes text — no gateway, no network, no
        // credentials. Wiring it before the runtime work below keeps that true.
        cmd @ Command::Replay { .. } => match panday_cli::run_replay(&cmd) {
            Ok(text) => {
                println!("{}", text.trim_end());
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("panday: {e}");
                return ExitCode::FAILURE;
            }
        },
        // The dogfood path (M10.3): everything here goes through the public SDK
        // sessions client, so a capability the CLI has is one every customer has.
        cmd @ Command::Session { .. } => {
            let mut out = Stdout;
            return match panday_cli::run_session(&cmd, &mut out).await {
                Ok(_) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("panday: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        // Model management (M18.2). No gateway and no credentials: a catalog, a hash, and a
        // directory of files.
        Command::Models {
            action,
            catalog,
            catalog_key,
            mirror,
        } => {
            return match panday_cli::run_models(
                &action,
                catalog.as_deref(),
                catalog_key.as_deref(),
                mirror.as_deref(),
            )
            .await
            {
                Ok(text) => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("panday: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        // An editor spawns this and speaks ACP on our stdio (docs/16, ADR-012).
        // Nothing may be printed to stdout here that is not a JSON-RPC frame.
        Command::Acp { workspace, profile } => {
            return match panday_cli::run_acp(workspace, &profile).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("panday: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        // Fetch, verify, consent, extract — in that order (docs/16 §install-time consent).
        Command::PluginInstall {
            spec,
            registry_url,
            trust,
            dir,
            yes,
        } => {
            let mut out = Stdout;
            let root = dir.unwrap_or_else(panday_cli::plugin_install::default_root);
            let mut request = match panday_cli::plugin_install::InstallRequest::parse(
                &spec,
                registry_url,
                root,
            ) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("panday: {e}");
                    return ExitCode::FAILURE;
                }
            };
            request.trust = trust;
            request.yes = yes;
            return match panday_cli::plugin_install::install(request, &mut out).await {
                // Not installing because consent was withheld is not a failure — the command
                // did what it was asked, which was to show what it would grant.
                Ok(_) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("panday: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Chat { model, prompt } => (model, prompt),
    };

    if prompt.trim().is_empty() {
        eprintln!("panday: nothing to say — give a prompt, e.g.\n  panday chat \"why is this test failing?\"");
        return ExitCode::FAILURE;
    }

    let mut config = Config::from_env();
    config.load_subscription_oauth().await;
    let usage = Arc::new(CollectUsage::new());
    let gateway = match config.build_gateway(panday_cli::DEFAULT_POLICY, usage.clone()) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("panday: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut out = Stdout;
    match panday_cli::run_chat(&gateway, &usage, chat_request(model, &prompt), &mut out).await {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "\npanday: {e}\n\nproviders configured:\n{}",
                config.describe()
            );
            ExitCode::FAILURE
        }
    }
}
