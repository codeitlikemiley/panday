//! M13.5 — crash during Executing, then resume.
//!
//! docs/13's acceptance: "Crash-kill during Executing → resume replays
//! correctly (idempotent) and refuses (irreversible) — both proven by tests."
//!
//! A "crash" here is modelled the way it actually happens: the log has a
//! `ToolCall` with no matching `ToolResult`, because the process died between
//! dispatching and recording. A fresh actor over that same store is exactly
//! what a restarted process sees.

use panday_harness::testing::{EchoTool, ScriptedClient, ScriptedTurn};
use panday_harness::tools::{Replay, ToolRegistry};
use panday_harness::{
    fold, EventStore, MemoryStore, PermissionEngine, Phase, Profile, SessionActor, TurnBudget,
};
use panday_types::event::{Envelope, Event, ReducedOutput};
use panday_types::model::{ModelRef, StopReason, Usage};
use panday_types::{AccountId, CallId, SessionId, TurnId};
use std::sync::Arc;

fn env(sid: SessionId, seq: u64, event: Event) -> Envelope {
    Envelope {
        v: panday_types::PROTOCOL_VERSION,
        session_id: sid,
        seq,
        turn_id: Some(TurnId::new()),
        at: time::OffsetDateTime::now_utc(),
        event,
    }
}

/// A log that stops mid-execution: the call is recorded, the result is not.
async fn crashed_log(tool: &str) -> (Arc<MemoryStore>, SessionId, CallId) {
    let store = Arc::new(MemoryStore::new());
    let sid = SessionId::new();
    let call = CallId::new();

    for e in [
        env(
            sid,
            1,
            Event::UserMessage {
                content: vec![panday_types::model::ContentBlock::Text {
                    text: "do the thing".into(),
                }],
                source: panday_types::event::ClientKind::Cli,
            },
        ),
        env(
            sid,
            2,
            Event::TurnStarted {
                model: ModelRef("local/test".into()),
                parent: None,
            },
        ),
        env(
            sid,
            3,
            Event::ToolCall {
                call_id: call,
                tool: tool.into(),
                args: serde_json::json!({"target": "production"}),
                provider_call_id: None,
            },
        ),
        // ---- process dies here ----
    ] {
        store.append(e).await.unwrap();
    }
    (store, sid, call)
}

fn actor_over(
    store: Arc<MemoryStore>,
    sid: SessionId,
    tools: Vec<Box<dyn panday_harness::tools::Tool>>,
) -> SessionActor {
    let mut registry = ToolRegistry::default();
    for t in tools {
        registry.register(t);
    }
    SessionActor::new(
        sid,
        AccountId::new(),
        ModelRef("local/test".into()),
        store,
        Arc::new(ScriptedClient::new(vec![ScriptedTurn::text("continuing")])),
        registry,
        PermissionEngine::new(Profile::Unleashed),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    )
}

#[tokio::test]
async fn the_crash_leaves_exactly_one_call_to_decide_about() {
    let (store, _sid, call) = crashed_log("read_file").await;
    let state = fold(&store.all());
    assert_eq!(state.phase, Phase::Executing);
    assert_eq!(state.inflight_calls.len(), 1);
    assert_eq!(state.inflight_calls[0].0, call);
}

#[tokio::test]
async fn a_replay_safe_call_is_re_run_on_resume() {
    let (store, sid, call) = crashed_log("read_file").await;
    let mut actor = actor_over(
        store.clone(),
        sid,
        vec![EchoTool::ok("read_file", "file contents")],
    );

    actor.resume().await.unwrap();
    let refused = actor.resume_pending().await.unwrap();

    assert!(refused.is_empty(), "a safe call should not be refused");
    let result = store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolResult {
                call_id, output, ..
            } if call_id == call => Some(output),
            _ => None,
        })
        .expect("the replayed call must record a result");
    assert!(
        result.text.contains("file contents"),
        "the call was not actually re-run: {}",
        result.text
    );
    assert!(fold(&store.all()).inflight_calls.is_empty());
}

#[tokio::test]
async fn a_replay_unsafe_call_is_refused_and_the_refusal_carries_the_arguments() {
    // The whole point of refusing: a human has to decide whether it already
    // ran, and cannot without seeing what was run.
    let (store, sid, call) = crashed_log("deploy").await;
    let mut actor = actor_over(
        store.clone(),
        sid,
        vec![EchoTool::irreversible("deploy", "DEPLOYED AGAIN")],
    );

    actor.resume().await.unwrap();
    let refused = actor.resume_pending().await.unwrap();

    assert_eq!(refused, vec![call]);
    let result = store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolResult {
                call_id, output, ..
            } if call_id == call => Some(output),
            _ => None,
        })
        .expect("the refusal must be recorded");

    assert_eq!(result.strategy, "replay_refused");
    assert!(
        result.text.contains("production"),
        "the refusal must surface the call's arguments: {}",
        result.text
    );
    assert!(
        !result.text.contains("DEPLOYED AGAIN"),
        "the tool must NOT have run: {}",
        result.text
    );
    assert!(fold(&store.all()).inflight_calls.is_empty());
}

#[tokio::test]
async fn bash_is_refused_on_resume_despite_being_idempotent_for_consent() {
    // The seam M13.2 surfaced. `bash` declares Idempotent so `dev` can run
    // tests without prompting; that must not be read as replay-safe.
    let tool = panday_harness::native::Bash(panday_harness::native::Workspace::new(
        Arc::new(panday_sandbox::T0Sandbox::new()),
        panday_sandbox::SandboxHandle {
            id: "unused".into(),
            tier: panday_sandbox::SandboxTier::T2OsJail,
        },
        std::env::temp_dir(),
    ));
    let req = panday_harness::tools::Tool::requirements(&tool);

    assert_eq!(
        req.side_effects,
        panday_harness::tools::SideEffects::Idempotent,
        "consent: dev must be able to run tests unprompted"
    );
    assert_eq!(
        req.replay,
        Replay::Unsafe,
        "replay: an arbitrary shell command must never be re-run automatically"
    );
}

#[tokio::test]
async fn a_tool_the_registry_no_longer_knows_is_refused_not_replayed() {
    // After an upgrade a tool may be gone. Its replay safety is unknowable,
    // and guessing "safe" is the dangerous direction.
    let (store, sid, call) = crashed_log("removed_in_v2").await;
    let mut actor = actor_over(store.clone(), sid, vec![]);

    actor.resume().await.unwrap();
    let refused = actor.resume_pending().await.unwrap();
    assert_eq!(refused, vec![call]);
}

#[tokio::test]
async fn a_permission_parked_call_is_never_replayed_on_resume() {
    // It was never dispatched, so there is nothing to replay — and running it
    // would execute something nobody approved.
    let store = Arc::new(MemoryStore::new());
    let sid = SessionId::new();
    let call = CallId::new();
    for e in [
        env(
            sid,
            1,
            Event::TurnStarted {
                model: ModelRef("local/test".into()),
                parent: None,
            },
        ),
        env(
            sid,
            2,
            Event::ToolCall {
                call_id: call,
                tool: "deploy".into(),
                args: serde_json::json!({}),
                provider_call_id: None,
            },
        ),
        env(
            sid,
            3,
            Event::PermissionRequest {
                call_id: call,
                tool: "deploy".into(),
                action: "deploy(prod)".into(),
                options: vec!["allow".into(), "deny".into()],
            },
        ),
    ] {
        store.append(e).await.unwrap();
    }

    let mut actor = actor_over(
        store.clone(),
        sid,
        vec![EchoTool::irreversible("deploy", "DEPLOYED")],
    );
    actor.resume().await.unwrap();
    let refused = actor.resume_pending().await.unwrap();

    assert!(refused.is_empty(), "nothing was in flight to refuse");
    assert!(
        !store
            .all()
            .iter()
            .any(|e| matches!(&e.event, Event::ToolResult { .. })),
        "a parked call must not run on resume"
    );
    assert_eq!(fold(&store.all()).pending_permissions, vec![call]);
}

#[tokio::test]
async fn a_finished_turn_leaves_nothing_for_resume_to_do() {
    let (store, sid, call) = crashed_log("read_file").await;
    store
        .append(env(
            sid,
            4,
            Event::ToolResult {
                call_id: call,
                output: ReducedOutput {
                    text: "already recorded".into(),
                    tokens_raw: 2,
                    tokens_kept: 2,
                    strategy: "passthrough".into(),
                },
                raw_ref: None,
                duration_ms: 5,
                is_error: false,
            },
        ))
        .await
        .unwrap();
    store
        .append(env(
            sid,
            5,
            Event::TurnFinished {
                reason: StopReason::EndTurn,
                usage: Usage::default(),
                cost_micros: 0,
            },
        ))
        .await
        .unwrap();

    let mut actor = actor_over(
        store.clone(),
        sid,
        vec![EchoTool::ok("read_file", "should not run again")],
    );
    actor.resume().await.unwrap();
    let refused = actor.resume_pending().await.unwrap();

    assert!(refused.is_empty());
    let reruns = store
        .all()
        .iter()
        .filter(|e| matches!(&e.event, Event::ToolResult { output, .. } if output.text.contains("should not run again")))
        .count();
    assert_eq!(reruns, 0, "a completed turn must not be re-executed");
}

#[tokio::test]
async fn resume_restores_usage_and_sequence_from_the_log_alone() {
    let (store, sid, _call) = crashed_log("read_file").await;
    store
        .append(env(
            sid,
            4,
            Event::AssistantMessage {
                content: vec![],
                usage: Usage {
                    input_tokens: 500,
                    output_tokens: 20,
                    ..Default::default()
                },
            },
        ))
        .await
        .unwrap();

    let mut actor = actor_over(store.clone(), sid, vec![EchoTool::ok("read_file", "x")]);
    actor.resume().await.unwrap();

    assert_eq!(actor.state().last_seq, 4);
    assert_eq!(actor.state().usage_total.input_tokens, 500);
}
