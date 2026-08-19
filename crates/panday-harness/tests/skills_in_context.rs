//! M16.2 — the skills index in the stable prefix, bodies loaded lazily.
//!
//! docs/16's runtime semantics are a cache-economics argument, so the tests are
//! too: the index is cheap and permanent, the bodies are expensive and
//! conditional, and loading one must never disturb what is already cached.
//!
//! The two skills under `tests/skills/` are written in the unmodified SKILL.md
//! format — YAML frontmatter with keys we do not model (`license`,
//! `allowed-tools`, `version`, `author`) plus a markdown body — to demonstrate
//! the "port with zero edits" claim. They are representative of the ecosystem's
//! format rather than copies of anyone's work, which would be a licensing
//! question rather than a technical one.

use panday_harness::context::{Band, ContextBuilder};
use panday_harness::testing::{ScriptedClient, ScriptedTurn};
use panday_harness::tools::ToolRegistry;
use panday_harness::{LoadSkill, MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget};
use panday_plugins::skill::{discover, index};
use panday_types::model::ModelRef;
use panday_types::{AccountId, SessionId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn skills_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/skills")
}

fn loaded_skills() -> Vec<panday_plugins::skill::Skill> {
    discover(&skills_dir()).expect("the bundled skills must load")
}

fn bodies() -> Arc<BTreeMap<String, String>> {
    Arc::new(
        loaded_skills()
            .into_iter()
            .map(|s| (s.frontmatter.name.clone(), s.body))
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Porting
// ---------------------------------------------------------------------------

#[test]
fn both_skills_load_unmodified_including_keys_we_do_not_model() {
    let skills = loaded_skills();
    assert_eq!(skills.len(), 2, "expected two skills");

    let names: Vec<&str> = skills.iter().map(|s| s.frontmatter.name.as_str()).collect();
    assert_eq!(names, ["conventional-commits", "rust-error-handling"]);

    let rust = skills
        .iter()
        .find(|s| s.frontmatter.name == "rust-error-handling")
        .unwrap();
    // Keys from other runtimes survive rather than causing a rejection.
    assert!(rust.frontmatter.extra.contains_key("license"));
    assert!(rust.frontmatter.extra.contains_key("allowed-tools"));
    assert_eq!(
        rust.frontmatter.triggers,
        ["error handling", "Result", "unwrap"]
    );
    // Fenced code blocks in the body survive intact.
    assert!(rust.body.contains("#[derive(Debug, thiserror::Error)]"));

    let commits = skills
        .iter()
        .find(|s| s.frontmatter.name == "conventional-commits")
        .unwrap();
    assert_eq!(commits.references.len(), 1, "references/ should be found");
}

// ---------------------------------------------------------------------------
// The index lives in the stable band
// ---------------------------------------------------------------------------

fn builder_with_index() -> ContextBuilder {
    let mut b = ContextBuilder::new("You are a coding agent.", vec![]);
    b.set_skills_index(index(&loaded_skills()));
    b
}

#[test]
fn the_index_is_in_the_stable_band_and_the_bodies_are_not() {
    let b = builder_with_index();
    let ctx = b.build(&[], 0);

    let stable = match &ctx.messages[0].content[0] {
        panday_types::model::ContentBlock::Text { text } => text.clone(),
        other => panic!("unexpected block {other:?}"),
    };
    assert_eq!(ctx.bands[0], Band::Stable);

    // Names and descriptions: cheap, and paid for every turn.
    assert!(stable.contains("rust-error-handling"), "{stable}");
    assert!(stable.contains("conventional-commits"), "{stable}");
    // Bodies: expensive, and absent until asked for.
    assert!(
        !stable.contains("thiserror::Error"),
        "a skill body leaked into the stable prefix:\n{stable}"
    );
    assert!(
        stable.contains("load_skill"),
        "the model must be told how to get the rest:\n{stable}"
    );
}

#[test]
fn the_index_size_is_independent_of_how_large_the_bodies_are() {
    // The invariant that makes lazy loading worth it, and a sharper claim than
    // "the index is smaller": a skill with a 50KB body costs the stable prefix
    // exactly as much as one with a 500-byte body. An arbitrary size ratio
    // would pass or fail on how verbose these two fixtures happen to be.
    let mut skills = loaded_skills();
    let before = index(&skills).len();

    for s in skills.iter_mut() {
        s.body = "x".repeat(50_000);
    }
    let after = index(&skills).len();

    assert_eq!(
        before, after,
        "the index grew when the bodies did — it is not an index"
    );

    // And it is genuinely smaller than what it stands in for.
    let body_bytes: usize = skills.iter().map(|s| s.body.len()).sum();
    assert!(
        before * 100 < body_bytes,
        "index {before} vs bodies {body_bytes}"
    );
}

#[test]
fn loading_a_body_appends_to_semi_stable_and_leaves_the_stable_band_untouched() {
    // The cache invariant: a load must never rewrite what is already cached.
    let mut b = builder_with_index();
    let before = b.build(&[], 0).messages[0].clone();

    assert!(b.load_skill_body("rust-error-handling", "BODY TEXT"));

    let after = b.build(&[], 0);
    assert_eq!(
        after.messages[0], before,
        "loading a skill must not disturb the stable prefix"
    );
    assert_eq!(after.bands[1], Band::SemiStable);
    match &after.messages[1].content[0] {
        panday_types::model::ContentBlock::Text { text } => {
            assert!(text.contains("BODY TEXT"), "{text}");
            assert!(
                text.contains("rust-error-handling"),
                "the body should be labelled: {text}"
            );
        }
        other => panic!("unexpected block {other:?}"),
    }
}

#[test]
fn loading_the_same_skill_twice_is_a_no_op() {
    // docs/16: a loaded body "stays for the session". Appending it again would
    // both waste tokens and churn the cache behind it.
    let mut b = builder_with_index();
    assert!(b.load_skill_body("x", "BODY"));
    assert!(
        !b.load_skill_body("x", "BODY"),
        "the second load must be refused"
    );

    let ctx = b.build(&[], 0);
    let semi = ctx
        .bands
        .iter()
        .filter(|band| **band == Band::SemiStable)
        .count();
    assert_eq!(semi, 1, "the body must appear exactly once");
    assert!(b.is_skill_loaded("x"));
}

// ---------------------------------------------------------------------------
// The model-driven path, through the real loop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_model_can_load_a_skill_and_it_lands_in_the_context() {
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(LoadSkill::new(bodies())));

    let store = Arc::new(MemoryStore::new());
    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "This looks like an error-handling question.",
                vec![(
                    LoadSkill::NAME,
                    serde_json::json!({"name": "rust-error-handling"}),
                )],
            ),
            ScriptedTurn::text("Use thiserror in the library."),
        ])),
        registry,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    )
    .with_context(builder_with_index());

    actor
        .handle_user_input("how should I handle errors here?")
        .await
        .unwrap();

    // The loaded skill must be visible to the NEXT model call — that is the
    // whole point of putting it in the context rather than only in a tool
    // result.
    assert!(
        actor.context_has_skill("rust-error-handling"),
        "the skill body never reached the context"
    );
}

#[tokio::test]
async fn asking_for_a_skill_that_does_not_exist_lists_the_ones_that_do() {
    // A hallucinated name should cost one recoverable turn, not repeated
    // guessing.
    let tool = LoadSkill::new(bodies());
    let out = panday_harness::tools::Tool::call(
        &tool,
        panday_harness::tools::ToolCtx {
            account: AccountId::new(),
            session: SessionId::new(),
            turn: panday_types::TurnId::new(),
        },
        serde_json::json!({"name": "no-such-skill"}),
    )
    .await;

    assert!(out.is_error);
    assert!(out.raw.contains("rust-error-handling"), "{}", out.raw);
    assert!(out.raw.contains("conventional-commits"), "{}", out.raw);
}
