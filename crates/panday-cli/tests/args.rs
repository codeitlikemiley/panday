//! M18.2 — `panday models` argument parsing.
//!
//! The parser is hand-rolled (no clap in this workspace), so the cases that matter are the ones a
//! hand-rolled parser gets wrong: a missing positional silently becoming a no-op, and a flag that
//! swallows the next flag as its value.

use panday_cli::{Command, CredsAction, CredsSource, Kind, ModelAction};

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

#[test]
fn creds_list_parses() {
    assert_eq!(parse(&["creds", "list"]), Command::Creds(CredsAction::List));
}

#[test]
fn creds_add_provider_and_label() {
    assert_eq!(
        parse(&["creds", "add", "--provider", "xai", "--label", "paid"]),
        Command::Creds(CredsAction::Add {
            provider: "xai".into(),
            label: Some("paid".into()),
            kind: Kind::ApiKey,
            source: CredsSource::Stdin,
        })
    );
}

#[test]
fn creds_add_kind_oauth() {
    let Command::Creds(CredsAction::Add { kind, source, .. }) =
        parse(&["creds", "add", "--provider", "xai", "--kind", "oauth"])
    else {
        panic!("expected add")
    };
    assert_eq!(kind, Kind::Oauth);
    assert_eq!(source, CredsSource::Stdin);
}

#[test]
fn creds_revoke_parses_a_uuid() {
    assert_eq!(
        parse(&["creds", "revoke", "550e8400-e29b-41d4-a716-446655440000"]),
        Command::Creds(CredsAction::Revoke {
            id: "550e8400-e29b-41d4-a716-446655440000".into(),
        })
    );
}

#[test]
fn creds_add_needs_a_provider() {
    let err = panday_cli::parse_args(["creds", "add"]).unwrap_err();
    assert!(err.contains("--provider"), "{err}");
    assert!(panday_cli::parse_args(["creds"]).is_err());
    assert!(panday_cli::parse_args(["creds", "frobnicate"]).is_err());
    assert!(panday_cli::parse_args(["creds", "revoke"]).is_err());
}

#[test]
fn creds_from_grok_parses() {
    assert_eq!(
        parse(&["creds", "add", "--from-grok", "--label", "laptop"]),
        Command::Creds(CredsAction::Add {
            provider: "xai".into(),
            label: Some("laptop".into()),
            kind: Kind::Oauth,
            source: CredsSource::Grok,
        })
    );
    assert_eq!(
        parse(&["creds", "add", "--from-claude"]).unwrap_source(),
        CredsSource::Claude
    );
    assert_eq!(
        parse(&["creds", "add", "--from-codex"]).unwrap_source(),
        CredsSource::Codex
    );
}

#[test]
fn creds_add_rejects_two_from_flags() {
    let err = panday_cli::parse_args(["creds", "add", "--from-grok", "--from-claude"]).unwrap_err();
    assert!(err.contains("only one"), "{err}");
}

#[test]
fn creds_from_grok_rejects_a_different_provider() {
    let err = panday_cli::parse_args(["creds", "add", "--from-grok", "--provider", "anthropic"])
        .unwrap_err();
    assert!(err.contains("xai"), "{err}");
}

#[test]
fn creds_add_rejects_a_secret_on_argv() {
    let err = panday_cli::parse_args(["creds", "add", "--provider", "xai", "sk-secret-on-argv"])
        .unwrap_err();
    assert!(
        err.contains("stdin") || err.contains("argv"),
        "secret on argv must be an explicit error: {err}"
    );
    assert!(err.contains("sk-secret-on-argv"), "{err}");
}

trait UnwrapSource {
    fn unwrap_source(self) -> CredsSource;
}

impl UnwrapSource for Command {
    fn unwrap_source(self) -> CredsSource {
        match self {
            Command::Creds(CredsAction::Add { source, .. }) => source,
            other => panic!("expected add, got {other:?}"),
        }
    }
}
