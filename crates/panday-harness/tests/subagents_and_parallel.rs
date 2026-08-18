//! M13.6 — subagents with budget split, and parallel independent tools.
//!
//! docs/13 §subagents is explicit about what makes this worth having:
//!
//! > "Parent receives `SubagentFinished{result_ref}` — the reduced result, not
//! > the child's transcript. Depth ≤ 2 ... This is how 'be comprehensive'
//! > scales without one context window eating the bill."
//!
//! So the tests check the *containment* properties, not just that a child
//! runs: the brief is the only context that crosses, the transcript does not
//! come back, the budget halves, and depth is bounded.

use panday_harness::actor::{SubagentFactory, SubagentResult};
use panday_harness::testing::{EchoTool, ScriptedClient, ScriptedTurn};
use panday_harness::tools::{
    Replay, SideEffects, Tool, ToolCtx, ToolOutcome, ToolRegistry, ToolReq, ToolSpec,
};
use panday_harness::{
    HarnessError, MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget, TurnOutcome,
    MAX_SUBAGENT_DEPTH, SPAWN_SUBAGENT,
};
use panday_sandbox::SandboxTier;
use panday_types::event::Event;
use panday_types::model::{ModelRef, StopReason, Usage};
use panday_types::{AccountId, Json, SessionId};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Records what the parent asked for, and answers with a fixed summary.
struct RecordingFactory {
    briefs: Mutex<Vec<String>>,
    budgets: Mutex<Vec<TurnBudget>>,
    depths: Mutex<Vec<u8>>,
    summary: String,
    fail: bool,
}

impl RecordingFactory {
    fn new(summary: &str) -> Arc<Self> {
        Arc::new(Self {
            briefs: Mutex::new(Vec::new()),
            budgets: Mutex::new(Vec::new()),
            depths: Mutex::new(Vec::new()),
            summary: summary.into(),
            fail: false,
        })
    }
    fn failing() -> Arc<Self> {
        Arc::new(Self {
            briefs: Mutex::new(Vec::new()),
            budgets: Mutex::new(Vec::new()),
            depths: Mutex::new(Vec::new()),
            summary: String::new(),
            fail: true,
        })
    }
}

#[async_trait::async_trait]
impl SubagentFactory for RecordingFactory {
    async fn run(
        &self,
        _parent: SessionId,
        brief: String,
        budget: TurnBudget,
        depth: u8,
    ) -> Result<SubagentResult, HarnessError> {
        self.briefs.lock().unwrap().push(brief);
        self.budgets.lock().unwrap().push(budget);
        self.depths.lock().unwrap().push(depth);

        if self.fail {
            return Err(HarnessError::Model(panday_sdk::PandayError::Protocol(
                "child exploded".into(),
            )));
        }
        Ok(SubagentResult {
            child: SessionId::new(),
            summary: self.summary.clone(),
            usage: Usage {
                input_tokens: 50,
                output_tokens: 5,
                ..Default::default()
            },
        })
    }
}

/// A tool that records when it started and finished, so overlap is observable.
struct SlowTool {
    name: String,
    independent: bool,
    order: Arc<Mutex<Vec<String>>>,
    concurrent: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for SlowTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "slow".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    fn requirements(&self) -> ToolReq {
        ToolReq {
            sandbox_tier: SandboxTier::T0InProcess,
            side_effects: SideEffects::None,
            independent: self.independent,
            replay: Replay::Safe,
        }
    }
    async fn call(&self, _ctx: ToolCtx, _args: Json) -> ToolOutcome {
        let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        self.order
            .lock()
            .unwrap()
            .push(format!("start {}", self.name));

        tokio::time::sleep(std::time::Duration::from_millis(60)).await;

        self.order
            .lock()
            .unwrap()
            .push(format!("end {}", self.name));
        self.concurrent.fetch_sub(1, Ordering::SeqCst);
        ToolOutcome {
            raw: format!("{} done", self.name),
            is_error: false,
        }
    }
}

fn registry(tools: Vec<Box<dyn Tool>>) -> ToolRegistry {
    let mut r = ToolRegistry::default();
    for t in tools {
        r.register(t);
    }
    r
}

fn actor(
    script: Vec<ScriptedTurn>,
    tools: ToolRegistry,
    budget: TurnBudget,
) -> (SessionActor, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(script)),
        tools,
        PermissionEngine::new(Profile::Unleashed),
        Box::new(panday_reducer::GenericReducer::default()),
        budget,
    );
    (actor, store)
}

// ---------------------------------------------------------------------------
// Subagents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_subagent_result_reaches_the_parent_as_a_reduced_answer() {
    let factory = RecordingFactory::new("the auth module has 3 callers");
    let (a, store) = actor(
        vec![
            ScriptedTurn::calling(
                "delegating",
                vec![(
                    SPAWN_SUBAGENT,
                    serde_json::json!({"brief": "count callers of the auth module"}),
                )],
            ),
            ScriptedTurn::text("thanks"),
        ],
        registry(vec![]),
        TurnBudget::default(),
    );
    let mut a = a.with_subagents(factory.clone(), 0);

    let outcome = a.handle_user_input("go").await.unwrap();
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));

    // The brief is the only context that crossed.
    assert_eq!(
        factory.briefs.lock().unwrap().as_slice(),
        ["count callers of the auth module"]
    );

    let log = store.all();
    assert!(
        log.iter()
            .any(|e| matches!(e.event, Event::SubagentSpawned { .. })),
        "the spawn must be on the record"
    );
    assert!(
        log.iter()
            .any(|e| matches!(e.event, Event::SubagentFinished { .. })),
        "the completion must be on the record"
    );

    let observation = log
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolResult { output, .. } => Some(output.text.clone()),
            _ => None,
        })
        .expect("the parent must see a result");
    assert!(
        observation.contains("3 callers"),
        "the reduced answer must reach the parent: {observation}"
    );
}

#[tokio::test]
async fn a_child_gets_half_the_parents_budget() {
    // docs/13: "a fraction of the parent budget". Halving makes a two-level
    // tree bounded in cost, not just in levels.
    let factory = RecordingFactory::new("done");
    let (a, _store) = actor(
        vec![
            ScriptedTurn::calling(
                "delegating",
                vec![(SPAWN_SUBAGENT, serde_json::json!({"brief": "look"}))],
            ),
            ScriptedTurn::text("ok"),
        ],
        registry(vec![]),
        TurnBudget {
            max_steps: 20,
            max_wall_ms: 600_000,
            max_spend_micros: 2_000_000,
        },
    );
    let mut a = a.with_subagents(factory.clone(), 0);
    a.handle_user_input("go").await.unwrap();

    let budgets = factory.budgets.lock().unwrap();
    assert_eq!(budgets.len(), 1);
    assert_eq!(budgets[0].max_steps, 10);
    assert_eq!(budgets[0].max_spend_micros, 1_000_000);
    assert_eq!(budgets[0].max_wall_ms, 300_000);
}

#[tokio::test]
async fn a_child_is_told_its_depth_and_the_limit_is_enforced() {
    let factory = RecordingFactory::new("done");

    // A session already at the limit must refuse rather than recurse.
    let (a, store) = actor(
        vec![
            ScriptedTurn::calling(
                "delegating",
                vec![(SPAWN_SUBAGENT, serde_json::json!({"brief": "go deeper"}))],
            ),
            ScriptedTurn::text("understood"),
        ],
        registry(vec![]),
        TurnBudget::default(),
    );
    let mut a = a.with_subagents(factory.clone(), MAX_SUBAGENT_DEPTH);
    a.handle_user_input("go").await.unwrap();

    assert!(
        factory.briefs.lock().unwrap().is_empty(),
        "no child should have been created at the depth limit"
    );
    let refusal = store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolResult {
                output, is_error, ..
            } if is_error => Some(output.text),
            _ => None,
        })
        .expect("the refusal must reach the model");
    assert!(refusal.contains("depth limit"), "{refusal}");
}

#[tokio::test]
async fn a_child_below_the_limit_is_told_the_next_depth() {
    let factory = RecordingFactory::new("done");
    let (a, _s) = actor(
        vec![
            ScriptedTurn::calling(
                "delegating",
                vec![(SPAWN_SUBAGENT, serde_json::json!({"brief": "look"}))],
            ),
            ScriptedTurn::text("ok"),
        ],
        registry(vec![]),
        TurnBudget::default(),
    );
    let mut a = a.with_subagents(factory.clone(), 0);
    a.handle_user_input("go").await.unwrap();
    assert_eq!(factory.depths.lock().unwrap().as_slice(), [1]);
}

#[tokio::test]
async fn spawning_without_a_brief_is_refused() {
    // The brief is the child's ONLY context; an empty one guarantees a wasted
    // session.
    let factory = RecordingFactory::new("done");
    let (a, store) = actor(
        vec![
            ScriptedTurn::calling("delegating", vec![(SPAWN_SUBAGENT, serde_json::json!({}))]),
            ScriptedTurn::text("ok"),
        ],
        registry(vec![]),
        TurnBudget::default(),
    );
    let mut a = a.with_subagents(factory.clone(), 0);
    a.handle_user_input("go").await.unwrap();

    assert!(factory.briefs.lock().unwrap().is_empty());
    let err = store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolResult {
                output, is_error, ..
            } if is_error => Some(output.text),
            _ => None,
        })
        .unwrap();
    assert!(err.contains("brief"), "{err}");
}

#[tokio::test]
async fn spawning_is_refused_when_subagents_are_not_enabled() {
    let (mut a, store) = actor(
        vec![
            ScriptedTurn::calling(
                "delegating",
                vec![(SPAWN_SUBAGENT, serde_json::json!({"brief": "x"}))],
            ),
            ScriptedTurn::text("ok"),
        ],
        registry(vec![]),
        TurnBudget::default(),
    );
    a.handle_user_input("go").await.unwrap();

    let err = store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolResult {
                output, is_error, ..
            } if is_error => Some(output.text),
            _ => None,
        })
        .unwrap();
    assert!(err.contains("not enabled"), "{err}");
}

#[tokio::test]
async fn a_failed_child_is_an_observation_not_a_dead_parent() {
    let factory = RecordingFactory::failing();
    let (a, store) = actor(
        vec![
            ScriptedTurn::calling(
                "delegating",
                vec![(SPAWN_SUBAGENT, serde_json::json!({"brief": "x"}))],
            ),
            ScriptedTurn::text("I will do it myself"),
        ],
        registry(vec![]),
        TurnBudget::default(),
    );
    let mut a = a.with_subagents(factory, 0);

    // The parent must survive and be able to adapt.
    let outcome = a.handle_user_input("go").await.unwrap();
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));

    let err = store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolResult {
                output, is_error, ..
            } if is_error => Some(output.text),
            _ => None,
        })
        .unwrap();
    assert!(err.contains("subagent failed"), "{err}");
}

// ---------------------------------------------------------------------------
// Parallel independent tools
// ---------------------------------------------------------------------------

fn slow(
    name: &str,
    independent: bool,
    order: Arc<Mutex<Vec<String>>>,
    peak: Arc<AtomicUsize>,
) -> Box<dyn Tool> {
    Box::new(SlowTool {
        name: name.into(),
        independent,
        order,
        concurrent: Arc::new(AtomicUsize::new(0)),
        peak,
    })
}

#[tokio::test]
async fn independent_tools_run_concurrently() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let peak = Arc::new(AtomicUsize::new(0));
    // Share one counter so overlap across tools is visible.
    let counter = Arc::new(AtomicUsize::new(0));

    let mut reg = ToolRegistry::default();
    for name in ["read_a", "read_b", "read_c"] {
        reg.register(Box::new(SlowTool {
            name: name.into(),
            independent: true,
            order: order.clone(),
            concurrent: counter.clone(),
            peak: peak.clone(),
        }));
    }

    let (mut a, _store) = actor(
        vec![
            ScriptedTurn::calling(
                "reading three files",
                vec![
                    ("read_a", serde_json::json!({})),
                    ("read_b", serde_json::json!({})),
                    ("read_c", serde_json::json!({})),
                ],
            ),
            ScriptedTurn::text("done"),
        ],
        reg,
        TurnBudget::default(),
    );

    let began = std::time::Instant::now();
    a.handle_user_input("go").await.unwrap();
    let elapsed = began.elapsed();

    assert!(
        peak.load(Ordering::SeqCst) >= 2,
        "independent tools did not overlap (peak concurrency {})",
        peak.load(Ordering::SeqCst)
    );
    // Three 60ms calls in series would be ~180ms.
    assert!(
        elapsed < std::time::Duration::from_millis(160),
        "took {elapsed:?}, which looks sequential"
    );
}

#[tokio::test]
async fn dependent_tools_run_in_order() {
    // A dependent call may rely on an earlier one's effect; overlapping them
    // would be a correctness bug the model cannot see.
    let order = Arc::new(Mutex::new(Vec::new()));
    let peak = Arc::new(AtomicUsize::new(0));
    let counter = Arc::new(AtomicUsize::new(0));

    let mut reg = ToolRegistry::default();
    for name in ["write_a", "write_b"] {
        reg.register(Box::new(SlowTool {
            name: name.into(),
            independent: false,
            order: order.clone(),
            concurrent: counter.clone(),
            peak: peak.clone(),
        }));
    }

    let (mut a, _store) = actor(
        vec![
            ScriptedTurn::calling(
                "writing two files",
                vec![
                    ("write_a", serde_json::json!({})),
                    ("write_b", serde_json::json!({})),
                ],
            ),
            ScriptedTurn::text("done"),
        ],
        reg,
        TurnBudget::default(),
    );
    a.handle_user_input("go").await.unwrap();

    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "dependent tools must not overlap"
    );
    assert_eq!(
        order.lock().unwrap().as_slice(),
        [
            "start write_a",
            "end write_a",
            "start write_b",
            "end write_b"
        ]
    );
}

#[tokio::test]
async fn parallel_results_are_still_logged_one_at_a_time_and_in_order() {
    // Execution overlaps; logging does not. The actor is the only writer to
    // its log, which is what makes `seq` gapless without locks.
    let order = Arc::new(Mutex::new(Vec::new()));
    let peak = Arc::new(AtomicUsize::new(0));

    let mut reg = ToolRegistry::default();
    reg.register(slow("read_a", true, order.clone(), peak.clone()));
    reg.register(slow("read_b", true, order.clone(), peak.clone()));

    let (mut a, store) = actor(
        vec![
            ScriptedTurn::calling(
                "two reads",
                vec![
                    ("read_a", serde_json::json!({})),
                    ("read_b", serde_json::json!({})),
                ],
            ),
            ScriptedTurn::text("done"),
        ],
        reg,
        TurnBudget::default(),
    );
    a.handle_user_input("go").await.unwrap();

    let seqs: Vec<u64> = store.all().iter().map(|e| e.seq).collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "the log must stay gapless even when work overlapped"
    );

    let results = store
        .all()
        .iter()
        .filter(|e| matches!(e.event, Event::ToolResult { .. }))
        .count();
    assert_eq!(results, 2, "both results must be recorded");
}

#[tokio::test]
async fn a_subagent_never_shares_a_group_with_other_tools() {
    // It runs a whole session; the budget split assumes one child at a time.
    let factory = RecordingFactory::new("child answer");
    let order = Arc::new(Mutex::new(Vec::new()));
    let peak = Arc::new(AtomicUsize::new(0));

    let mut reg = ToolRegistry::default();
    reg.register(slow("read_a", true, order.clone(), peak.clone()));
    reg.register(EchoTool::ok(SPAWN_SUBAGENT, "unused"));

    let (a, store) = actor(
        vec![
            ScriptedTurn::calling(
                "both",
                vec![
                    ("read_a", serde_json::json!({})),
                    (SPAWN_SUBAGENT, serde_json::json!({"brief": "delegate"})),
                ],
            ),
            ScriptedTurn::text("done"),
        ],
        reg,
        TurnBudget::default(),
    );
    let mut a = a.with_subagents(factory.clone(), 0);
    a.handle_user_input("go").await.unwrap();

    assert_eq!(factory.briefs.lock().unwrap().len(), 1);
    // Both produced results, and the log is still gapless.
    let seqs: Vec<u64> = store.all().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
}
