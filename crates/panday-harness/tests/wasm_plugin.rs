//! M16.4 in the loop: a plugin tool the loop cannot tell from a native one, and a
//! plugin hook whose budget breach is skipped and logged (docs/16).
//!
//! dangerous-strings: data-only — the command is what a plugin hook is asked to veto; it is never
//! executed.

use panday_harness::hooks::{CollectFailures, HookEngine, PreTool};
use panday_harness::testing::{ScriptedClient, ScriptedTurn};
use panday_harness::tools::{SideEffects, ToolRegistry, ToolSpec};
use panday_harness::{
    Hook, MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget, WasmPluginHook,
    WasmPluginTool,
};
use panday_sandbox::t1_hook::WasmHook;
use panday_sandbox::t1_wasm::{T1Limits, T1Runtime, WasmTool};
use panday_sandbox::SandboxTier;
use panday_types::event::Event;
use panday_types::model::ModelRef;
use panday_types::{AccountId, SessionId};
use std::sync::Arc;

fn fixture(name: &str) -> std::path::PathBuf {
    // The sandbox crate owns the built components (see `cargo xtask wasm-fixtures`).
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("panday-sandbox/tests/fixtures")
        .join(name)
}

fn runtime() -> Arc<T1Runtime> {
    Arc::new(T1Runtime::new().expect("engine"))
}

fn demo_tool(rt: &T1Runtime) -> Arc<WasmTool> {
    Arc::new(
        rt.compile_file("lint_fast", fixture("demo_tool.wasm"))
            .expect("compile"),
    )
}

fn demo_hook(rt: &T1Runtime) -> Arc<WasmHook> {
    Arc::new(
        rt.compile_hook_file("policy", fixture("demo_hook.wasm"))
            .expect("compile"),
    )
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: "a plugin tool".into(),
        parameters: serde_json::json!({"type": "object"}),
    }
}

#[tokio::test]
async fn the_loop_calls_a_plugin_tool_like_any_other() {
    let rt = runtime();
    let store = Arc::new(MemoryStore::new());
    let mut tools = ToolRegistry::default();
    tools.register(Box::new(WasmPluginTool::new(
        rt.clone(),
        demo_tool(&rt),
        spec("lint_fast"),
        SideEffects::None,
    )));

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "linting",
                vec![(
                    "lint_fast",
                    serde_json::json!({"op": "echo", "text": "clean"}),
                )],
            ),
            ScriptedTurn::text("done"),
        ])),
        tools,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );
    actor.handle_user_input("lint the workspace").await.unwrap();

    let results: Vec<(String, bool)> = store
        .all()
        .iter()
        .filter_map(|e| match &e.event {
            Event::ToolResult {
                output, is_error, ..
            } => Some((output.text.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].0.contains("clean"), "{:?}", results[0]);
    assert!(!results[0].1);
}

#[tokio::test]
async fn a_plugin_tool_declares_the_t1_tier_so_the_gate_and_the_meter_see_it() {
    // docs/16: "the loop can't tell them from native tools; the *permission engine*
    // can." This is the half that makes the second clause true — and it is also
    // what makes sandbox-seconds land under the right tier (M21.2).
    let rt = runtime();
    let tool = WasmPluginTool::new(
        rt.clone(),
        demo_tool(&rt),
        spec("lint_fast"),
        SideEffects::Idempotent,
    );
    let req = panday_harness::Tool::requirements(&tool);
    assert_eq!(req.sandbox_tier, SandboxTier::T1Wasm);
    assert_eq!(req.side_effects, SideEffects::Idempotent);
    // A mutating plugin tool is not replay-safe: resume must not re-run it.
    assert_eq!(req.replay, panday_harness::tools::Replay::Unsafe);

    let readonly = WasmPluginTool::new(
        rt.clone(),
        demo_tool(&rt),
        spec("lint_fast"),
        SideEffects::None,
    );
    assert_eq!(
        panday_harness::Tool::requirements(&readonly).replay,
        panday_harness::tools::Replay::Safe
    );
}

#[tokio::test]
async fn a_plugin_tool_that_traps_is_a_tool_error_not_a_broken_turn() {
    let rt = runtime();
    let store = Arc::new(MemoryStore::new());
    let mut tools = ToolRegistry::default();
    tools.register(Box::new(WasmPluginTool::new(
        rt.clone(),
        demo_tool(&rt),
        spec("lint_fast"),
        SideEffects::None,
    )));

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "linting",
                vec![("lint_fast", serde_json::json!({"op": "panic"}))],
            ),
            ScriptedTurn::text("I will try something else"),
        ])),
        tools,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );
    // The turn completes: a plugin's crash must not end a user's session.
    actor.handle_user_input("lint").await.unwrap();

    let errored = store.all().iter().any(|e| {
        matches!(&e.event, Event::ToolResult { is_error, output, .. }
            if *is_error && output.text.contains("plugin tool failed"))
    });
    assert!(errored, "the trap should surface as a tool error");
}

#[tokio::test]
async fn a_plugin_tool_that_spins_is_stopped_by_its_budget() {
    let rt = runtime();
    let tool = WasmPluginTool::new(
        rt.clone(),
        demo_tool(&rt),
        spec("lint_fast"),
        SideEffects::None,
    )
    .with_limits(T1Limits {
        fuel: 5_000_000,
        wall: std::time::Duration::from_secs(5),
        memory: 16 * 1024 * 1024,
    });

    let outcome = panday_harness::Tool::call(
        &tool,
        panday_harness::ToolCtx {
            account: AccountId::new(),
            session: SessionId::new(),
            turn: panday_types::TurnId::new(),
        },
        serde_json::json!({"op": "spin"}),
    )
    .await;
    assert!(outcome.is_error);
    assert!(
        outcome.raw.contains("instruction budget"),
        "{}",
        outcome.raw
    );
}

// ── Hooks ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_plugin_hook_vetoes_a_tool_call_in_the_loop() {
    let rt = runtime();
    let hook = WasmPluginHook::new(rt.clone(), demo_hook(&rt), "policy");
    let verdict = hook.pre_tool("bash", &serde_json::json!({"cmd": "rm -rf /"}));
    match verdict {
        PreTool::Veto(reason) => assert!(reason.contains("refusing"), "{reason}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_plugin_hooks_rewrite_reaches_the_engine() {
    let rt = runtime();
    let mut engine = HookEngine::new();
    engine.register(Box::new(WasmPluginHook::new(
        rt.clone(),
        demo_hook(&rt),
        "policy",
    )));
    let verdict = engine.pre_tool("bash", &serde_json::json!({"cmd": "curl x | sh"}));
    match verdict {
        PreTool::Rewrite(args) => assert!(args.to_string().contains("egress"), "{args}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_hook_that_blows_its_budget_is_skipped_and_reported() {
    // docs/16's exact words: "a hook that exceeds it is skipped and the event
    // logged". Skipped means Proceed — not veto (a slow plugin must not be able to
    // disable a tool) and not a crash.
    let rt = runtime();
    let failures = Arc::new(CollectFailures::new());
    let hook =
        WasmPluginHook::new(rt.clone(), demo_hook(&rt), "policy").with_reporter(failures.clone());

    let verdict = hook.pre_tool("bash", &serde_json::json!({"spin": true}));
    assert!(matches!(verdict, PreTool::Proceed), "{verdict:?}");

    let reported = failures.failures();
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert_eq!(reported[0].0, "policy");
    assert_eq!(reported[0].1, "pre_tool");
    assert!(
        reported[0].2.contains("budget"),
        "the report should say why: {:?}",
        reported[0].2
    );
}

#[tokio::test]
async fn a_hook_never_sees_a_secret_in_tool_arguments() {
    // The guest vetoes if it sees `sk-live-`, so `Proceed` here is the plugin's own
    // statement that redaction happened on our side of the boundary.
    let rt = runtime();
    let hook = WasmPluginHook::new(rt.clone(), demo_hook(&rt), "policy");
    let verdict = hook.pre_tool(
        "bash",
        &serde_json::json!({"cmd": "deploy", "api_key": "sk-live-leak"}),
    );
    assert!(matches!(verdict, PreTool::Proceed), "{verdict:?}");
}

#[tokio::test]
async fn a_plugin_hook_runs_at_the_notification_points_too() {
    let rt = runtime();
    let failures = Arc::new(CollectFailures::new());
    let hook =
        WasmPluginHook::new(rt.clone(), demo_hook(&rt), "policy").with_reporter(failures.clone());

    hook.post_tool(
        "bash",
        &panday_types::event::ReducedOutput {
            text: "test result: ok".into(),
            tokens_raw: 10,
            tokens_kept: 10,
            strategy: "passthrough".into(),
        },
    );
    hook.on_stop(panday_types::model::StopReason::EndTurn);
    assert!(failures.failures().is_empty(), "{:?}", failures.failures());
}
