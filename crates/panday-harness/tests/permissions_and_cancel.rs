//! M13.3 — permission engine, the Ask flow, and cancellation.
//!
//! docs/13's acceptance: "Permission engine + Ask flow over WS; cancellation
//! kills a sleeping bash cleanly."
//!
//! The Ask flow is exercised at the actor's API (`decide`), not over a
//! WebSocket: the WS endpoint is M3.3, and the transport is a delivery detail
//! over the same events. The cancellation test is the real thing — it starts
//! a genuinely sleeping `bash` inside a real jail and checks the process is
//! gone.

use panday_harness::native::{register_native, Workspace};
use panday_harness::permissions::{render_call, Rule};
use panday_harness::testing::{EchoTool, ScriptedClient, ScriptedTurn};
use panday_harness::tools::{SideEffects, ToolRegistry, ToolReq};
use panday_harness::{
    Gate, MemoryStore, PermissionEngine, Phase, Profile, SessionActor, TurnBudget, TurnOutcome,
};
use panday_types::event::{Actor, Event, PermDecision};
use panday_types::model::{ModelRef, StopReason};
use panday_types::{AccountId, SessionId};
use std::sync::Arc;

fn req(se: SideEffects) -> ToolReq {
    ToolReq {
        sandbox_tier: panday_sandbox::SandboxTier::T2OsJail,
        side_effects: se,
        independent: true,
        replay: panday_harness::tools::Replay::Unsafe,
    }
}

// ---------------------------------------------------------------------------
// Rule matching
// ---------------------------------------------------------------------------

#[test]
fn an_argument_pattern_gates_a_dangerous_command_even_when_unleashed() {
    // The example docs/13 gives verbatim: `bash(rm -rf*) → Ask` even in
    // unleashed.
    let e = PermissionEngine::new(Profile::Unleashed).with_rule(Rule::ask("bash(rm -rf*)"));

    assert_eq!(
        e.gate(
            "bash",
            &req(SideEffects::Idempotent),
            &serde_json::json!({"cmd": "rm -rf /"})
        ),
        Gate::Ask
    );
    // A different command through the same tool is unaffected.
    assert_eq!(
        e.gate(
            "bash",
            &req(SideEffects::Idempotent),
            &serde_json::json!({"cmd": "cargo test"})
        ),
        Gate::Allow
    );
}

#[test]
fn a_rule_cannot_be_evaded_by_case_or_key_order() {
    let e = PermissionEngine::new(Profile::Unleashed).with_rule(Rule::deny("bash(*rm -rf*)"));

    for cmd in ["RM -RF /tmp", "sudo rm -rf /tmp", "rm -rf /tmp"] {
        assert_eq!(
            e.gate(
                "bash",
                &req(SideEffects::Idempotent),
                &serde_json::json!({"cmd": cmd})
            ),
            Gate::Deny,
            "evaded by: {cmd}"
        );
    }
}

#[test]
fn a_deny_rule_outranks_a_remembered_grant() {
    // Otherwise "always allow bash" earlier in the session would quietly
    // authorise `rm -rf` later.
    let mut e = PermissionEngine::new(Profile::Dev).with_rule(Rule::deny("bash(*rm -rf*)"));
    e.remember("bash", PermDecision::AllowRemember);

    assert_eq!(
        e.gate(
            "bash",
            &req(SideEffects::Idempotent),
            &serde_json::json!({"cmd": "cargo test"})
        ),
        Gate::Allow,
        "the remembered grant should still work for ordinary commands"
    );
    assert_eq!(
        e.gate(
            "bash",
            &req(SideEffects::Idempotent),
            &serde_json::json!({"cmd": "rm -rf /"})
        ),
        Gate::Deny,
        "a remembered grant must not override an explicit deny"
    );
}

#[test]
fn a_deny_rule_outranks_even_the_irreversible_ask() {
    // "Never" must beat "ask a human", or an operator's hard limit becomes a
    // dialog they can click through.
    let e = PermissionEngine::new(Profile::Unleashed).with_rule(Rule::deny("drop_database"));
    assert_eq!(
        e.gate(
            "drop_database",
            &req(SideEffects::Irreversible),
            &serde_json::json!({})
        ),
        Gate::Deny
    );
}

#[test]
fn a_bare_tool_pattern_matches_any_arguments() {
    let e = PermissionEngine::new(Profile::Unleashed).with_rule(Rule::ask("git_push"));
    assert_eq!(
        e.gate(
            "git_push",
            &req(SideEffects::Idempotent),
            &serde_json::json!({"remote": "origin"})
        ),
        Gate::Ask
    );
}

#[test]
fn rendering_is_stable_across_key_order() {
    let a = render_call(&serde_json::json!({"a": "one", "b": "two"}));
    let b = render_call(&serde_json::json!({"b": "two", "a": "one"}));
    assert_eq!(a, b, "a rule must not depend on JSON key order");
}

// ---------------------------------------------------------------------------
// The Ask flow
// ---------------------------------------------------------------------------

fn parked_actor(profile: Profile) -> (SessionActor, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let mut registry = ToolRegistry::default();
    registry.register(EchoTool::irreversible("deploy", "deployed to production"));

    let actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "deploying",
                vec![("deploy", serde_json::json!({"env": "prod"}))],
            ),
            ScriptedTurn::text("done"),
        ])),
        registry,
        PermissionEngine::new(profile),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );
    (actor, store)
}

#[tokio::test]
async fn allowing_a_parked_call_runs_it_and_resumes_the_turn() {
    let (mut actor, store) = parked_actor(Profile::Dev);

    let TurnOutcome::AwaitingPermission(ids) = actor.handle_user_input("deploy it").await.unwrap()
    else {
        panic!("expected the turn to park");
    };
    assert_eq!(ids.len(), 1);

    let outcome = actor
        .decide(ids[0], PermDecision::Allow, Actor::User)
        .await
        .unwrap();
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));

    let log = store.all();
    // The decision is on the record, before the result.
    let decision_at = log
        .iter()
        .position(|e| matches!(e.event, Event::PermissionDecision { .. }))
        .expect("the decision must be logged");
    let result_at = log
        .iter()
        .position(|e| matches!(e.event, Event::ToolResult { .. }))
        .expect("the tool must have run");
    assert!(
        decision_at < result_at,
        "consent must be recorded before the action it authorises"
    );

    let text = log
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolResult { output, .. } => Some(output.text.clone()),
            _ => None,
        })
        .unwrap();
    assert!(text.contains("deployed"), "the approved call did not run");
}

#[tokio::test]
async fn denying_a_parked_call_records_a_refusal_the_model_can_see() {
    let (mut actor, store) = parked_actor(Profile::Dev);
    let TurnOutcome::AwaitingPermission(ids) = actor.handle_user_input("deploy it").await.unwrap()
    else {
        panic!("expected a park");
    };

    let outcome = actor
        .decide(ids[0], PermDecision::Deny, Actor::User)
        .await
        .unwrap();
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));

    let denied = store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolResult {
                output, is_error, ..
            } if is_error => Some(output),
            _ => None,
        })
        .expect("a refusal must reach the model");
    assert_eq!(denied.strategy, "permission_denied");
    assert!(
        !store.all().iter().any(|e| matches!(
            &e.event,
            Event::ToolResult { output, .. } if output.text.contains("deployed")
        )),
        "a denied call must not have executed"
    );
}

#[tokio::test]
async fn allow_remember_stops_asking_for_that_tool() {
    let store = Arc::new(MemoryStore::new());
    let mut registry = ToolRegistry::default();
    registry.register(EchoTool::mutating("write_config", "written"));

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling("first", vec![("write_config", serde_json::json!({"k": 1}))]),
            ScriptedTurn::calling(
                "second",
                vec![("write_config", serde_json::json!({"k": 2}))],
            ),
            ScriptedTurn::text("done"),
        ])),
        registry,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );

    let TurnOutcome::AwaitingPermission(ids) = actor.handle_user_input("go").await.unwrap() else {
        panic!("expected a park");
    };

    // Remembering must carry the turn to completion without parking again.
    let outcome = actor
        .decide(ids[0], PermDecision::AllowRemember, Actor::User)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        TurnOutcome::Finished(StopReason::EndTurn),
        "the second call should not have parked after AllowRemember"
    );

    let asks = store
        .all()
        .iter()
        .filter(|e| matches!(e.event, Event::PermissionRequest { .. }))
        .count();
    assert_eq!(asks, 1, "the user should have been asked exactly once");
}

#[tokio::test]
async fn deciding_an_unknown_call_is_an_error_not_a_silent_no_op() {
    let (mut actor, _store) = parked_actor(Profile::Dev);
    actor.handle_user_input("deploy it").await.unwrap();

    let err = actor
        .decide(
            panday_types::CallId::new(),
            PermDecision::Allow,
            Actor::User,
        )
        .await
        .expect_err("an unknown call id must be rejected");
    assert!(err.to_string().contains("no parked call"), "{err}");
}

#[tokio::test]
async fn a_policy_decision_is_attributed_to_the_policy_not_the_user() {
    let (mut actor, store) = parked_actor(Profile::Dev);
    let TurnOutcome::AwaitingPermission(ids) = actor.handle_user_input("deploy it").await.unwrap()
    else {
        panic!("expected a park");
    };

    actor
        .decide(
            ids[0],
            PermDecision::Allow,
            Actor::Policy {
                rule: "ci:auto-approve-deploy".into(),
            },
        )
        .await
        .unwrap();

    let by = store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::PermissionDecision { by, .. } => Some(by),
            _ => None,
        })
        .unwrap();
    // An audit that cannot distinguish a human from a rule is not an audit.
    assert!(matches!(by, Actor::Policy { .. }));
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[tokio::test]
async fn cancellation_kills_a_sleeping_bash_cleanly() {
    use panday_sandbox::{
        FsPolicy, Limits, NetPolicy, Sandbox, SandboxPolicy, SandboxTier, SessionSpec,
        T2MacosSandbox,
    };

    if !T2MacosSandbox::available() {
        return;
    }

    let dir = std::env::temp_dir().join(format!("panday-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let root = std::fs::canonicalize(&dir).unwrap();

    let sandbox = Arc::new(T2MacosSandbox::new());
    let handle = sandbox
        .create(SessionSpec {
            tier: SandboxTier::T2OsJail,
            policy: SandboxPolicy {
                fs: FsPolicy {
                    workspace_rw: root.clone(),
                    staged_ro: vec![],
                },
                net: NetPolicy::default(),
                // Far longer than the test: the kill must come from
                // cancellation, not from the wall-clock backstop.
                limits: Limits {
                    wall_clock_ms: 600_000,
                    ..Default::default()
                },
                env: vec![],
            },
        })
        .await
        .unwrap();

    let mut registry = ToolRegistry::default();
    register_native(
        &mut registry,
        Workspace::new(sandbox.clone(), handle, root.clone()),
    );

    let store = Arc::new(MemoryStore::new());
    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![ScriptedTurn::calling(
            "sleeping",
            // Writes a marker only if it survives the sleep.
            vec![(
                "bash",
                serde_json::json!({"cmd": "sleep 30; echo survived > ./marker.txt"}),
            )],
        )])),
        registry,
        PermissionEngine::new(Profile::Unleashed),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget {
            max_wall_ms: 600_000,
            ..Default::default()
        },
    );

    let cancel = actor.cancel_handle();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        cancel.cancel();
    });

    let began = std::time::Instant::now();
    let outcome = actor.handle_user_input("run it").await.unwrap();
    let elapsed = began.elapsed();

    assert_eq!(
        outcome,
        TurnOutcome::Finished(StopReason::Cancelled),
        "cancellation must be a normal stop reason"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "cancellation did not interrupt the sleep; took {elapsed:?}"
    );
    assert_eq!(actor.state().phase, Phase::Idle, "the turn must be closed");

    // The command was actually killed, not merely abandoned: had it kept
    // running, the marker would appear.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(
        !root.join("marker.txt").exists(),
        "the sleeping command survived cancellation and kept running"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_cancelled_turn_is_recorded_and_leaves_nothing_to_replay() {
    let store = Arc::new(MemoryStore::new());
    let mut registry = ToolRegistry::default();
    registry.register(EchoTool::ok("noop", "ok"));

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![ScriptedTurn::text("hi")])),
        registry,
        PermissionEngine::new(Profile::Unleashed),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );

    actor.cancel();
    let outcome = actor.handle_user_input("go").await.unwrap();

    assert_eq!(outcome, TurnOutcome::Finished(StopReason::Cancelled));
    let folded = panday_harness::fold(&store.all());
    assert!(folded.inflight_calls.is_empty());
    assert!(folded.pending_permissions.is_empty());
    assert_eq!(folded.last_stop, Some(StopReason::Cancelled));
}
