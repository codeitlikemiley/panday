//! M18.2 — `panday models` argument parsing.
//!
//! The parser is hand-rolled (no clap in this workspace), so the cases that matter are the ones a
//! hand-rolled parser gets wrong: a missing positional silently becoming a no-op, and a flag that
//! swallows the next flag as its value.

use panday_cli::{Command, ModelAction};

fn parse(args: &[&str]) -> Command {
    panday_cli::parse_args(args).expect("parse")
}

#[test]
fn models_actions_parse() {
    let Command::Models { action, .. } = parse(&["models", "list"]) else {
        panic!("expected models")
    };
    assert_eq!(action, ModelAction::List);

    let Command::Models {
        action,
        catalog,
        catalog_key,
        mirror,
    } = parse(&[
        "models",
        "pull",
        "qwen3.5-4b-q4",
        "--catalog",
        "https://models.example/index.json",
        "--catalog-key",
        "ab12",
        "--mirror",
        "https://mirror.internal",
    ])
    else {
        panic!("expected models")
    };
    assert_eq!(
        action,
        ModelAction::Pull {
            id: "qwen3.5-4b-q4".into()
        }
    );
    assert_eq!(
        catalog.as_deref(),
        Some("https://models.example/index.json")
    );
    assert_eq!(catalog_key.as_deref(), Some("ab12"));
    assert_eq!(mirror.as_deref(), Some("https://mirror.internal"));
}

#[test]
fn verify_without_an_id_means_everything_installed() {
    // The form a cron job wants: "tell me if anything on this disk rotted".
    let Command::Models { action, .. } = parse(&["models", "verify"]) else {
        panic!("expected models")
    };
    assert_eq!(action, ModelAction::Verify { id: None });
}

#[test]
fn a_missing_model_id_is_an_error_not_a_silent_noop() {
    assert!(panday_cli::parse_args(["models", "pull"]).is_err());
    assert!(panday_cli::parse_args(["models", "rm"]).is_err());
    assert!(panday_cli::parse_args(["models"]).is_err());
    assert!(panday_cli::parse_args(["models", "frobnicate"]).is_err());
}

#[test]
fn signing_needs_a_key_file() {
    // Signing is the one action with a private key in it; defaulting the path would put a
    // "signed with something" outcome one typo away.
    assert!(panday_cli::parse_args(["models", "sign", "index.json"]).is_err());
    assert!(panday_cli::parse_args(["models", "sign", "index.json", "--key-file", "k"]).is_ok());
}
