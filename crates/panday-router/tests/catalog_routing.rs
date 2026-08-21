//! M12.2 — routing through the catalog: patterns become models, hard limits are enforced.
//!
//! docs/12 M12.1 shipped with a note: pool entries are globs and `RouteDecision.chain` carries the
//! pattern through, because nothing could resolve one. These tests are that note being closed.

use panday_router::catalog::ModelCatalog;
use panday_router::{BudgetPressure, Caps, PolicyRouter, RouteError, RouteQuery, Router};
use panday_types::model::ModelRef;
use panday_types::model::TaskClass;

const POLICY: &str = r#"
version: 1
pools:
  frontier:  [anthropic/claude-opus-*]
  workhorse: [anthropic/claude-sonnet-*, together/qwen3.5-*-instruct]
  cheap:     [local/qwen3.5-4b, anthropic/claude-sonnet-4-5]
rules:
  - match: { task: route }
    use: cheap
  - match: { task: code }
    use: frontier
    fallback: workhorse
  - match: {}
    use: workhorse
constraints: {}
"#;

const CATALOG: &str = r#"
version: 1
models:
  - id: anthropic/claude-opus-4-1
    context: 200000
    vision: true
  - id: anthropic/claude-opus-4
    context: 200000
    vision: true
    deprecated: true
  - id: anthropic/claude-sonnet-4-5
    context: 200000
    vision: true
  - id: together/qwen3.5-32b-instruct
    context: 128000
  - id: local/qwen3.5-4b
    context: 16000
"#;

fn router() -> PolicyRouter {
    PolicyRouter::from_yaml(POLICY)
        .unwrap()
        .with_catalog(ModelCatalog::from_yaml(CATALOG).unwrap())
}

fn query(task: TaskClass) -> RouteQuery {
    RouteQuery {
        requested: ModelRef::auto(),
        task: Some(task),
        context_tokens: 1_000,
        needs: Caps::default(),
        plan: "pro".into(),
        privacy_strict: false,
        budget_pressure: BudgetPressure::Normal,
        offline: false,
    }
}

fn chain(d: &panday_router::RouteDecision) -> Vec<&str> {
    d.chain.iter().map(|m| m.0.as_str()).collect()
}

#[test]
fn a_pool_pattern_becomes_the_models_that_exist() {
    let d = router().route(&query(TaskClass::Code)).unwrap();
    // frontier, then the fallback pool, all concrete — and the deprecated opus-4 is not in it.
    assert_eq!(
        chain(&d),
        [
            "anthropic/claude-opus-4-1",
            "anthropic/claude-sonnet-4-5",
            "together/qwen3.5-32b-instruct"
        ]
    );
}

#[test]
fn overlapping_pools_do_not_produce_a_chain_that_retries_itself() {
    // `cheap` and `workhorse` share the sonnet model. Listing it twice would make failover retry a
    // model that just failed before moving on — a retry that does nothing, slowly.
    let mut q = query(TaskClass::Route);
    q.budget_pressure = BudgetPressure::Normal;
    let d = router().route(&q).unwrap();
    let c = chain(&d);
    let mut sorted = c.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), c.len(), "{c:?} contains a duplicate");
}

#[test]
fn a_model_that_cannot_hold_the_context_is_not_a_fallback() {
    // The 4B model's usable context is 16k. Offering it as the failover leg for a 40k request is
    // offering a call that will fail — later, and after the caller has waited for the good model to
    // time out first.
    let mut q = query(TaskClass::Route);
    q.context_tokens = 40_000;
    let d = router().route(&q).unwrap();
    assert!(!chain(&d).contains(&"local/qwen3.5-4b"), "{:?}", chain(&d));
    assert!(chain(&d).contains(&"anthropic/claude-sonnet-4-5"));
}

#[test]
fn a_request_needing_vision_never_lands_on_a_model_without_it() {
    let mut q = query(TaskClass::Chat);
    q.needs = Caps {
        vision: true,
        ..Default::default()
    };
    let d = router().route(&q).unwrap();
    assert_eq!(chain(&d), ["anthropic/claude-sonnet-4-5"]);
}

#[test]
fn a_chain_the_catalog_empties_is_no_route_rather_than_a_call_that_cannot_work() {
    // Nothing here can hold a million tokens. Answering `NoRoute` is the honest failure; returning
    // a chain anyway would move the error to the provider, after the caller paid to find out.
    let mut q = query(TaskClass::Chat);
    q.context_tokens = 1_000_000;
    assert!(matches!(
        router().route(&q),
        Err(RouteError::NoRoute { .. })
    ));
}

#[test]
fn without_a_catalog_patterns_pass_through_exactly_as_before() {
    // The offline tier has no catalog: `panday local` routes to whatever model name the llama-server
    // is serving, which is not a fact our file can know. Absent must therefore mean "pass through",
    // not "expand to nothing".
    let d = PolicyRouter::from_yaml(POLICY)
        .unwrap()
        .route(&query(TaskClass::Code))
        .unwrap();
    assert_eq!(chain(&d)[0], "anthropic/claude-opus-*");
}

#[test]
fn an_empty_catalog_is_a_deployment_with_no_models() {
    // Distinct from having no catalog at all, and the distinction is the whole reason the field is
    // an `Option` rather than a possibly-empty list.
    let r = PolicyRouter::from_yaml(POLICY)
        .unwrap()
        .with_catalog(ModelCatalog::empty());
    assert!(matches!(
        r.route(&query(TaskClass::Chat)),
        Err(RouteError::NoRoute { .. })
    ));
}

#[test]
fn a_pinned_model_the_catalog_knows_is_still_a_pin() {
    let mut q = query(TaskClass::Chat);
    q.requested = ModelRef("anthropic/claude-sonnet-4-5".into());
    let d = router().route(&q).unwrap();
    assert_eq!(chain(&d), ["anthropic/claude-sonnet-4-5"]);
    assert_eq!(d.matched_rule, "pinned");
}

#[test]
fn a_pinned_model_the_catalog_does_not_know_is_kept() {
    // Live `/v1/models` lists names the YAML overlay has not priced yet (agy's
    // `gemini-3.1-pro` is the example). Emptying the chain would 400 a request
    // the provider would have served. A typo still fails at the provider.
    let mut q = query(TaskClass::Chat);
    q.requested = ModelRef("anthropic/claude-onyx-9".into());
    let d = router()
        .route(&q)
        .expect("exact unknown pins stay in the chain");
    assert_eq!(chain(&d), ["anthropic/claude-onyx-9"]);
    assert_eq!(d.matched_rule, "pinned");
}

#[test]
fn the_decision_carries_what_the_chosen_model_can_do() {
    // docs/12 §offline: the router returns the active profile so the harness can tell the model
    // what it is. Without a catalog there is no claim to make, and `None` must not be read as "no
    // capabilities" — those are different statements.
    let d = router().route(&query(TaskClass::Code)).unwrap();
    let profile = d.profile.expect("the head model's profile");
    assert_eq!(profile.max_context_tokens, 200_000);
    assert!(profile.vision);

    let bare = PolicyRouter::from_yaml(POLICY)
        .unwrap()
        .route(&query(TaskClass::Code))
        .unwrap();
    assert!(bare.profile.is_none());
}
