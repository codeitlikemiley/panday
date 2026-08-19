//! M10.4 — `#[tool]` and its compile-fail UI tests (docs/10 Layer 4).
//!
//! The happy path is tested here; the *error messages* are tested in `tests/ui`, which
//! is most of what a proc macro's quality is. A macro that accepts a bad signature and
//! fails somewhere inside its own generated code produces an error pointing at tokens
//! the developer never wrote, and that is the experience these fixtures exist to
//! prevent.

use panday_harness::testing::{ScriptedClient, ScriptedTurn};
use panday_harness::tools::{Replay, SideEffects, Tool, ToolCtx, ToolRegistry};
use panday_harness::{MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget};
use panday_sandbox::SandboxTier;
use panday_types::event::Event;
use panday_types::model::ModelRef;
use panday_types::{AccountId, SessionId, TurnId};
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
struct Order {
    id: String,
    status: String,
}

/// Look up an order by id.
#[panday_sdk::tool]
async fn lookup_order(
    _ctx: &ToolCtx,
    order_id: String,
    verbose: Option<bool>,
) -> Result<Order, String> {
    if order_id.is_empty() {
        return Err("an order id is required".into());
    }
    Ok(Order {
        id: order_id,
        status: if verbose.unwrap_or(false) {
            "shipped, tracking 1Z999".into()
        } else {
            "shipped".into()
        },
    })
}

/// Refund an order. This one moves money.
#[panday_sdk::tool(side_effects = "irreversible")]
async fn refund_order(_ctx: &ToolCtx, order_id: String) -> Result<String, String> {
    Ok(format!("refunded {order_id}"))
}

fn ctx() -> ToolCtx {
    ToolCtx {
        account: AccountId::new(),
        session: SessionId::new(),
        turn: TurnId::new(),
    }
}

#[test]
fn the_schema_comes_from_the_signature() {
    // The point of the macro: the parameter list *is* the schema, so they cannot
    // disagree. A hand-written schema next to a signature drifts on the first edit.
    let spec = LookupOrder.spec();
    assert_eq!(spec.name, "lookup_order");
    assert_eq!(spec.description, "Look up an order by id.");

    let properties = &spec.parameters["properties"];
    assert!(properties.get("order_id").is_some(), "{spec:?}");
    assert!(properties.get("verbose").is_some(), "{spec:?}");
    // Required-ness comes from `Option`, which is the Rust reading a developer
    // already has in their head.
    let required = spec.parameters["required"].as_array().unwrap();
    assert!(required.iter().any(|r| r == "order_id"));
    assert!(
        !required.iter().any(|r| r == "verbose"),
        "an Option must not be required: {spec:?}"
    );
    // `ctx` is not an argument the model fills in.
    assert!(properties.get("ctx").is_none(), "{spec:?}");
    assert!(properties.get("_ctx").is_none(), "{spec:?}");
}

#[test]
fn the_description_is_the_doc_comment() {
    // One description, maintained for humans, read by the model. Two would drift, and
    // the one the model sees is the one nobody reviews.
    assert_eq!(
        RefundOrder.spec().description,
        "Refund an order. This one moves money."
    );
}

#[tokio::test]
async fn calling_it_deserializes_arguments_and_serializes_the_result() {
    let outcome = LookupOrder
        .call(ctx(), serde_json::json!({"order_id": "A-1"}))
        .await;
    assert!(!outcome.is_error);
    let parsed: serde_json::Value = serde_json::from_str(&outcome.raw).unwrap();
    assert_eq!(parsed["id"], "A-1");
    assert_eq!(parsed["status"], "shipped");
}

#[tokio::test]
async fn the_functions_own_error_reaches_the_model_as_a_tool_error() {
    let outcome = LookupOrder
        .call(ctx(), serde_json::json!({"order_id": ""}))
        .await;
    assert!(outcome.is_error);
    assert_eq!(outcome.raw, "an order id is required");
}

#[tokio::test]
async fn a_missing_argument_is_a_tool_error_naming_the_problem() {
    // Recoverable: the model reads this and fixes its next call.
    let outcome = LookupOrder.call(ctx(), serde_json::json!({})).await;
    assert!(outcome.is_error);
    assert!(
        outcome.raw.contains("invalid arguments for `lookup_order`"),
        "{}",
        outcome.raw
    );
    assert!(outcome.raw.contains("order_id"), "{}", outcome.raw);
}

#[tokio::test]
async fn a_misspelled_argument_is_refused_rather_than_ignored() {
    // `deny_unknown_fields`. A silently dropped `order_i` would look like the tool
    // ignoring instructions, which is the hardest kind of bug to see in a transcript.
    let outcome = LookupOrder
        .call(
            ctx(),
            serde_json::json!({"order_id": "A-1", "order_i": "A-2"}),
        )
        .await;
    assert!(outcome.is_error);
    assert!(outcome.raw.contains("order_i"), "{}", outcome.raw);
}

#[test]
fn declared_side_effects_reach_the_permission_engine() {
    assert_eq!(LookupOrder.requirements().side_effects, SideEffects::None);
    assert_eq!(LookupOrder.requirements().replay, Replay::Safe);
    assert_eq!(
        LookupOrder.requirements().sandbox_tier,
        SandboxTier::T0InProcess
    );

    // A tool that moves money is never replayed after a crash.
    assert_eq!(
        RefundOrder.requirements().side_effects,
        SideEffects::Irreversible
    );
    assert_eq!(RefundOrder.requirements().replay, Replay::Unsafe);
}

#[tokio::test]
async fn the_function_is_still_callable_by_its_own_name() {
    // The reason the macro generates `LookupOrder` instead of taking over
    // `lookup_order` (docs/10's sketch): a tool you cannot call directly is a tool you
    // can only test through an agent loop.
    let order = lookup_order(&ctx(), "A-9".into(), None).await.unwrap();
    assert_eq!(order.id, "A-9");
}

#[tokio::test]
async fn a_macro_tool_works_in_a_real_loop() {
    let store = Arc::new(MemoryStore::new());
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(LookupOrder));

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "checking",
                vec![("lookup_order", serde_json::json!({"order_id": "A-7"}))],
            ),
            ScriptedTurn::text("it shipped"),
        ])),
        registry,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );
    actor
        .handle_user_input("where is order A-7?")
        .await
        .unwrap();

    let result = store
        .all()
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolResult { output, .. } => Some(output.text.clone()),
            _ => None,
        })
        .expect("the tool ran");
    assert!(result.contains("A-7"), "{result}");
}

#[test]
fn bad_signatures_fail_to_compile_with_a_useful_message() {
    // docs/10 M10.4: "compile-fail UI tests for bad signatures". The expected stderr
    // is checked in; regenerate with `TRYBUILD=overwrite cargo test -p panday-harness
    // --test tool_macro`.
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
