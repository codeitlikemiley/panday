//! M21.3 — `panday replay` (docs/21).
//!
//! docs/21: "This tool is why state-must-fold-from-log is an invariant and not
//! a preference (ADR-002)." So these tests double as an audit of that invariant:
//! every rendering below is produced from the event log and nothing else, and
//! anything a replay cannot show is a missing event rather than a missing
//! feature.

use panday_harness::replay::{diff, render, summarize, turn_costs, ReplayOptions};
use panday_harness::testing::{EchoTool, ScriptedClient, ScriptedTurn};
use panday_harness::tools::ToolRegistry;
use panday_harness::{
    EventStore, MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget,
};
use panday_types::event::{Envelope, Event};
use panday_types::model::{ModelRef, StopReason};
use panday_types::{AccountId, SessionId};
use std::sync::Arc;

/// Run a real session and return its log — a replay must work on logs the
/// harness actually produces, not on hand-built ones.
async fn real_session() -> Vec<Envelope> {
    let store = Arc::new(MemoryStore::new());
    let mut reg = ToolRegistry::default();
    reg.register(EchoTool::ok(
        "read_file",
        "pub fn add(a: i32, b: i32) -> i32 { a - b }",
    ));
    reg.register(EchoTool::failing("bash", "test result: FAILED. 1 failed"));

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("anthropic/claude-sonnet-4-5".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "Let me look at the source and run the tests.",
                vec![
                    ("read_file", serde_json::json!({"path": "src/lib.rs"})),
                    ("bash", serde_json::json!({"cmd": "cargo test"})),
                ],
            ),
            ScriptedTurn::text("The subtraction should be an addition."),
        ])),
        reg,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );
    actor
        .handle_user_input("why does the test fail?")
        .await
        .unwrap();
    store.all()
}

#[tokio::test]
async fn a_replay_reads_like_the_session_the_user_saw() {
    let log = real_session().await;
    let text = render(&log, ReplayOptions::default());

    assert!(text.contains("user"), "{text}");
    assert!(text.contains("why does the test fail?"), "{text}");
    assert!(text.contains("turn 1"), "{text}");
    assert!(text.contains("anthropic/claude-sonnet-4-5"), "{text}");
    assert!(text.contains("→ read_file"), "{text}");
    assert!(text.contains("→ bash"), "{text}");
    assert!(text.contains("EndTurn"), "{text}");

    // Success and failure are distinguishable at a glance — the thing a person
    // scans a replay for.
    assert!(text.contains('✓'), "{text}");
    assert!(text.contains('✗'), "{text}");
}

#[tokio::test]
async fn every_line_is_anchored_to_a_seq_so_it_can_be_cited() {
    // A replay is a debugging artifact; "the thing at seq 7" has to be findable.
    let log = real_session().await;
    let text = render(&log, ReplayOptions::default());

    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            line.trim_start().starts_with('[') || line.starts_with("     "),
            "line is not anchored to a seq: {line:?}"
        );
    }
}

#[tokio::test]
async fn time_travel_shows_a_state_the_session_really_passed_through() {
    let log = real_session().await;
    let full = render(&log, ReplayOptions::default());

    // Cut before the last event.
    let cut = log[log.len() - 2].seq;
    let partial = render(
        &log,
        ReplayOptions {
            at_seq: Some(cut),
            ..Default::default()
        },
    );

    assert!(partial.len() < full.len());
    assert!(
        !partial.contains("EndTurn"),
        "the turn had not finished at that point:\n{partial}"
    );
    // And it is a PREFIX of the full rendering — no interpolation, because the
    // log at seq N is a real state, not a reconstruction.
    assert!(
        full.starts_with(&partial),
        "a truncated replay must be a prefix of the whole"
    );
}

#[tokio::test]
async fn at_seq_zero_renders_nothing_rather_than_everything() {
    // An off-by-one here would show a whole session when asked for none of it.
    let log = real_session().await;
    let none = render(
        &log,
        ReplayOptions {
            at_seq: Some(0),
            ..Default::default()
        },
    );
    assert!(none.trim().is_empty(), "{none}");
}

#[tokio::test]
async fn the_costs_overlay_reports_usage_per_turn() {
    let log = real_session().await;
    let text = render(
        &log,
        ReplayOptions {
            costs: true,
            ..Default::default()
        },
    );
    assert!(text.contains("usage:"), "{text}");
    assert!(text.contains("cache read"), "{text}");
    assert!(text.contains('$'), "{text}");

    // Without the flag the overlay is absent — a replay is often read for shape.
    let plain = render(&log, ReplayOptions::default());
    assert!(!plain.contains("usage:"));
}

#[tokio::test]
async fn per_turn_costs_count_usage_once_not_twice() {
    // docs/03: `TurnFinished.usage` is "a redundant turn summary ... folds count
    // the former only". Adding both would double-bill every turn in the report.
    let log = real_session().await;
    let costs = turn_costs(&log);

    assert!(!costs.is_empty());
    let from_assistant_messages: u64 = log
        .iter()
        .filter_map(|e| match &e.event {
            Event::AssistantMessage { usage, .. } => Some(usage.input_tokens),
            _ => None,
        })
        .sum();
    let reported: u64 = costs.iter().map(|c| c.usage.input_tokens).sum();
    assert_eq!(
        reported, from_assistant_messages,
        "the report must not add TurnFinished.usage on top"
    );
}

#[tokio::test]
async fn verbose_shows_full_observations_and_the_default_does_not() {
    let long = "x".repeat(400);
    let store = Arc::new(MemoryStore::new());
    let mut reg = ToolRegistry::default();
    reg.register(EchoTool::ok("read_file", &long));

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "look",
                vec![("read_file", serde_json::json!({"path": "a"}))],
            ),
            ScriptedTurn::text("done"),
        ])),
        reg,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );
    actor.handle_user_input("go").await.unwrap();
    let log = store.all();

    let brief = render(&log, ReplayOptions::default());
    let full = render(
        &log,
        ReplayOptions {
            verbose: true,
            ..Default::default()
        },
    );
    assert!(full.len() > brief.len());
    assert!(brief.contains('…'), "the brief form should elide: {brief}");
}

#[tokio::test]
async fn a_diff_shows_what_changed_between_two_replays() {
    // docs/21's stated use: "before/after a reducer change".
    let log = real_session().await;
    let before = render(&log, ReplayOptions::default());
    let after = render(
        &log,
        ReplayOptions {
            costs: true,
            ..Default::default()
        },
    );

    let d = diff(&before, &after);
    assert!(d.contains("+ "), "the added cost lines should show: {d}");
    assert!(
        !d.contains("- ["),
        "no rendered event should have been removed: {d}"
    );
}

#[tokio::test]
async fn an_identical_replay_diffs_to_nothing_meaningful() {
    let log = real_session().await;
    let text = render(&log, ReplayOptions::default());
    let d = diff(&text, &text);
    assert!(
        !d.contains("+ ") && !d.contains("- "),
        "a replay compared with itself should show no changes: {d}"
    );
}

#[tokio::test]
async fn a_summary_folds_the_whole_session_into_one_line() {
    let log = real_session().await;
    let line = summarize(&log);
    assert!(line.contains("turns"), "{line}");
    assert!(line.contains("cache read"), "{line}");
    assert!(line.contains("EndTurn"), "{line}");
    assert!(!line.contains('\n'), "a summary must be one line: {line}");
}

#[test]
fn a_replay_does_not_crash_on_an_event_from_a_newer_version() {
    // docs/03 requires unknown kinds be ignored-and-preserved. A replay tool
    // that failed on a newer server's log would be useless at exactly the
    // moment someone reached for it.
    let raw = serde_json::json!({
        "v": 1,
        "session_id": "01930000-0000-7000-8000-000000000001",
        "seq": 1,
        "at": "2026-01-15T12:00:00Z",
        "event": "cache_warmed",
        "tokens_primed": 128
    });
    let env: Envelope = serde_json::from_value(raw).expect("unknown kinds must parse");

    let text = render(&[env], ReplayOptions::default());
    assert!(text.contains("unknown event"), "{text}");
    assert!(
        text.contains("cache_warmed"),
        "the kind should be named: {text}"
    );
}

#[test]
fn an_empty_log_renders_empty_rather_than_erroring() {
    assert!(render(&[], ReplayOptions::default()).trim().is_empty());
    assert!(turn_costs(&[]).is_empty());
}

#[tokio::test]
async fn a_permission_round_trip_is_visible_in_a_replay() {
    // An audit that cannot show who approved what is not an audit.
    let store = Arc::new(MemoryStore::new());
    let mut reg = ToolRegistry::default();
    reg.register(EchoTool::irreversible("deploy", "deployed"));

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        store.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "deploy",
                vec![("deploy", serde_json::json!({"env": "prod"}))],
            ),
            ScriptedTurn::text("done"),
        ])),
        reg,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );

    let outcome = actor.handle_user_input("ship it").await.unwrap();
    let ids = match outcome {
        panday_harness::TurnOutcome::AwaitingPermission(ids) => ids,
        other => panic!("expected a park, got {other:?}"),
    };
    actor
        .decide(
            ids[0],
            panday_types::event::PermDecision::Allow,
            panday_types::event::Actor::User,
        )
        .await
        .unwrap();

    let text = render(&store.all(), ReplayOptions::default());
    assert!(text.contains("? permission: deploy"), "{text}");
    assert!(text.contains("Allow"), "{text}");
    assert!(text.contains("User"), "who decided must be visible: {text}");
    assert_eq!(
        render(&store.all(), ReplayOptions::default())
            .matches("? permission")
            .count(),
        1
    );
    let _ = StopReason::EndTurn;
}

// ── The on-disk log (M21.3's `JsonlStore`) ───────────────────────────────────

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("panday-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[tokio::test]
async fn a_session_written_to_disk_replays_identically_to_one_in_memory() {
    // This is the whole claim of ADR-002 reduced to one assertion: the log is
    // the state, so a log that made a round trip through a file must render the
    // same session.
    let path = scratch("roundtrip.jsonl");
    let _ = std::fs::remove_file(&path);

    let file = Arc::new(panday_harness::JsonlStore::open(&path).unwrap());
    let mut reg = ToolRegistry::default();
    reg.register(EchoTool::ok(
        "read_file",
        "fn add(a: i32, b: i32) -> i32 { a - b }",
    ));

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/test".into()),
        file.clone(),
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "look",
                vec![("read_file", serde_json::json!({"path": "a"}))],
            ),
            ScriptedTurn::text("found it"),
        ])),
        reg,
        PermissionEngine::new(Profile::Dev),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );
    actor.handle_user_input("why?").await.unwrap();

    let from_disk = panday_harness::read_log(&path).unwrap();
    assert!(!from_disk.is_empty());
    let text = render(&from_disk, ReplayOptions::default());
    assert!(text.contains("why?"), "{text}");
    assert!(text.contains("→ read_file"), "{text}");
    assert!(text.contains("found it"), "{text}");
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn a_log_with_a_hole_in_it_is_refused_rather_than_folded() {
    // A fold over a gapped log produces a state no session ever held. Silently
    // rendering it would be a debugging tool that invents history.
    let path = scratch("gapped.jsonl");
    let log = real_session().await;
    let mut lines: Vec<String> = log
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect();
    lines.remove(2);
    std::fs::write(&path, lines.join("\n")).unwrap();

    let err = panday_harness::read_log(&path).unwrap_err();
    assert!(
        matches!(err, panday_harness::StoreError::SeqConflict(_)),
        "{err}"
    );
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn a_corrupt_line_names_the_line_it_is_on() {
    let path = scratch("corrupt.jsonl");
    std::fs::write(&path, "{\"not\": \"an envelope\"}\n").unwrap();
    let err = panday_harness::read_log(&path).unwrap_err().to_string();
    assert!(
        err.contains(":1:"),
        "the line number must be in the error: {err}"
    );
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn the_file_store_keeps_seq_gapless_across_a_reopen() {
    // `next_seq` after a restart is the thing that makes resume possible at all
    // (docs/13 §persist-before-proceed).
    let path = scratch("reopen.jsonl");
    let _ = std::fs::remove_file(&path);
    let session = SessionId::new();

    {
        let store = panday_harness::JsonlStore::open(&path).unwrap();
        assert_eq!(store.next_seq(session).await.unwrap(), 1);
        for seq in 1..=3 {
            store
                .append(Envelope {
                    v: 1,
                    session_id: session,
                    seq,
                    at: time::OffsetDateTime::UNIX_EPOCH,
                    turn_id: None,
                    event: Event::SessionForked { from_seq: 0 },
                })
                .await
                .unwrap();
        }
    }

    let reopened = panday_harness::JsonlStore::open(&path).unwrap();
    assert_eq!(reopened.next_seq(session).await.unwrap(), 4);
    assert_eq!(reopened.read_after(session, 1).await.unwrap().len(), 2);
    // A different session sharing the process must not inherit this one's seq.
    assert_eq!(reopened.next_seq(SessionId::new()).await.unwrap(), 1);
    std::fs::remove_file(&path).ok();
}
