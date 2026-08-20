//! M13.2 — the loop fixes a real failing test in a fixture repo, unattended.
//!
//! docs/13's acceptance for this milestone is behavioural, so this test is
//! too: it copies a genuinely broken crate into a sandboxed workspace, runs
//! the turn loop over the **real** native tools and a **real** T2 jail, and
//! then checks the repo by running `cargo test` in it afterwards.
//!
//! The model is scripted rather than live. That is deliberate and is what
//! docs/02 asks for ("the harness suite runs without network using the fake
//! client") — the script decides *what* to do, but every tool call, file
//! edit, sandbox policy and test run underneath it is real. What is not
//! covered here is whether a live model would choose these steps; that is
//! `--ignored` below and needs credentials.

#![cfg(target_os = "macos")]

use panday_harness::native::{register_native, Workspace};
use panday_harness::testing::{ScriptedClient, ScriptedTurn};
use panday_harness::tools::ToolRegistry;
use panday_harness::{
    CollectSink, MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget, TurnOutcome,
};
use panday_sandbox::{
    FsPolicy, Limits, NetPolicy, Sandbox, SandboxPolicy, SandboxTier, SessionSpec, T2MacosSandbox,
};
use panday_types::event::Event;
use panday_types::model::{ModelRef, StopReason};
use panday_types::{AccountId, SessionId};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "panday-m132-{tag}-{}-{nanos}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// Stage a copy of the broken crate in a fresh workspace.
fn staged_repo() -> TempDir {
    let dir = TempDir::new("repo");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture_repo");
    copy_tree(&fixture, dir.path());
    dir
}

/// Where cargo and rustc live — injected explicitly, since the jail inherits
/// no environment (docs/14 §policy).
fn toolchain_env() -> Vec<(String, String)> {
    let mut path = "/usr/bin:/bin:/usr/sbin:/sbin".to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let cargo_bin = PathBuf::from(&home).join(".cargo/bin");
        if cargo_bin.exists() {
            path = format!("{}:{path}", cargo_bin.display());
        }
    }
    let mut env = vec![("PATH".into(), path)];
    // cargo needs its own home for the registry cache; without it cargo
    // rebuilds its metadata into the workspace on every run.
    if let Some(home) = std::env::var_os("HOME") {
        env.push((
            "CARGO_HOME".into(),
            PathBuf::from(&home).join(".cargo").display().to_string(),
        ));
        env.push((
            "RUSTUP_HOME".into(),
            PathBuf::from(&home).join(".rustup").display().to_string(),
        ));
    }
    env
}

async fn jailed_workspace(root: &Path) -> Option<(Arc<T2MacosSandbox>, Workspace)> {
    if !T2MacosSandbox::available() {
        return None;
    }
    let sandbox = Arc::new(T2MacosSandbox::new());
    let handle = sandbox
        .create(SessionSpec {
            tier: SandboxTier::T2OsJail,
            policy: SandboxPolicy {
                fs: FsPolicy {
                    workspace_rw: root.to_path_buf(),
                    staged_ro: vec![],
                },
                net: NetPolicy::default(),
                limits: Limits {
                    wall_clock_ms: 180_000,
                    ..Default::default()
                },
                env: toolchain_env(),
            },
        })
        .await
        .expect("create T2 session");

    let canonical = std::fs::canonicalize(root).unwrap();
    let ws = Workspace::new(sandbox.clone(), handle, canonical);
    Some((sandbox, ws))
}

/// Run `cargo test` in the repo, outside the harness, to check the result.
fn cargo_test_passes(repo: &Path) -> (bool, String) {
    let out = std::process::Command::new("cargo")
        .arg("test")
        .current_dir(repo)
        .output()
        .expect("run cargo test");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

#[tokio::test]
async fn the_loop_fixes_a_real_failing_test_unattended() {
    let repo = staged_repo();
    let Some((_sandbox, ws)) = jailed_workspace(repo.path()).await else {
        eprintln!("skipping: T2 unavailable on this host");
        return;
    };

    // Precondition: the repo really is broken. Without this the whole test
    // could pass against a repo that was already green.
    let (before, _) = cargo_test_passes(repo.path());
    assert!(!before, "the fixture must start out failing");

    let mut registry = ToolRegistry::default();
    register_native(&mut registry, ws);

    // The script is the model's *plan*; everything it invokes is real.
    let script = vec![
        ScriptedTurn::calling(
            "Let me find the function under test.",
            vec![("grep", serde_json::json!({"pattern": "fn sum_to"}))],
        ),
        ScriptedTurn::calling(
            "Reading it.",
            vec![("read_file", serde_json::json!({"path": "src/lib.rs"}))],
        ),
        ScriptedTurn::calling(
            "The range excludes n. Making it inclusive.",
            vec![(
                "edit_file",
                serde_json::json!({
                    "path": "src/lib.rs",
                    "old": "(1..n).sum()",
                    "new": "(1..=n).sum()"
                }),
            )],
        ),
        ScriptedTurn::calling(
            "Verifying.",
            vec![(
                "bash",
                serde_json::json!({"cmd": "cargo test 2>&1 | tail -5"}),
            )],
        ),
        ScriptedTurn::text("Fixed: the range is now inclusive and the test passes."),
    ];

    let store = Arc::new(MemoryStore::new());
    let sink = Arc::new(CollectSink::new());
    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(script)),
        registry,
        // `dev`, which is what Phase 1's exit criterion names: "fixes a real
        // failing test ... unattended, under `dev` profile". docs/13 says dev
        // allows read/edit/test, so none of this should park for consent.
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget {
            max_wall_ms: 300_000,
            ..Default::default()
        },
    );
    actor.subscribe(sink);

    let outcome = actor
        .handle_user_input("cargo test fails — find out why and fix it")
        .await
        .expect("the loop must not error");

    assert_eq!(
        outcome,
        TurnOutcome::Finished(StopReason::EndTurn),
        "the loop did not run to completion"
    );

    // The actual acceptance: the repo is now green.
    let (after, output) = cargo_test_passes(repo.path());
    assert!(
        after,
        "the repo is still failing after the loop ran:\n{output}"
    );

    // And the source really changed — not, say, the test being deleted.
    let fixed = std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(
        fixed.contains("(1..=n).sum()"),
        "the fix is not in the source"
    );
    assert!(
        fixed.contains("assert_eq!(sum_to(5), 15"),
        "the test itself must still be there — passing by deleting the test is not fixing it"
    );
}

#[tokio::test]
async fn every_tool_call_in_that_run_is_recorded_with_its_observation() {
    // The run above must be auditable and replayable (ADR-002), not just
    // effective.
    let repo = staged_repo();
    let Some((_sandbox, ws)) = jailed_workspace(repo.path()).await else {
        return;
    };

    let mut registry = ToolRegistry::default();
    register_native(&mut registry, ws);

    let store = Arc::new(MemoryStore::new());
    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "look",
                vec![("read_file", serde_json::json!({"path": "src/lib.rs"}))],
            ),
            ScriptedTurn::text("done"),
        ])),
        registry,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );

    actor.handle_user_input("read it").await.unwrap();

    let log = store.all();
    let calls = log
        .iter()
        .filter(|e| matches!(e.event, Event::ToolCall { .. }))
        .count();
    let results = log
        .iter()
        .filter(|e| matches!(e.event, Event::ToolResult { .. }))
        .count();
    assert_eq!(calls, 1);
    assert_eq!(
        results, calls,
        "every call must have a recorded observation"
    );

    // And the observation is the real file, read through the sandbox.
    let text = log
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolResult { output, .. } => Some(output.text.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        text.contains("sum_to"),
        "the tool did not read the real file; got: {text}"
    );
}

#[tokio::test]
async fn the_bash_tool_cannot_reach_the_network_from_inside_the_loop() {
    // The agent runs commands it was told to run; the jail is what stops
    // those commands exfiltrating. Proving it at the tool layer, not just the
    // sandbox layer, is the point.
    let repo = staged_repo();
    let Some((_sandbox, ws)) = jailed_workspace(repo.path()).await else {
        return;
    };
    let mut registry = ToolRegistry::default();
    register_native(&mut registry, ws);

    let tool = registry.get("bash").expect("bash is registered");
    let out = tool
        .call(
            panday_harness::tools::ToolCtx {
                account: AccountId::new(),
                session: SessionId::new(),
                turn: panday_types::TurnId::new(),
            },
            serde_json::json!({"cmd": "curl -s -m 8 -o /dev/null https://example.com; echo rc=$?"}),
        )
        .await;

    assert!(
        !out.raw.contains("rc=0"),
        "ESCAPE: the bash tool reached the network: {}",
        out.raw
    );
}

/// Pick a live backend the same way the CLI does (docs/11): an Anthropic key,
/// or any OpenAI-compatible base via `PANDAY_COMPAT_BASE_URL`.
///
/// The second path is how this machine dogsfoods without an Anthropic key —
/// OpenCodex on loopback already holds the xAI OAuth session and speaks Chat
/// Completions. We do **not** copy that OAuth flow into panday; we point the
/// existing `openai_compat` adapter at the proxy. The `together/` prefix is
/// the adapter slot the CLI already registers; OpenCodex sees the rest of the
/// id (`xai/grok-4.6`). Override with `PANDAY_COMPAT_MODEL`.
fn live_backend() -> (String, Arc<dyn panday_gateway::ProviderAdapter>, ModelRef) {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.trim().is_empty() {
            return (
                "anthropic".into(),
                Arc::new(panday_gateway::adapters::anthropic::Anthropic::new(key))
                    as Arc<dyn panday_gateway::ProviderAdapter>,
                ModelRef::auto(),
            );
        }
    }
    if let Ok(base) = std::env::var("PANDAY_COMPAT_BASE_URL") {
        if !base.trim().is_empty() {
            let key = std::env::var("PANDAY_COMPAT_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty());
            let model = std::env::var("PANDAY_COMPAT_MODEL")
                .ok()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| "together/xai/grok-4.6".into());
            return (
                "together".into(),
                Arc::new(panday_gateway::adapters::openai_compat::OpenAiCompat::new(
                    base, key,
                )) as Arc<dyn panday_gateway::ProviderAdapter>,
                ModelRef(model),
            );
        }
    }
    panic!(
        "live leg needs ANTHROPIC_API_KEY, or PANDAY_COMPAT_BASE_URL \
         (and optionally PANDAY_COMPAT_MODEL, default together/xai/grok-4.6)"
    );
}

/// The live-model leg of M13.2's acceptance.
///
/// Ignored by default: it needs a real provider, and docs/02 requires the
/// unit suite to run with no network. Run it deliberately:
///
/// ```text
/// ANTHROPIC_API_KEY=... cargo test -p panday-harness --test fix_a_failing_test -- --ignored
///
/// # or, any OpenAI-compatible proxy already holding a login (OpenCodex, llama-server, …):
/// PANDAY_COMPAT_BASE_URL=http://127.0.0.1:8080 \
///   cargo test -p panday-harness --test fix_a_failing_test -- --ignored
/// ```
#[tokio::test]
#[ignore = "needs a live model; see the doc comment"]
async fn a_live_model_fixes_it_unattended() {
    let (prefix, adapter, model) = live_backend();

    let repo = staged_repo();
    let Some((_sandbox, ws)) = jailed_workspace(repo.path()).await else {
        return;
    };
    let (before, _) = cargo_test_passes(repo.path());
    assert!(!before, "the fixture must start out failing");

    let mut registry = ToolRegistry::default();
    register_native(&mut registry, ws);

    let gateway = panday_gateway::Gateway::builder(Arc::new(
        panday_router::PolicyRouter::from_yaml(include_str!("../../panday-router/policy/dev.yaml"))
            .unwrap(),
    ))
    .adapter(&prefix, adapter)
    .build();

    let store = Arc::new(MemoryStore::new());
    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        model,
        store.clone(),
        Arc::new(gateway),
        registry,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget {
            max_steps: 25,
            max_wall_ms: 600_000,
            ..Default::default()
        },
    );

    actor
        .handle_user_input(
            "`cargo test` in this repo fails. Find the bug and fix it. \
             Do not change the test; fix the implementation. Verify with `cargo test`.",
        )
        .await
        .expect("the loop must not error");

    let (after, output) = cargo_test_passes(repo.path());
    assert!(after, "a live model did not fix the repo:\n{output}");
    let fixed = std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap();
    assert!(
        fixed.contains("assert_eq!(sum_to(5), 15"),
        "the model deleted the test instead of fixing the bug"
    );
}

// ---------------------------------------------------------------------------
// M15.3 — read dedup wired into the real tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn re_reading_an_unchanged_file_returns_a_pointer_not_the_file() {
    // docs/15 layer 2: the Read channel rtk never covered. Agents re-read the
    // same files constantly, and in a long session those re-reads dominate.
    let repo = staged_repo();
    let Some((_sandbox, ws)) = jailed_workspace(repo.path()).await else {
        return;
    };
    let mut registry = ToolRegistry::default();
    register_native(&mut registry, ws);

    let ctx = || panday_harness::tools::ToolCtx {
        account: AccountId::new(),
        session: SessionId::new(),
        turn: panday_types::TurnId::new(),
    };
    let read = registry.get("read_file").unwrap();
    let args = serde_json::json!({"path": "src/lib.rs"});

    let first = read.call(ctx(), args.clone()).await;
    assert!(first.raw.contains("sum_to"), "first read must be verbatim");

    let second = read.call(ctx(), args.clone()).await;
    assert!(
        second.raw.contains("unchanged since"),
        "a re-read should point at the held copy: {}",
        second.raw
    );
    assert!(
        second.raw.len() < first.raw.len() / 2,
        "the re-read should be far smaller: {} vs {}",
        second.raw.len(),
        first.raw.len()
    );
}

#[tokio::test]
async fn a_write_invalidates_the_held_copy_so_a_re_read_is_not_falsely_unchanged() {
    // The dangerous case: reporting "unchanged" about content that changed is
    // a wrong answer, not merely a missed saving.
    let repo = staged_repo();
    let Some((_sandbox, ws)) = jailed_workspace(repo.path()).await else {
        return;
    };
    let mut registry = ToolRegistry::default();
    register_native(&mut registry, ws);

    let ctx = || panday_harness::tools::ToolCtx {
        account: AccountId::new(),
        session: SessionId::new(),
        turn: panday_types::TurnId::new(),
    };
    let read = registry.get("read_file").unwrap();
    let edit = registry.get("edit_file").unwrap();
    let args = serde_json::json!({"path": "src/lib.rs"});

    read.call(ctx(), args.clone()).await;

    let edited = edit
        .call(
            ctx(),
            serde_json::json!({
                "path": "src/lib.rs",
                "old": "(1..n).sum()",
                "new": "(1..=n).sum()"
            }),
        )
        .await;
    assert!(!edited.is_error, "{}", edited.raw);

    let after = read.call(ctx(), args).await;
    assert!(
        !after.raw.contains("unchanged since"),
        "a file edited in between must never report unchanged: {}",
        after.raw
    );
    assert!(
        after.raw.contains("1..=n"),
        "the new content must be visible: {}",
        after.raw
    );
}

#[tokio::test]
async fn write_file_also_invalidates_the_held_copy() {
    // Same hazard as edit_file: a stale belief would report "unchanged" about
    // content the agent itself just replaced.
    let repo = staged_repo();
    let Some((_sandbox, ws)) = jailed_workspace(repo.path()).await else {
        return;
    };
    let mut registry = ToolRegistry::default();
    register_native(&mut registry, ws);

    let ctx = || panday_harness::tools::ToolCtx {
        account: AccountId::new(),
        session: SessionId::new(),
        turn: panday_types::TurnId::new(),
    };
    let read = registry.get("read_file").unwrap();
    let write = registry.get("write_file").unwrap();
    let args = serde_json::json!({"path": "src/lib.rs"});

    read.call(ctx(), args.clone()).await;
    let wrote = write
        .call(
            ctx(),
            serde_json::json!({"path": "src/lib.rs", "content": "pub fn replaced() {}"}),
        )
        .await;
    assert!(!wrote.is_error, "{}", wrote.raw);

    let after = read.call(ctx(), args).await;
    assert!(
        !after.raw.contains("unchanged since"),
        "must not claim unchanged after a write: {}",
        after.raw
    );
    assert!(after.raw.contains("replaced"), "{}", after.raw);
}
