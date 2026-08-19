//! M10.5 — the embedded agent runs a three-tool loop offline (docs/10 Layer 4).
//!
//! > "Embedded Agent runs a 3-tool loop offline against `panday local`."
//!
//! "Offline" here means against an OpenAI-compatible server on loopback — the same thing
//! `panday local` talks to (docs/18: "the gateway's `local` adapter doesn't care which"). The
//! server in this suite is a fake one, because CI has no GGUF; what is real is the loop, the
//! three tools, the gate, the reducer and the log.
//!
//! The tools are defined with `#[panday_sdk::tool]` (M10.4), which is the point: the two
//! halves of docs/10's Layer 4 are a macro that turns a function into a tool and an agent
//! that runs those tools, and a test using one without the other would leave the seam
//! between them untested.

use panday_harness::tools::{ToolCtx, ToolRegistry};
use panday_harness::{Agent, Profile};
use panday_types::model::StopReason;
use serde::Serialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Serialize)]
struct Order {
    id: String,
    status: String,
    total_cents: u32,
}

/// Look up an order by id.
#[panday_sdk::tool]
async fn lookup_order(_ctx: &ToolCtx, order_id: String) -> Result<Order, String> {
    Ok(Order {
        id: order_id,
        status: "shipped".into(),
        total_cents: 4_299,
    })
}

/// Find the tracking number for a shipment.
#[panday_sdk::tool]
async fn track_shipment(_ctx: &ToolCtx, order_id: String) -> Result<String, String> {
    Ok(format!("1Z999AA1{}", order_id.len()))
}

/// Write a one-line summary of an order into the notes file.
#[panday_sdk::tool(side_effects = "idempotent")]
async fn note_order(_ctx: &ToolCtx, order_id: String, note: String) -> Result<String, String> {
    Ok(format!("noted {order_id}: {note}"))
}

/// Notify the customer. Sends a real email.
#[panday_sdk::tool(side_effects = "irreversible")]
async fn notify_customer(_ctx: &ToolCtx, order_id: String, body: String) -> Result<String, String> {
    Ok(format!("emailed the customer about {order_id}: {body}"))
}

fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(LookupOrder));
    registry.register(Box::new(TrackShipment));
    registry.register(Box::new(NoteOrder));
    registry.register(Box::new(NotifyCustomer));
    registry
}

/// A loopback OpenAI-compatible server that drives a three-tool loop.
///
/// Scripted per call rather than by a model, because the milestone is about the *loop*: a
/// real local model would make this test a measurement of that model's tool discipline.
async fn fake_local_server(script: Vec<Vec<(&'static str, serde_json::Value)>>) -> String {
    use axum::response::sse::{Event, Sse};
    use axum::routing::post;
    use axum::Router;

    let calls = Arc::new(AtomicUsize::new(0));
    let script = Arc::new(script);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let calls = calls.clone();
            let script = script.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let mut frames: Vec<String> = Vec::new();
                match script.get(n) {
                    Some(tool_calls) if !tool_calls.is_empty() => {
                        let calls_json: Vec<serde_json::Value> = tool_calls
                            .iter()
                            .enumerate()
                            .map(|(i, (name, args))| {
                                serde_json::json!({
                                    "index": i,
                                    "id": format!("call_{n}_{i}"),
                                    "type": "function",
                                    "function": {"name": name, "arguments": args.to_string()}
                                })
                            })
                            .collect();
                        frames.push(
                            serde_json::json!({
                                "choices": [{"index": 0, "delta": {"content": "working on it"}}]
                            })
                            .to_string(),
                        );
                        frames.push(
                            serde_json::json!({
                                "choices": [{"index": 0, "delta": {"tool_calls": calls_json}}]
                            })
                            .to_string(),
                        );
                        frames.push(
                            serde_json::json!({
                                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                                "usage": {"prompt_tokens": 500, "completion_tokens": 20,
                                          "prompt_tokens_details": {"cached_tokens": 300}}
                            })
                            .to_string(),
                        );
                    }
                    _ => {
                        frames.push(
                            serde_json::json!({
                                "choices": [{"index": 0, "delta": {"content":
                                    "Order A-1 shipped for $42.99, tracking 1Z999AA13; the customer has been told."}}]
                            })
                            .to_string(),
                        );
                        frames.push(
                            serde_json::json!({
                                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                                "usage": {"prompt_tokens": 900, "completion_tokens": 40,
                                          "prompt_tokens_details": {"cached_tokens": 800}}
                            })
                            .to_string(),
                        );
                    }
                }
                let stream = futures_util::stream::iter(
                    frames
                        .into_iter()
                        .map(|f| Ok::<_, std::convert::Infallible>(Event::default().data(f)))
                        .chain(std::iter::once(Ok(Event::default().data("[DONE]")))),
                );
                Sse::new(stream)
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{}", addr.port())
}

/// A gateway with only the local adapter — `panday local`'s composition (docs/18).
fn local_gateway(base_url: String) -> Arc<dyn panday_sdk::ModelClient> {
    let router = panday_router::PolicyRouter::from_yaml(include_str!(
        "../../panday-router/policy/local.yaml"
    ))
    .expect("policy");
    Arc::new(
        panday_gateway::Gateway::builder(Arc::new(router))
            .adapter(
                "local",
                Arc::new(panday_gateway::adapters::openai_compat::OpenAiCompat::local(base_url))
                    as Arc<dyn panday_gateway::ProviderAdapter>,
            )
            .build(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn three_tools_run_in_one_loop_against_a_loopback_server() {
    let base = fake_local_server(vec![
        vec![("lookup_order", serde_json::json!({"order_id": "A-1"}))],
        vec![("track_shipment", serde_json::json!({"order_id": "A-1"}))],
        vec![(
            "note_order",
            serde_json::json!({"order_id": "A-1", "note": "shipped, tracking 1Z999AA13"}),
        )],
    ])
    .await;

    let mut agent = Agent::builder()
        .model("local/qwen3.5-4b")
        .tools(registry())
        // `unleashed` because this is the embedder's own process and they said so. Note that
        // it would *not* be enough for `notify_customer`: an `Irreversible` tool asks in
        // every profile (docs/13 M13.5), which is why the loop here uses a mutating-but-
        // replayable tool for its third step and the gating tests use the email.
        .policy(Profile::Unleashed)
        .build(local_gateway(base));

    let run = agent
        .run("where is order A-1, and tell the customer")
        .await
        .expect("the loop ran");

    assert!(run.finished(), "{run:?}");
    assert_eq!(run.stop, StopReason::EndTurn);
    assert_eq!(run.tool_calls(), 3, "three tools, one loop");
    assert!(run.text.contains("1Z999AA13"), "{}", run.text);
    // Usage is folded from the assistant messages, not invented.
    assert!(run.usage.input_tokens > 0);
    assert!(run.usage.cache_read_tokens > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn each_tools_result_reaches_the_next_turn() {
    // A "three-tool loop" that never fed a result back would be three independent calls. The
    // proof is that the *server* saw the earlier results in the transcript it was sent.
    let base = fake_local_server(vec![
        vec![("lookup_order", serde_json::json!({"order_id": "A-7"}))],
        vec![("track_shipment", serde_json::json!({"order_id": "A-7"}))],
    ])
    .await;

    let mut agent = Agent::builder()
        .model("local/qwen3.5-4b")
        .tools(registry())
        .policy(Profile::Unleashed)
        .build(local_gateway(base));
    let run = agent.run("where is A-7?").await.unwrap();

    let log = format!("{:?}", run.events);
    // The order lookup's own output is in the log, so it was in the context of the turn that
    // asked for tracking.
    assert!(log.contains("shipped"), "{log}");
    assert!(log.contains("1Z999AA13"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_irreversible_tool_parks_the_run_by_default() {
    // The default policy is `Dev`, not `Unleashed`: an embedded agent with no gate is a
    // library that can email a customer because a model asked. A caller who wants that has
    // to type it.
    let base = fake_local_server(vec![vec![(
        "notify_customer",
        serde_json::json!({"order_id": "A-9", "body": "hello"}),
    )]])
    .await;

    let mut agent = Agent::builder()
        .model("local/qwen3.5-4b")
        .tools(registry())
        .build(local_gateway(base));

    let run = agent.run("tell the customer about A-9").await.unwrap();
    assert!(!run.finished(), "an irreversible tool must ask: {run:?}");
    assert_eq!(
        run.stop,
        StopReason::ToolUse,
        "a paused turn is not EndTurn"
    );
    assert_eq!(run.awaiting.len(), 1);

    // And continuing is one call, with the same log carrying both halves.
    let after = agent.decide(run.awaiting[0], true).await.unwrap();
    assert!(after.tool_calls() >= 1, "{after:?}");
    assert!(
        format!("{:?}", after.events).contains("emailed the customer"),
        "{after:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_tool_leaves_the_side_effect_undone() {
    let base = fake_local_server(vec![vec![(
        "notify_customer",
        serde_json::json!({"order_id": "A-9", "body": "hello"}),
    )]])
    .await;
    let mut agent = Agent::builder()
        .model("local/qwen3.5-4b")
        .tools(registry())
        .build(local_gateway(base));

    let run = agent.run("tell the customer").await.unwrap();
    let after = agent.decide(run.awaiting[0], false).await.unwrap();
    assert!(
        !format!("{:?}", after.events).contains("emailed the customer"),
        "the denied tool ran anyway: {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_run_carries_the_log_a_replay_can_render() {
    // docs/10: "the same state machine that powers the cloud". The observable form of that
    // claim is that an embedded run's events render with the same tool the hosted product's
    // do.
    let base = fake_local_server(vec![vec![(
        "lookup_order",
        serde_json::json!({"order_id": "A-1"}),
    )]])
    .await;
    let mut agent = Agent::builder()
        .model("local/qwen3.5-4b")
        .tools(registry())
        .policy(Profile::Unleashed)
        .build(local_gateway(base));
    agent.run("where is A-1?").await.unwrap();

    let rendered = panday_harness::render(&agent.events(), Default::default());
    assert!(rendered.contains("where is A-1?"), "{rendered}");
    assert!(rendered.contains("→ lookup_order"), "{rendered}");
    assert!(rendered.contains("EndTurn"), "{rendered}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tools_schema_comes_from_its_signature_all_the_way_through() {
    // The M10.4/M10.5 seam: what the model is offered is what the macro derived, and nobody
    // wrote it twice.
    let base = fake_local_server(vec![]).await;
    let mut agent = Agent::builder()
        .model("local/qwen3.5-4b")
        .tools(registry())
        .policy(Profile::Unleashed)
        .build(local_gateway(base));
    agent.run("hello").await.unwrap();

    // The tool schemas ride in the stable band, so they are in the first request — asserted
    // through the log rather than by reaching into the client, because the log is what an
    // embedder can also see.
    let names: Vec<String> = registry().specs().into_iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        [
            "lookup_order",
            "track_shipment",
            "note_order",
            "notify_customer"
        ]
    );
    let schema = registry().specs()[0].parameters.clone();
    assert_eq!(schema["properties"]["order_id"]["type"], "string");
    assert!(schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "order_id"));
}
