//! M15.6 — the semantic tier and its budget gate (docs/15 §strategy stack layer 5).
//!
//! The tier is the only strategy that spends tokens to save tokens, so most of
//! these tests are about it *declining* to run. That is the milestone: a
//! summarizer that always fires is ADR-007's mistake with an extra API call.

use panday_harness::testing::{EchoTool, ScriptedClient, ScriptedTurn};
use panday_harness::tools::ToolRegistry;
use panday_harness::{
    CheapPoolSummarizer, MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget,
};
use panday_reducer::{
    Decision, GenericReducer, Pricing, ReduceCtx, SemanticConfig, SemanticTier, SummarizeError,
    Summarizer,
};
use panday_types::event::{Event, ReducedOutput};
use panday_types::model::ModelRef;
use panday_types::{AccountId, SessionId};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A summarizer that reports what it was asked and returns a fixed digest.
struct Fake {
    calls: Arc<AtomicUsize>,
    reply: String,
    fail: bool,
    last_target: Arc<AtomicUsize>,
}

impl Fake {
    fn new(reply: &str) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            reply: reply.into(),
            fail: false,
            last_target: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl Summarizer for Fake {
    fn name(&self) -> &str {
        "fake-2b"
    }
    fn cost_micros(&self, input_tokens: u32, _target: u32) -> u64 {
        // A cheap pool: $0.50/mtok.
        (input_tokens as u64) / 2
    }
    async fn summarize(&self, _text: &str, target_tokens: u32) -> Result<String, SummarizeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.last_target
            .store(target_tokens as usize, Ordering::SeqCst);
        if self.fail {
            return Err(SummarizeError::Unavailable("model down".into()));
        }
        Ok(self.reply.clone())
    }
}

fn structural(raw: &str, tokens_kept: u32) -> ReducedOutput {
    ReducedOutput {
        text: raw.to_string(),
        tokens_raw: panday_reducer::approx_tokens(raw),
        tokens_kept,
        strategy: "cargo_test_v1".into(),
    }
}

fn ctx(price_per_mtok_micros: u64, expected_reads: u32) -> ReduceCtx {
    ReduceCtx {
        tool: "bash".into(),
        task: None,
        expected_reads,
        price_per_mtok_micros,
        aggressive: false,
    }
}

/// A structural output that is still big: 8k tokens of prose.
fn big_output() -> ReducedOutput {
    let text = "a line of surviving structural output\n".repeat(900);
    structural(&text, 8_000)
}

#[tokio::test]
async fn a_local_model_never_pays_a_cloud_summarizer() {
    // The clearest instance of ADR-007: reduction on a free model saves nothing,
    // so spending cloud tokens on it is a pure loss. A size-based trigger would
    // fire here and lose money on every call.
    let tier = SemanticTier::new(Fake::new("digest"));
    let calls = tier.summarizer().calls.clone();
    let (out, decision) = tier.apply(big_output(), &ctx(0, 10)).await;

    assert_eq!(decision, Decision::Free);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        out.strategy, "cargo_test_v1",
        "the structural output stands"
    );
}

#[tokio::test]
async fn a_saving_smaller_than_the_call_is_declined() {
    // One re-read of a modest output on a cheap model: the summarizer's own cost
    // eats the saving.
    let tier = SemanticTier::new(Fake::new("digest"));
    let (_, decision) = tier
        .apply(structural(&"x\n".repeat(500), 900), &ctx(500_000, 1))
        .await;
    assert!(
        matches!(decision, Decision::NotWorthIt { .. }),
        "{decision:?}"
    );
    assert_eq!(tier.summarizer().calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_big_expensive_long_lived_output_is_summarized() {
    // 8k tokens that will be re-sent for 10 more turns on a frontier model: this
    // is the case the tier exists for.
    let tier = SemanticTier::new(Fake::new(&"short digest ".repeat(20)));
    let (out, decision) = tier.apply(big_output(), &ctx(3_000_000, 10)).await;

    assert!(decision.ran(), "{decision:?}");
    assert_eq!(tier.summarizer().calls.load(Ordering::SeqCst), 1);
    assert!(out.tokens_kept < 8_000);
    assert_eq!(out.strategy, "semantic_v1:fake-2b");
    // `tokens_raw` still measures the whole pipeline's saving: crediting layer 5
    // with layer 1's work would make every dashboard number wrong.
    assert_eq!(out.tokens_raw, big_output().tokens_raw);
}

#[tokio::test]
async fn the_target_is_a_budget_the_summarizer_is_told_about() {
    let tier = SemanticTier::new(Fake::new("digest"));
    let _ = tier.apply(big_output(), &ctx(3_000_000, 10)).await;
    let target = tier.summarizer().last_target.load(Ordering::SeqCst);
    // 25% of 8k by default — the caller priced the saving at that size.
    assert_eq!(target, 2_000);
}

#[tokio::test]
async fn a_summarizer_that_is_down_costs_the_turn_nothing() {
    // The only layer with a network dependency. A context pipeline that failed
    // when a summarizer was unreachable would be worse than not having the layer.
    let mut fake = Fake::new("unused");
    fake.fail = true;
    let tier = SemanticTier::new(fake);
    let (out, decision) = tier.apply(big_output(), &ctx(3_000_000, 10)).await;

    assert!(matches!(decision, Decision::Failed(_)), "{decision:?}");
    assert_eq!(out.strategy, "cargo_test_v1");
    assert_eq!(out.tokens_kept, 8_000);
}

#[tokio::test]
async fn a_summary_that_grew_is_discarded() {
    // Providers do echo the input back when a prompt confuses them, and storing
    // that costs tokens twice for nothing.
    let tier = SemanticTier::new(Fake::new(&"very long non-summary ".repeat(3_000)));
    let (out, decision) = tier.apply(big_output(), &ctx(3_000_000, 10)).await;
    assert_eq!(decision, Decision::NotSmaller);
    assert_eq!(out.strategy, "cargo_test_v1");
}

#[tokio::test]
async fn a_small_output_is_left_alone() {
    let tier = SemanticTier::new(Fake::new("digest"));
    let (_, decision) = tier
        .apply(
            structural("test result: ok. 12 passed", 12),
            &ctx(3_000_000, 10),
        )
        .await;
    assert_eq!(decision, Decision::AlreadySmall);
}

#[tokio::test]
async fn an_output_larger_than_the_summarizers_context_is_left_alone() {
    // Truncating the input to fit would summarize the wrong half — and quietly.
    let tier = SemanticTier::with_config(
        Fake::new("digest"),
        SemanticConfig {
            max_input_tokens: 4_000,
            ..Default::default()
        },
    );
    let (_, decision) = tier.apply(big_output(), &ctx(3_000_000, 10)).await;
    assert_eq!(decision, Decision::TooLarge);
}

#[tokio::test]
async fn error_output_is_held_to_a_higher_bar() {
    // docs/15: generic elision over error output carries a high information-risk
    // penalty. A summary is a lossy re-write, so the same penalty applies — the
    // same numbers that justify summarizing prose must not justify summarizing a
    // stack trace.
    let prose = structural(&"ordinary progress output\n".repeat(600), 4_000);
    let errors = structural(
        &"error[E0308]: mismatched types in a wall of diagnostics\n".repeat(400),
        4_000,
    );

    // Identical numbers for both — $1/mtok, re-sent twice. The only difference is
    // the information-risk penalty, and it is what flips the decision.
    let tier = SemanticTier::new(Fake::new("digest"));
    let (_, prose_decision) = tier.apply(prose, &ctx(1_000_000, 2)).await;
    let (_, error_decision) = tier.apply(errors, &ctx(1_000_000, 2)).await;

    assert!(prose_decision.ran(), "prose: {prose_decision:?}");
    assert!(
        matches!(error_decision, Decision::NotWorthIt { .. }),
        "errors: {error_decision:?}"
    );
}

// ── In the loop ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_loop_runs_the_tier_when_it_is_configured_and_priced() {
    let store = Arc::new(MemoryStore::new());
    let mut tools = ToolRegistry::default();
    tools.register(EchoTool::ok(
        "bash",
        &"a long line of tool output that structure cannot compress\n".repeat(2_000),
    ));

    let tier: SemanticTier<Box<dyn Summarizer>> = SemanticTier::new(Box::new(Fake::new(
        "digest: the build failed in gateway.rs:142",
    )));

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("anthropic/claude-sonnet-4-5".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "look",
                vec![("bash", serde_json::json!({"cmd": "cargo build"}))],
            ),
            ScriptedTurn::text("done"),
        ])),
        tools,
        PermissionEngine::new(Profile::Dev),
        Box::new(GenericReducer::default()),
        TurnBudget::default(),
    )
    .with_pricing(Pricing::anthropic_sonnet_class())
    .with_semantic_tier(tier);

    actor.handle_user_input("build it").await.unwrap();

    let strategies: Vec<String> = store
        .all()
        .iter()
        .filter_map(|e| match &e.event {
            Event::ToolResult { output, .. } => Some(output.strategy.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(strategies, vec!["semantic_v1:fake-2b".to_string()]);
}

#[tokio::test]
async fn an_unpriced_session_does_not_run_the_tier_even_when_configured() {
    // `None` pricing means unpriced, not free. Guessing a price here would let a
    // summarizer spend real money on an assumption.
    let store = Arc::new(MemoryStore::new());
    let mut tools = ToolRegistry::default();
    tools.register(EchoTool::ok("bash", &"output\n".repeat(2_000)));

    let tier: SemanticTier<Box<dyn Summarizer>> = SemanticTier::new(Box::new(Fake::new("digest")));
    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("anthropic/claude-sonnet-4-5".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling("look", vec![("bash", serde_json::json!({}))]),
            ScriptedTurn::text("done"),
        ])),
        tools,
        PermissionEngine::new(Profile::Dev),
        Box::new(GenericReducer::default()),
        TurnBudget::default(),
    )
    .with_semantic_tier(tier);

    actor.handle_user_input("go").await.unwrap();
    let strategies: Vec<String> = store
        .all()
        .iter()
        .filter_map(|e| match &e.event {
            Event::ToolResult { output, .. } => Some(output.strategy.clone()),
            _ => None,
        })
        .collect();
    assert!(
        strategies.iter().all(|s| !s.starts_with("semantic")),
        "{strategies:?}"
    );
}

#[tokio::test]
async fn the_cheap_pool_summarizer_declares_summarize_so_the_router_can_route_it() {
    // The pool is chosen by the router from the declared task class (docs/12), not
    // by a hard-coded model here — otherwise layer 5 bypasses the one component
    // whose job is picking a model.
    let script = Arc::new(ScriptedClient::new(vec![ScriptedTurn::text(
        "a terse digest",
    )]));
    let s = CheapPoolSummarizer::new(script.clone(), AccountId::new(), SessionId::new());
    let out = s.summarize("some long tool output", 100).await.unwrap();
    assert_eq!(out, "a terse digest");

    let req = &script.requests()[0];
    assert_eq!(
        req.metadata.task,
        Some(panday_types::model::TaskClass::Summarize)
    );
    assert!(req.model.is_auto(), "the router picks the pool");
    assert_eq!(
        req.sampling.temperature,
        Some(0.0),
        "a varying summary breaks the eval"
    );
    assert_eq!(req.sampling.max_tokens, Some(100));
    // The prompt names the facts that must survive — the same list the retention
    // fixtures assert on.
    let prompt = format!("{:?}", req.messages[0].content);
    assert!(prompt.contains("line numbers"), "{prompt}");
    assert!(prompt.contains("failing test names"), "{prompt}");
}

#[tokio::test]
async fn an_empty_summary_is_an_error_not_a_hundred_percent_reduction() {
    // The most expensive possible bug in this layer.
    let script = Arc::new(ScriptedClient::new(vec![ScriptedTurn::text("")]));
    let s = CheapPoolSummarizer::new(script, AccountId::new(), SessionId::new());
    assert!(s.summarize("output", 100).await.is_err());
}

#[test]
fn the_cheap_pool_price_is_the_pool_it_actually_uses() {
    let s = CheapPoolSummarizer::new(
        Arc::new(ScriptedClient::new(vec![])),
        AccountId::new(),
        SessionId::new(),
    );
    // 4000 input + 1000 output at $0.50/$1.50 per mtok = 2000 + 1500 micro-dollars.
    assert_eq!(s.cost_micros(4_000, 1_000), 3_500);

    // And the swap-in point: a tuned local model costs nothing to run, which is
    // what makes M19.5's model worth training.
    let tuned = CheapPoolSummarizer::new(
        Arc::new(ScriptedClient::new(vec![])),
        AccountId::new(),
        SessionId::new(),
    )
    .with_model(
        ModelRef("local/panday-summarizer-2b".into()),
        Pricing::local(),
    );
    assert_eq!(tuned.cost_micros(4_000, 1_000), 0);
    assert_eq!(tuned.name(), "local/panday-summarizer-2b");
}
