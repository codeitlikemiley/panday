//! M16.3's acceptance: "mount a public MCP server, call its tool through the loop
//! with Ask-gating" (docs/16 §MCP host).
//!
//! The server is `panday-plugins`' hand-written fixture — a foreign JSON-RPC
//! implementation, so this exercises interop and not self-agreement.

use panday_harness::testing::{ScriptedClient, ScriptedTurn};
use panday_harness::tools::{SideEffects, ToolRegistry};
use panday_harness::{
    McpTool, MemoryStore, PermissionEngine, Profile, SessionActor, Tool, TurnBudget, TurnOutcome,
};
use panday_plugins::mcp::{MountedServer, StdioServer};
use panday_sandbox::SandboxTier;
use panday_types::event::{Actor, Event, PermDecision};
use panday_types::model::ModelRef;
use panday_types::{AccountId, SessionId};
use std::sync::Arc;

/// The fixture binary lives in the workspace target dir. `CARGO_BIN_EXE_` is only
/// set for the crate that declares the bin, so it is located rather than injected —
/// and a missing binary fails loudly with the command to build it, because a test
/// that skipped itself here would stop covering the gate.
fn fixture_server() -> std::path::PathBuf {
    let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target");
    for profile in ["debug", "release"] {
        let candidate = target.join(profile).join("fixture-mcp-server");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "fixture-mcp-server is not built. Run:\n  \
         cargo build -p panday-plugins --bin fixture-mcp-server"
    );
}

async fn mount() -> Arc<MountedServer> {
    let spec = StdioServer::new("github", fixture_server().to_string_lossy().to_string());
    Arc::new(MountedServer::mount(&spec).await.expect("mount"))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_tool_defaults_to_ask_in_every_profile() {
    // docs/16: "MCP tools default to `Ask` until granted." We do not know what a
    // third party's tool does; MCP's own `readOnlyHint` is the *server's* claim about
    // itself, and treating an unverified claim as a permission grant is how a consent
    // model becomes decorative.
    let server = mount().await;
    let tools = McpTool::all(server.clone());
    assert_eq!(tools.len(), server.tools().len());

    let req = tools[0].requirements();
    assert_eq!(req.sandbox_tier, SandboxTier::T2OsJail);
    assert_eq!(req.side_effects, SideEffects::Irreversible);
    // Never replayed after a crash: re-running a `create_issue` reaches a human's
    // inbox.
    assert_eq!(req.replay, panday_harness::tools::Replay::Unsafe);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_loop_parks_on_an_mcp_call_and_runs_it_after_approval() {
    let server = mount().await;
    let store = Arc::new(MemoryStore::new());
    let mut registry = ToolRegistry::default();
    for tool in McpTool::all(server.clone()) {
        registry.register(tool);
    }

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "checking the issues",
                vec![(
                    "mcp:github:list_issues",
                    serde_json::json!({"repo": "hexuria/panday"}),
                )],
            ),
            ScriptedTurn::text("there are two open issues"),
        ])),
        registry,
        // `dev` is the everyday profile. An MCP tool still asks.
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );

    let outcome = actor
        .handle_user_input("how many open issues?")
        .await
        .unwrap();
    let parked = match outcome {
        TurnOutcome::AwaitingPermission(ids) => ids,
        other => panic!("an MCP tool must ask before it runs, got {other:?}"),
    };
    assert_eq!(parked.len(), 1);

    // The request names the tool a human is being asked about, qualified — "allow
    // bash" and "allow mcp:github:create_issue" are very different questions.
    let asked = store
        .all()
        .iter()
        .find_map(|e| match &e.event {
            Event::PermissionRequest { tool, .. } => Some(tool.clone()),
            _ => None,
        })
        .expect("a permission request");
    assert_eq!(asked, "mcp:github:list_issues");

    actor
        .decide(parked[0], PermDecision::Allow, Actor::User)
        .await
        .unwrap();

    let result = store
        .all()
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolResult {
                output, is_error, ..
            } => Some((output.text.clone(), *is_error)),
            _ => None,
        })
        .expect("the tool ran after approval");
    assert!(
        result.0.contains("2 open issues in hexuria/panday"),
        "{result:?}"
    );
    assert!(!result.1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_mcp_call_never_reaches_the_server() {
    let server = mount().await;
    let store = Arc::new(MemoryStore::new());
    let mut registry = ToolRegistry::default();
    for tool in McpTool::all(server.clone()) {
        registry.register(tool);
    }

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "filing a bug",
                vec![(
                    "mcp:github:create_issue",
                    serde_json::json!({"repo": "hexuria/panday", "title": "spurious"}),
                )],
            ),
            ScriptedTurn::text("understood, not filing it"),
        ])),
        registry,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );

    let parked = match actor.handle_user_input("file a bug").await.unwrap() {
        TurnOutcome::AwaitingPermission(ids) => ids,
        other => panic!("{other:?}"),
    };
    actor
        .decide(parked[0], PermDecision::Deny, Actor::User)
        .await
        .unwrap();

    let results: Vec<String> = store
        .all()
        .iter()
        .filter_map(|e| match &e.event {
            Event::ToolResult { output, .. } => Some(output.text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        results.iter().all(|r| !r.contains("opened")),
        "the denied call reached the server: {results:?}"
    );
    assert!(
        results.iter().any(|r| r.contains("denied")),
        "the model should be told it was denied: {results:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_granted_tool_still_runs_under_the_sandbox_tier_it_belongs_to() {
    // A grant relaxes the *gate*, never the isolation: the server is still a child
    // process, and its seconds still meter as T2 (docs/21 M21.2).
    let server = mount().await;
    let granted = McpTool::new(server.clone(), server.tools()[0].clone()).granted();
    let req = Tool::requirements(&granted);
    assert_eq!(req.sandbox_tier, SandboxTier::T2OsJail);
    // And not `None`: a granted tool is one a human said yes to, not one we have
    // established is a pure read.
    assert_eq!(req.side_effects, SideEffects::Idempotent);
}
