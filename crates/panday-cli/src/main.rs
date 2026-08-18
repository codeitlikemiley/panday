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
        Command::Chat { model, prompt } => (model, prompt),
    };

    if prompt.trim().is_empty() {
        eprintln!("panday: nothing to say — give a prompt, e.g.\n  panday chat \"why is this test failing?\"");
        return ExitCode::FAILURE;
    }

    let config = Config::from_env();
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
