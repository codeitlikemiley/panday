//! M13.7 — the hook engine, with the `pre_tool` veto demonstrated.
//!
//! docs/13: "Hook misbehavior (timeout, panic) is contained: log, skip,
//! continue." A hook is other people's code running inside our loop, so the
//! tests here are mostly about the loop *surviving* hooks rather than about
//! hooks working.

use panday_harness::hooks::{CollectFailures, Hook, HookEngine, PreTool};
use panday_harness::testing::{EchoTool, ScriptedClient, ScriptedTurn};
use panday_harness::tools::ToolRegistry;
use panday_harness::{
    MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget, TurnOutcome,
};
use panday_types::event::{Event, ReducedOutput};
use panday_types::model::{ChatRequest, ModelRef, StopReason};
use panday_types::{AccountId, Json, SessionId};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Records which points fired, in order.
struct Recorder {
    seen: Arc<Mutex<Vec<String>>>,
}

impl Hook for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }
    fn pre_turn(&self) {
        self.seen.lock().unwrap().push("pre_turn".into());
    }
    fn pre_model(&self, _req: &mut ChatRequest) {
        self.seen.lock().unwrap().push("pre_model".into());
    }
    fn post_model(&self, _text: &str) {
        self.seen.lock().unwrap().push("post_model".into());
    }
    fn pre_tool(&self, tool: &str, _args: &Json) -> PreTool {
        self.seen.lock().unwrap().push(format!("pre_tool:{tool}"));
        PreTool::Proceed
    }
    fn post_tool(&self, tool: &str, _o: &ReducedOutput) {
        self.seen.lock().unwrap().push(format!("post_tool:{tool}"));
    }
    fn on_stop(&self, reason: StopReason) {
        self.seen
            .lock()
            .unwrap()
            .push(format!("on_stop:{reason:?}"));
    }
}

/// Refuses anything matching a substring — the DLP shape docs/13 describes.
struct Vetoer(&'static str);

impl Hook for Vetoer {
    fn name(&self) -> &str {
        "vetoer"
    }
    fn pre_tool(&self, _tool: &str, args: &Json) -> PreTool {
        if args.to_string().contains(self.0) {
            PreTool::Veto(format!("arguments contain `{}`", self.0))
        } else {
            PreTool::Proceed
        }
    }
}

/// Redacts a field — the command-rewrite shape.
struct Redactor {
    field: &'static str,
    replacement: &'static str,
}

impl Hook for Redactor {
    fn name(&self) -> &str {
        "redactor"
    }
    fn pre_tool(&self, _tool: &str, args: &Json) -> PreTool {
        let Some(obj) = args.as_object() else {
            return PreTool::Proceed;
        };
        if !obj.contains_key(self.field) {
            return PreTool::Proceed;
        }
        let mut next = obj.clone();
        next.insert(
            self.field.into(),
            Json::String(self.replacement.to_string()),
        );
        PreTool::Rewrite(Json::Object(next))
    }
}

/// Panics at a chosen point.
struct Exploder(&'static str);

impl Hook for Exploder {
    fn name(&self) -> &str {
        "exploder"
    }
    fn pre_turn(&self) {
        if self.0 == "pre_turn" {
            panic!("boom in pre_turn");
        }
    }
    fn pre_tool(&self, _t: &str, _a: &Json) -> PreTool {
        if self.0 == "pre_tool" {
            panic!("boom in pre_tool");
        }
        PreTool::Proceed
    }
    fn post_tool(&self, _t: &str, _o: &ReducedOutput) {
        if self.0 == "post_tool" {
            panic!("boom in post_tool");
        }
    }
}

/// Counts calls, to prove a chain stopped.
struct Counter(Arc<AtomicUsize>);

impl Hook for Counter {
    fn name(&self) -> &str {
        "counter"
    }
    fn pre_tool(&self, _t: &str, _a: &Json) -> PreTool {
        self.0.fetch_add(1, Ordering::SeqCst);
        PreTool::Proceed
    }
}

fn actor(script: Vec<ScriptedTurn>, hooks: HookEngine) -> (SessionActor, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new());
    let mut reg = ToolRegistry::default();
    reg.register(EchoTool::ok("read_file", "file contents"));
    reg.register(EchoTool::ok("bash", "command output"));

    let actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(script)),
        reg,
        PermissionEngine::new(Profile::Unleashed),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    )
    .with_hooks(hooks);
    (actor, store)
}

fn one_tool_turn() -> Vec<ScriptedTurn> {
    vec![
        ScriptedTurn::calling(
            "reading",
            vec![("read_file", serde_json::json!({"path": "src/lib.rs"}))],
        ),
        ScriptedTurn::text("done"),
    ]
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_extension_point_fires_in_order() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut hooks = HookEngine::new();
    hooks.register(Box::new(Recorder { seen: seen.clone() }));

    let (mut a, _store) = actor(one_tool_turn(), hooks);
    a.handle_user_input("go").await.unwrap();

    let order = seen.lock().unwrap().clone();
    assert_eq!(order.first().map(String::as_str), Some("pre_turn"));
    assert!(order.contains(&"pre_model".to_string()));
    assert!(order.contains(&"post_model".to_string()));
    assert!(order.contains(&"pre_tool:read_file".to_string()));
    assert!(order.contains(&"post_tool:read_file".to_string()));
    assert!(
        order.last().is_some_and(|l| l.starts_with("on_stop")),
        "on_stop must be last: {order:?}"
    );

    // pre_tool must precede post_tool for the same call.
    let pre = order
        .iter()
        .position(|s| s == "pre_tool:read_file")
        .unwrap();
    let post = order
        .iter()
        .position(|s| s == "post_tool:read_file")
        .unwrap();
    assert!(pre < post);
}

#[tokio::test]
async fn a_pre_tool_veto_stops_the_call_and_tells_the_model_why() {
    // The demonstration M13.7 names.
    let mut hooks = HookEngine::new();
    hooks.register(Box::new(Vetoer("secrets")));

    let (mut a, store) = actor(
        vec![
            ScriptedTurn::calling(
                "reading",
                vec![("read_file", serde_json::json!({"path": "secrets.env"}))],
            ),
            ScriptedTurn::text("understood, I will not read that"),
        ],
        hooks,
    );

    let outcome = a.handle_user_input("go").await.unwrap();
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));

    let result = store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolResult {
                output, is_error, ..
            } if is_error => Some(output),
            _ => None,
        })
        .expect("the veto must be recorded");
    assert_eq!(result.strategy, "hook_veto");
    assert!(result.text.contains("secrets"), "{}", result.text);
    assert!(
        !store.all().iter().any(|e| matches!(
            &e.event,
            Event::ToolResult { output, .. } if output.text.contains("file contents")
        )),
        "the vetoed tool must NOT have run"
    );
}

#[tokio::test]
async fn a_rewrite_changes_what_the_tool_actually_receives() {
    // The DLP / command-rewrite path: the tool sees redacted arguments, and so
    // does the log, so nothing downstream can recover the original.
    let mut hooks = HookEngine::new();
    hooks.register(Box::new(Redactor {
        field: "path",
        replacement: "[redacted]",
    }));

    let (mut a, store) = actor(one_tool_turn(), hooks);
    a.handle_user_input("go").await.unwrap();

    let args = store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolCall { args, .. } => Some(args),
            _ => None,
        })
        .unwrap();
    assert_eq!(args["path"], "[redacted]");
}

#[tokio::test]
async fn rewrites_compose_in_order() {
    // Two hooks each redacting a different field must both take effect —
    // otherwise only the last one registered would matter.
    let mut hooks = HookEngine::new();
    hooks.register(Box::new(Redactor {
        field: "path",
        replacement: "[path]",
    }));
    hooks.register(Box::new(Redactor {
        field: "cmd",
        replacement: "[cmd]",
    }));

    let (mut a, store) = actor(
        vec![
            ScriptedTurn::calling(
                "both",
                vec![(
                    "bash",
                    serde_json::json!({"path": "/etc/passwd", "cmd": "cat /etc/passwd"}),
                )],
            ),
            ScriptedTurn::text("done"),
        ],
        hooks,
    );
    a.handle_user_input("go").await.unwrap();

    let args = store
        .all()
        .into_iter()
        .find_map(|e| match e.event {
            Event::ToolCall { args, .. } => Some(args),
            _ => None,
        })
        .unwrap();
    assert_eq!(args["path"], "[path]");
    assert_eq!(args["cmd"], "[cmd]");
}

#[test]
fn the_first_veto_stops_the_chain() {
    // Asking later hooks after a refusal lets a subsequent Rewrite silently
    // override it.
    let calls = Arc::new(AtomicUsize::new(0));
    let mut hooks = HookEngine::new();
    hooks.register(Box::new(Vetoer("nope")));
    hooks.register(Box::new(Counter(calls.clone())));

    let decision = hooks.pre_tool("bash", &serde_json::json!({"cmd": "nope"}));
    assert!(matches!(decision, PreTool::Veto(_)));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "hooks after the veto must not be consulted"
    );
}

// ---------------------------------------------------------------------------
// Containment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_panicking_hook_does_not_kill_the_turn() {
    let failures = Arc::new(CollectFailures::new());
    let seen = Arc::new(Mutex::new(Vec::new()));

    let mut hooks = HookEngine::new().with_reporter(failures.clone());
    hooks.register(Box::new(Exploder("pre_turn")));
    // A later hook must still run — "skip, continue", not "abort".
    hooks.register(Box::new(Recorder { seen: seen.clone() }));

    let (mut a, _store) = actor(one_tool_turn(), hooks);
    let outcome = a.handle_user_input("go").await.unwrap();

    assert_eq!(
        outcome,
        TurnOutcome::Finished(StopReason::EndTurn),
        "one bad hook must not lose the session"
    );
    assert!(
        seen.lock().unwrap().contains(&"pre_turn".to_string()),
        "the surviving hook should still have run"
    );

    let reported = failures.failures();
    assert_eq!(
        reported.len(),
        1,
        "the failure must be reported, not swallowed"
    );
    assert_eq!(reported[0].0, "exploder", "attributed to the culprit");
    assert_eq!(reported[0].1, "pre_turn");
    assert!(reported[0].2.contains("boom"), "{:?}", reported[0]);
}

#[tokio::test]
async fn a_hook_that_panics_in_pre_tool_gets_no_say() {
    // Treating a crash as a veto would let a bug silently disable tools;
    // treating it as approval is equally wrong. It is skipped and reported.
    let failures = Arc::new(CollectFailures::new());
    let mut hooks = HookEngine::new().with_reporter(failures.clone());
    hooks.register(Box::new(Exploder("pre_tool")));

    let (mut a, store) = actor(one_tool_turn(), hooks);
    a.handle_user_input("go").await.unwrap();

    // The tool ran normally.
    assert!(
        store.all().iter().any(|e| matches!(
            &e.event,
            Event::ToolResult { output, .. } if output.text.contains("file contents")
        )),
        "the call should have proceeded"
    );
    assert_eq!(failures.failures().len(), 1);
}

#[tokio::test]
async fn a_panic_in_post_tool_does_not_lose_the_result() {
    // The observation is already computed; a hook failing afterwards must not
    // prevent it reaching the log, or the model would lose work that happened.
    let failures = Arc::new(CollectFailures::new());
    let mut hooks = HookEngine::new().with_reporter(failures.clone());
    hooks.register(Box::new(Exploder("post_tool")));

    let (mut a, store) = actor(one_tool_turn(), hooks);
    a.handle_user_input("go").await.unwrap();

    assert!(
        store.all().iter().any(|e| matches!(
            &e.event,
            Event::ToolResult { output, .. } if output.text.contains("file contents")
        )),
        "the result must still be recorded"
    );
    assert_eq!(failures.failures().len(), 1);
}

#[tokio::test]
async fn no_hooks_is_the_default_and_costs_nothing() {
    let (mut a, store) = actor(one_tool_turn(), HookEngine::new());
    let outcome = a.handle_user_input("go").await.unwrap();
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));
    assert!(HookEngine::new().is_empty());
    assert!(!store.all().is_empty());
}
