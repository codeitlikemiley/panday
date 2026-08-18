//! The turn state machine, driven by a scripted model (M13.1).
//!
//! docs/13 acceptance: "the harness suite runs **without network** using the
//! fake client; every invariant above has a named test." Nothing here opens a
//! socket or starts a model.
//!
//! Regenerate the golden log after an intentional change:
//! ```text
//! PANDAY_UPDATE_GOLDEN=1 cargo test -p panday-harness --test loop
//! ```

use panday_harness::actor::MemoryStore;
use panday_harness::testing::{render_log, EchoTool, ScriptedClient, ScriptedTurn};
use panday_harness::tools::ToolRegistry;
use panday_harness::{
    fold, CollectSink, EventStore, PermissionEngine, Phase, Profile, SessionActor, TurnBudget,
    TurnOutcome,
};
use panday_types::event::Event;
use panday_types::model::{ModelRef, StopReason};
use panday_types::{AccountId, SessionId};
use std::sync::Arc;

fn registry(tools: Vec<Box<dyn panday_harness::tools::Tool>>) -> ToolRegistry {
    let mut r = ToolRegistry::default();
    for t in tools {
        r.register(t);
    }
    r
}

struct Harness {
    actor: SessionActor,
    store: Arc<MemoryStore>,
    sink: Arc<CollectSink>,
    client: Arc<ScriptedClient>,
}

fn build(
    script: Vec<ScriptedTurn>,
    tools: Vec<Box<dyn panday_harness::tools::Tool>>,
    profile: Profile,
    budget: TurnBudget,
) -> Harness {
    let store = Arc::new(MemoryStore::new());
    let client = Arc::new(ScriptedClient::new(script));
    let sink = Arc::new(CollectSink::new());

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        client.clone(),
        registry(tools),
        PermissionEngine::new(profile),
        Box::new(panday_reducer::GenericReducer::default()),
        budget,
    );
    actor.subscribe(sink.clone());

    Harness {
        actor,
        store,
        sink,
        client,
    }
}

// ---------------------------------------------------------------------------
// The golden log — a scripted multi-tool loop, end to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scripted_multi_tool_loop_matches_the_golden_log() {
    // Note on the shape: both `EchoTool`s declare `independent`, so M13.6 runs
    // them concurrently — which is why both `tool_call` events appear before
    // either `tool_result`. That is the honest record of what happened: both
    // were dispatched before either finished.
    let mut h = build(
        vec![
            ScriptedTurn::calling(
                "Let me look.",
                vec![
                    ("read_file", serde_json::json!({"path": "src/lib.rs"})),
                    ("bash", serde_json::json!({"cmd": "cargo test"})),
                ],
            ),
            ScriptedTurn::calling(
                "One test fails; checking git.",
                vec![("bash", serde_json::json!({"cmd": "git diff"}))],
            ),
            ScriptedTurn::text("The assertion compares stale state."),
        ],
        vec![
            EchoTool::ok("read_file", "pub fn add(a: i32, b: i32) -> i32 { a - b }"),
            EchoTool::failing("bash", "test result: FAILED. 1 failed"),
        ],
        Profile::Dev,
        TurnBudget::default(),
    );

    let outcome = h
        .actor
        .handle_user_input("why does the test fail?")
        .await
        .unwrap();
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));

    let rendered = render_log(&h.store.all());
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/multi_tool_loop.jsonl");

    if std::env::var_os("PANDAY_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        rendered, expected,
        "the turn loop's event log changed — review it, then regenerate with \
         PANDAY_UPDATE_GOLDEN=1"
    );
}

// ---------------------------------------------------------------------------
// Event-sourcing invariants (ADR-002, docs/03)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deltas_stream_to_clients_but_never_reach_the_log() {
    // docs/03: "Deltas are ephemeral; messages are durable." A persisted
    // delta would make replay non-deterministic and double-count text.
    let mut h = build(
        vec![ScriptedTurn::text("hello there")],
        vec![],
        Profile::Dev,
        TurnBudget::default(),
    );
    h.actor.handle_user_input("hi").await.unwrap();

    let logged = h.store.all();
    assert!(
        !logged
            .iter()
            .any(|e| matches!(e.event, Event::AssistantDelta { .. })),
        "a delta was persisted"
    );

    let streamed = h.sink.snapshot();
    assert!(
        streamed
            .iter()
            .any(|e| matches!(e.event, Event::AssistantDelta { .. })),
        "clients received no deltas — streaming UX is broken"
    );
}

#[tokio::test]
async fn the_actors_state_equals_a_fold_of_its_own_log() {
    // The load-bearing claim of ADR-002: state is a fold, not a parallel
    // truth. If these diverge, resume and audit are both lying.
    let mut h = build(
        vec![
            ScriptedTurn::calling("look", vec![("read_file", serde_json::json!({}))]),
            ScriptedTurn::text("done"),
        ],
        vec![EchoTool::ok("read_file", "contents")],
        Profile::Dev,
        TurnBudget::default(),
    );
    h.actor.handle_user_input("go").await.unwrap();

    let folded = fold(&h.store.all());
    let live = h.actor.state();
    assert_eq!(live.phase, folded.phase);
    assert_eq!(live.last_seq, folded.last_seq);
    assert_eq!(live.finished_turns, folded.finished_turns);
    assert_eq!(live.usage_total, folded.usage_total);
    assert_eq!(live.inflight_calls.len(), folded.inflight_calls.len());
}

#[tokio::test]
async fn seq_is_gapless_and_starts_at_one() {
    let mut h = build(
        vec![
            ScriptedTurn::calling("t", vec![("read_file", serde_json::json!({}))]),
            ScriptedTurn::text("done"),
        ],
        vec![EchoTool::ok("read_file", "x")],
        Profile::Dev,
        TurnBudget::default(),
    );
    h.actor.handle_user_input("go").await.unwrap();

    let seqs: Vec<u64> = h.store.all().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
}

#[tokio::test]
async fn resume_rebuilds_state_from_the_log_alone() {
    let mut h = build(
        vec![
            ScriptedTurn::calling("t", vec![("read_file", serde_json::json!({}))]),
            ScriptedTurn::text("done"),
        ],
        vec![EchoTool::ok("read_file", "x")],
        Profile::Dev,
        TurnBudget::default(),
    );
    h.actor.handle_user_input("go").await.unwrap();
    let before = h.actor.state().clone();

    // A fresh actor over the same store must reconstruct the same state.
    let mut revived = SessionActor::new(
        h.store.all()[0].session_id,
        AccountId::new(),
        ModelRef("local/test".into()),
        h.store.clone(),
        Arc::new(ScriptedClient::new(vec![])),
        ToolRegistry::default(),
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );
    revived.resume().await.unwrap();

    assert_eq!(revived.state().last_seq, before.last_seq);
    assert_eq!(revived.state().phase, before.phase);
    assert_eq!(revived.state().usage_total, before.usage_total);
}

// ---------------------------------------------------------------------------
// Bounds: exceeding one is a NORMAL stop, never a panic (docs/13)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_steps_stops_the_loop_normally() {
    // A model that always calls a tool would otherwise loop forever.
    let script: Vec<ScriptedTurn> = (0..10)
        .map(|_| ScriptedTurn::calling("again", vec![("read_file", serde_json::json!({}))]))
        .collect();

    let mut h = build(
        script,
        vec![EchoTool::ok("read_file", "x")],
        Profile::Dev,
        TurnBudget {
            max_steps: 3,
            ..Default::default()
        },
    );

    let outcome = h.actor.handle_user_input("go").await.unwrap();
    assert_eq!(
        outcome,
        TurnOutcome::Finished(StopReason::MaxSteps),
        "hitting the step ceiling must be a stop reason, not an error"
    );
    assert_eq!(h.actor.state().phase, Phase::Idle);
    // The turn is closed in the log, so a resumed session is not stuck.
    assert!(h
        .store
        .all()
        .iter()
        .any(|e| matches!(e.event, Event::TurnFinished { .. })));
}

#[tokio::test]
async fn a_wall_clock_ceiling_of_zero_stops_before_calling_the_model() {
    let mut h = build(
        vec![ScriptedTurn::text("never reached")],
        vec![],
        Profile::Dev,
        TurnBudget {
            max_wall_ms: 0,
            ..Default::default()
        },
    );
    let outcome = h.actor.handle_user_input("go").await.unwrap();
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::BudgetExceeded));
    assert_eq!(
        h.client.remaining(),
        1,
        "the model must not have been called"
    );
}

// ---------------------------------------------------------------------------
// Gating (docs/13 §the turn state machine)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_denied_call_becomes_an_error_observation_and_the_loop_continues() {
    // Denial is an observation the model must see, not a hard failure — it
    // needs to learn it was refused and try something else.
    let mut h = build(
        vec![
            ScriptedTurn::calling("push it", vec![("git_push", serde_json::json!({}))]),
            ScriptedTurn::text("understood, I will not push"),
        ],
        // Mutating, so `read_only` denies it. (`EchoTool::ok` has no side
        // effects and would be *allowed* even under read_only.)
        vec![EchoTool::mutating("git_push", "pushed")],
        Profile::ReadOnly,
        TurnBudget::default(),
    );

    let outcome = h.actor.handle_user_input("push").await.unwrap();
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));

    let denied = h
        .store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolResult {
                output, is_error, ..
            } if is_error => Some(output),
            _ => None,
        })
        .expect("a denial must be logged as a tool result");
    assert_eq!(denied.strategy, "permission_denied");
    assert!(denied.text.contains("git_push"));
}

#[tokio::test]
async fn an_irreversible_tool_parks_the_turn_instead_of_running() {
    // docs/13: `side_effects: irreversible` forces Ask regardless of profile.
    let mut h = build(
        vec![ScriptedTurn::calling(
            "deleting",
            vec![("drop_database", serde_json::json!({"name": "prod"}))],
        )],
        vec![EchoTool::irreversible("drop_database", "dropped")],
        // Even unleashed must still gate irreversible actions.
        Profile::Unleashed,
        TurnBudget::default(),
    );

    let outcome = h.actor.handle_user_input("drop it").await.unwrap();
    match outcome {
        TurnOutcome::AwaitingPermission(ids) => assert_eq!(ids.len(), 1),
        other => panic!("expected a permission park, got {other:?}"),
    }

    let log = h.store.all();
    assert!(
        log.iter()
            .any(|e| matches!(e.event, Event::PermissionRequest { .. })),
        "the park must be recorded so a reconnecting client can answer it"
    );
    assert!(
        !log.iter()
            .any(|e| matches!(e.event, Event::ToolResult { .. })),
        "a parked call must NOT have executed"
    );
    assert_eq!(h.actor.state().phase, Phase::Gating);
}

#[tokio::test]
async fn a_parked_call_is_not_in_the_crash_replay_set() {
    let mut h = build(
        vec![ScriptedTurn::calling(
            "x",
            vec![("drop_database", serde_json::json!({}))],
        )],
        vec![EchoTool::irreversible("drop_database", "dropped")],
        Profile::Unleashed,
        TurnBudget::default(),
    );
    h.actor.handle_user_input("go").await.unwrap();

    let s = fold(&h.store.all());
    assert!(
        s.inflight_calls.is_empty(),
        "resume must not replay a call nobody approved"
    );
    assert_eq!(s.pending_permissions.len(), 1);
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_output_passes_through_the_reducer_before_context() {
    // ADR-007: "Tool output never enters context raw."
    let huge = (0..400)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut h = build(
        vec![
            ScriptedTurn::calling("run", vec![("bash", serde_json::json!({}))]),
            ScriptedTurn::text("done"),
        ],
        vec![EchoTool::ok("bash", &huge)],
        Profile::Dev,
        TurnBudget::default(),
    );
    h.actor.handle_user_input("go").await.unwrap();

    let out = h
        .store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();

    assert!(out.tokens_kept < out.tokens_raw, "output was not reduced");
    assert_ne!(out.strategy, "", "the strategy must be recorded for audit");
    assert!(
        out.text.len() < huge.len(),
        "the reduced text is not smaller than the raw"
    );
}

#[tokio::test]
async fn an_unknown_tool_is_a_tool_error_not_a_crash() {
    // Models hallucinate tool names; that is a correctable observation.
    let mut h = build(
        vec![
            ScriptedTurn::calling("try", vec![("nonexistent", serde_json::json!({}))]),
            ScriptedTurn::text("ok, that tool does not exist"),
        ],
        vec![],
        Profile::Dev,
        TurnBudget::default(),
    );

    let outcome = h.actor.handle_user_input("go").await.unwrap();
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));

    let err = h
        .store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolResult {
                output, is_error, ..
            } if is_error => Some(output.text),
            _ => None,
        })
        .expect("the failure must reach the model as an observation");
    assert!(err.contains("nonexistent"), "{err}");
}

#[tokio::test]
async fn the_provider_call_id_survives_into_the_log() {
    // Needed to answer the provider on the next turn (docs/03, M11.2).
    let mut h = build(
        vec![
            ScriptedTurn::calling("t", vec![("read_file", serde_json::json!({}))]),
            ScriptedTurn::text("done"),
        ],
        vec![EchoTool::ok("read_file", "x")],
        Profile::Dev,
        TurnBudget::default(),
    );
    h.actor.handle_user_input("go").await.unwrap();

    let pid = h
        .store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolCall {
                provider_call_id, ..
            } => provider_call_id,
            _ => None,
        })
        .expect("provider id must be recorded");
    assert_eq!(pid, "call_scripted_0");
}

#[tokio::test]
async fn observations_are_fed_back_to_the_model_on_the_next_step() {
    // The loop is only a loop if the model sees what the tools returned.
    let mut h = build(
        vec![
            ScriptedTurn::calling("look", vec![("read_file", serde_json::json!({}))]),
            ScriptedTurn::text("I see it"),
        ],
        vec![EchoTool::ok("read_file", "SENTINEL_CONTENT")],
        Profile::Dev,
        TurnBudget::default(),
    );
    h.actor.handle_user_input("go").await.unwrap();

    let second = &h.client.requests()[1];
    let sent: String = second
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            panday_types::model::ContentBlock::Text { text } => text.clone(),
            panday_types::model::ContentBlock::ToolOutput { text, .. } => text.clone(),
            panday_types::model::ContentBlock::Artifact { summary, .. } => summary.clone(),
        })
        .collect();
    assert!(
        sent.contains("SENTINEL_CONTENT"),
        "the tool observation never reached the model: {sent}"
    );
}

#[tokio::test]
async fn tool_schemas_are_sent_with_every_request() {
    // They are part of the stable prefix (ADR-008); omitting them mid-session
    // would both break the cache and hide the toolset from the model.
    let mut h = build(
        vec![ScriptedTurn::text("hi")],
        vec![EchoTool::ok("read_file", "x"), EchoTool::ok("bash", "y")],
        Profile::Dev,
        TurnBudget::default(),
    );
    h.actor.handle_user_input("go").await.unwrap();

    let names: Vec<String> = h.client.requests()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(names, vec!["read_file", "bash"]);
}

// ---------------------------------------------------------------------------
// Persist-before-proceed (docs/13 §invariants)
// ---------------------------------------------------------------------------

/// A store that fails after N successful appends.
struct FlakyStore {
    inner: MemoryStore,
    fail_after: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl EventStore for FlakyStore {
    async fn append(
        &self,
        e: panday_types::event::Envelope,
    ) -> Result<(), panday_harness::StoreError> {
        use std::sync::atomic::Ordering;
        if self.fail_after.load(Ordering::SeqCst) == 0 {
            return Err(panday_harness::StoreError::Io("disk gone".into()));
        }
        self.fail_after.fetch_sub(1, Ordering::SeqCst);
        self.inner.append(e).await
    }
    async fn read_after(
        &self,
        s: SessionId,
        after: u64,
    ) -> Result<Vec<panday_types::event::Envelope>, panday_harness::StoreError> {
        self.inner.read_after(s, after).await
    }
    async fn next_seq(&self, s: SessionId) -> Result<u64, panday_harness::StoreError> {
        self.inner.next_seq(s).await
    }
}

#[tokio::test]
async fn a_store_failure_aborts_the_turn_rather_than_proceeding_blind() {
    // Continuing past a failed append would advance against a log that does
    // not describe reality — the one thing event sourcing must never do.
    let store = Arc::new(FlakyStore {
        inner: MemoryStore::new(),
        fail_after: std::sync::atomic::AtomicUsize::new(2),
    });

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![ScriptedTurn::text("hi")])),
        ToolRegistry::default(),
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );

    let err = actor.handle_user_input("go").await.unwrap_err();
    assert!(
        matches!(err, panday_harness::HarnessError::Store(_)),
        "expected a store error, got {err}"
    );
}
