//! M21.2 — what the loop puts on the metrics endpoint (docs/21 §Metrics).
//!
//! Deltas, not absolutes: the registry is process-global, so an absolute
//! assertion would depend on test order.

use panday_harness::testing::{EchoTool, ScriptedClient, ScriptedTurn};
use panday_harness::tools::ToolRegistry;
use panday_harness::{MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget};
use panday_types::model::ModelRef;
use panday_types::{AccountId, SessionId};
use std::sync::Arc;

fn series(text: &str, prefix: &str) -> f64 {
    text.lines()
        .find(|l| l.starts_with(prefix))
        .and_then(|l| l.rsplit_once(' '))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0.0)
}

async fn run_a_turn() {
    let store = Arc::new(MemoryStore::new());
    let mut reg = ToolRegistry::default();
    reg.register(EchoTool::ok("read_file", &"a line of source\n".repeat(200)));

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store,
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "look",
                vec![("read_file", serde_json::json!({"path": "src/lib.rs"}))],
            ),
            ScriptedTurn::text("done"),
        ])),
        reg,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );
    actor.handle_user_input("go").await.unwrap();
}

#[tokio::test]
async fn a_turn_records_its_stop_reason_and_its_sandbox_time() {
    let before = panday_sdk::metrics::render();
    let turns_before = series(&before, "panday_turns_total ");
    let ended_before = series(
        &before,
        "panday_turn_stop_reasons_total{reason=\"end_turn\"}",
    );

    run_a_turn().await;

    let after = panday_sdk::metrics::render();
    // Two model calls (tool round trip, then the answer) but one *turn* per
    // step of the loop — this asserts the counter moves, not the exact loop
    // shape, which belongs to the loop's own tests.
    assert!(series(&after, "panday_turns_total ") > turns_before);
    // `>=`, not `==`: tests in one binary share the process-global registry and
    // run concurrently, so another test's turn can land between the two reads.
    assert!(
        series(
            &after,
            "panday_turn_stop_reasons_total{reason=\"end_turn\"}"
        ) >= ended_before + 1.0
    );
    // T0 because `EchoTool` declares T0 — the tier comes from the tool's
    // requirements, not from where the call was made.
    assert!(
        after.contains("panday_sandbox_seconds_total{tier=\"t0_in_process\"}"),
        "{after}"
    );
}

#[tokio::test]
async fn a_reduction_reports_the_tokens_it_removed_by_strategy() {
    run_a_turn().await;
    let text = panday_sdk::metrics::render();
    assert!(
        text.contains("panday_reducer_tokens_removed_total{strategy="),
        "{text}"
    );
    // And no dollar figure: the loop has no price table yet (M11.4), and $0
    // would be a claim that the reduction was worthless rather than unpriced.
    assert!(!text.contains("panday_reducer_saved_usd_total"), "{text}");
}

#[tokio::test]
async fn no_metric_carries_a_session_or_account_id() {
    // The whole reason `MAX_SERIES_PER_FAMILY` exists. Also a privacy property:
    // docs/21 §T5 keeps ids out of anything content-adjacent, and a metric is
    // scraped by more systems than a trace is.
    run_a_turn().await;
    let text = panday_sdk::metrics::render();
    for label in ["session_id=", "account_id=", "turn_id=", "request_id="] {
        assert!(
            !text.contains(label),
            "{label} appears in the scrape:\n{text}"
        );
    }
}
