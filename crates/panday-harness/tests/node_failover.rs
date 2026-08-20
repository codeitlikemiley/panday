//! M22.4 — a node dies mid-session and the session resumes elsewhere.
//!
//! docs/22: "session-actor affinity via consistent hashing on session_id at the LB (an actor lives
//! on one node; failover = fold the log on another)."
//!
//! The claim being tested is that failover needs no migration. Nothing is copied between nodes, no
//! handoff is coordinated, no state is drained — the log *is* the state (ADR-002), so a second node
//! reconstructs the session by folding it. This is why killing a node mid-execution is a latency
//! event rather than a data-loss event, and it is the half of M22.4's chaos test that does not need
//! a cluster.
//!
//! What still needs real nodes is timing: how long a fold takes on a cold cache, and whether the
//! load balancer notices the death before the client does.

use panday_harness::testing::{EchoTool, ScriptedClient, ScriptedTurn};
use panday_harness::tools::ToolRegistry;
use panday_harness::{
    fold, MemoryStore, PermissionEngine, Phase, Profile, SessionActor, TurnBudget,
    TurnOutcome,
};
use panday_sandbox::t3::{drain, NodeId, Ring};
use panday_types::model::ModelRef;
use panday_types::{AccountId, SessionId};
use std::sync::Arc;

/// One node's actor for a session. Nodes are stateless, so "moving" a session is just building this
/// somewhere else over the same store.
fn actor_on(node: &NodeId, session: SessionId, store: Arc<MemoryStore>) -> SessionActor {
    let mut registry = ToolRegistry::default();
    registry.register(EchoTool::ok("read_file", "fn main() {}"));
    let _ = node; // a node contributes nothing but where the process happens to run
    SessionActor::new(
        session,
        AccountId::new(),
        ModelRef("local/test".into()),
        store,
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "reading",
                vec![("read_file", serde_json::json!({"path": "a"}))],
            ),
            ScriptedTurn::text("done"),
        ])),
        registry,
        PermissionEngine::new(Profile::Unleashed),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    )
}

#[tokio::test]
async fn a_session_resumes_on_another_node_by_folding_its_log() {
    // The whole of failover, in one test: run a turn on the node the ring chose, kill that node,
    // and rebuild the session somewhere else from nothing but the log.
    let mut ring = Ring::with_nodes(128, &["node-a", "node-b", "node-c"]);
    let session = SessionId::new();
    let key = session.0.to_string();

    let home = ring.place(&key).expect("a home node");
    let store = Arc::new(MemoryStore::new());

    let mut actor = actor_on(&home, session, store.clone());
    actor.handle_user_input("read a").await.expect("turn");

    let before = fold(&store.all());
    assert!(before.finished_turns > 0, "nothing happened to resume");
    let events_before = store.all().len();

    // The node dies. Nothing is drained, nothing is handed over.
    let moves = drain(&mut ring, &home, std::slice::from_ref(&key));
    assert_eq!(moves.len(), 1, "the session was not reassigned");
    let elsewhere = moves[0].to.clone();
    assert_ne!(elsewhere, home);

    // A fresh actor on the new node, over the same log, adopting it with `resume()`. That call is
    // the entire migration: it reads the log and folds it, and there is nothing else to transfer.
    let mut resumed = actor_on(&elsewhere, session, store.clone());
    resumed.resume().await.expect("adopt the log");
    let after = fold(&store.all());

    assert_eq!(
        after.phase, before.phase,
        "the fold produced a different phase"
    );
    assert_eq!(after.finished_turns, before.finished_turns);
    assert_eq!(after.last_seq, before.last_seq);
    assert_eq!(after.usage_total, before.usage_total);
    assert_eq!(
        store.all().len(),
        events_before,
        "resuming wrote events of its own"
    );
    drop(resumed);
}

#[tokio::test]
async fn the_resumed_session_can_carry_on() {
    // Resuming is only useful if the session then *works*. The second node takes the next turn.
    let mut ring = Ring::with_nodes(128, &["node-a", "node-b"]);
    let session = SessionId::new();
    let key = session.0.to_string();
    let store = Arc::new(MemoryStore::new());

    let home = ring.place(&key).unwrap();
    let mut first = actor_on(&home, session, store.clone());
    first.handle_user_input("read a").await.unwrap();
    let turns_after_first = fold(&store.all()).finished_turns;

    let moves = drain(&mut ring, &home, &[key]);
    let mut second = actor_on(&moves[0].to, session, store.clone());
    // Without `resume()` the new actor would start numbering at seq 1 and collide with the log it
    // inherited — which is exactly the `SeqConflict` a naive failover would hit in production.
    second.resume().await.expect("adopt the log");
    let outcome = second.handle_user_input("read it again").await.unwrap();

    assert!(matches!(
        outcome,
        TurnOutcome::Finished(_) | TurnOutcome::AwaitingPermission(_)
    ));
    assert!(
        fold(&store.all()).finished_turns > turns_after_first,
        "the resumed node could not take a turn"
    );
}

#[tokio::test]
async fn a_session_mid_execution_resumes_into_the_same_phase() {
    // The chaos-test shape: the node dies *during* a tool call, so the log holds a call with no
    // result. What the second node must not do is invent an outcome for it.
    let store = Arc::new(MemoryStore::new());
    let session = SessionId::new();

    // A turn that parks awaiting permission is a mid-flight state we can create deterministically.
    let mut registry = ToolRegistry::default();
    registry.register(EchoTool::irreversible("bash", "pushed"));
    let mut actor = SessionActor::new(
        session,
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "pushing",
                vec![("bash", serde_json::json!({"cmd": "git push"}))],
            ),
            ScriptedTurn::text("done"),
        ])),
        registry,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );

    let parked = actor.handle_user_input("push it").await.unwrap();
    assert!(matches!(parked, TurnOutcome::AwaitingPermission(_)));
    let phase_before = fold(&store.all()).phase;

    // The node vanishes here — mid-flight, with a decision outstanding.
    drop(actor);

    let state = fold(&store.all());
    assert_eq!(
        state.phase, phase_before,
        "the fold lost the pending decision"
    );
    // `Gating` is the parked phase: a decision is outstanding and no result exists. The point is
    // that the fold reports the session as mid-flight rather than finished — inventing an outcome
    // for the outstanding call is the failure this guards against.
    assert_eq!(
        state.phase,
        Phase::Gating,
        "a mid-flight session folded wrong"
    );
    assert_eq!(state.pending_permissions.len(), 1);
    assert!(
        state.inflight_calls.is_empty(),
        "a parked call must not be counted as dispatched"
    );
}

#[test]
fn every_session_has_a_home_while_any_node_is_alive() {
    // The invariant a load balancer needs: as long as one node is up, no session is homeless.
    let mut ring = Ring::with_nodes(128, &["a", "b", "c", "d"]);
    let sessions: Vec<String> = (0..200).map(|i| format!("s-{i}")).collect();

    for dead in ["a", "b", "c"] {
        drain(&mut ring, &NodeId(dead.into()), &sessions);
        for session in &sessions {
            assert!(
                ring.place(session).is_some(),
                "`{session}` lost its home while `d` was still up"
            );
        }
    }
    assert_eq!(ring.nodes(), vec![NodeId("d".into())]);
}
