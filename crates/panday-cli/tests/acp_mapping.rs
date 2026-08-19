//! M3.4 — unknown-event tolerance in the CLI, and the AEP⇄ACP mapping table
//! (docs/03 §Milestones, docs/16 §ACP bridge).
//!
//! The assertions are made against **serialized ACP JSON** rather than against
//! the Rust types. An editor parses bytes: a mapping that type-checks but emits
//! the wrong field name is a mapping that does not work, and only the wire form
//! catches that.

use agent_client_protocol::schema::v1 as acp;
use panday_cli::acp as map;
use panday_types::event::{Actor, ClientKind, Envelope, Event, PermDecision, ReducedOutput};
use panday_types::model::{ContentBlock, ModelRef, StopReason, Usage};
use panday_types::{CallId, SessionId};

fn wrap(seq: u64, event: Event) -> Envelope {
    Envelope {
        v: 1,
        session_id: SessionId::new(),
        seq,
        at: time::OffsetDateTime::UNIX_EPOCH,
        turn_id: None,
        event,
    }
}

fn json(update: &acp::SessionUpdate) -> serde_json::Value {
    serde_json::to_value(update).expect("ACP updates serialize")
}

fn text(content: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: content.into(),
    }]
}

// ── The six core events ──────────────────────────────────────────────────────

#[test]
fn a_user_message_becomes_a_user_message_chunk() {
    let u = map::session_update(&Event::UserMessage {
        content: text("why does the test fail?"),
        source: ClientKind::Acp,
    })
    .expect("mapped");
    let j = json(&u);
    assert_eq!(j["sessionUpdate"], "user_message_chunk");
    assert_eq!(j["content"]["text"], "why does the test fail?");
}

#[test]
fn a_delta_becomes_an_agent_message_chunk() {
    let u = map::session_update(&Event::AssistantDelta {
        text: "Let me look".into(),
    })
    .expect("mapped");
    let j = json(&u);
    assert_eq!(j["sessionUpdate"], "agent_message_chunk");
    assert_eq!(j["content"]["text"], "Let me look");
}

#[test]
fn a_tool_call_becomes_an_in_progress_tool_call_with_a_readable_title() {
    let u = map::session_update(&Event::ToolCall {
        call_id: CallId::new(),
        tool: "read_file".into(),
        args: serde_json::json!({"path": "src/lib.rs"}),
        provider_call_id: None,
    })
    .expect("mapped");
    let j = json(&u);
    assert_eq!(j["sessionUpdate"], "tool_call");
    assert_eq!(j["status"], "in_progress");
    assert_eq!(j["kind"], "read");
    // The collapsed row in an editor is this string. "read_file" alone would
    // make every row identical; the raw JSON blob would make it unreadable.
    assert_eq!(j["title"], "read_file: src/lib.rs");
    // Follow-along: the editor can open the file the agent is reading.
    assert_eq!(j["locations"][0]["path"], "src/lib.rs");
    assert_eq!(j["rawInput"]["path"], "src/lib.rs");
}

#[test]
fn a_tool_result_updates_the_same_call_rather_than_starting_a_new_one() {
    // The seam that decides whether an editor draws one row or two.
    let id = CallId::new();
    let call = map::session_update(&Event::ToolCall {
        call_id: id,
        tool: "bash".into(),
        args: serde_json::json!({"cmd": "cargo test"}),
        provider_call_id: None,
    })
    .unwrap();
    let result = map::session_update(&Event::ToolResult {
        call_id: id,
        output: ReducedOutput {
            text: "test result: ok. 12 passed".into(),
            tokens_raw: 900,
            tokens_kept: 40,
            strategy: "cargo_test_v1".into(),
        },
        raw_ref: None,
        duration_ms: 1200,
        is_error: false,
    })
    .unwrap();

    let (a, b) = (json(&call), json(&result));
    assert_eq!(b["sessionUpdate"], "tool_call_update");
    assert_eq!(a["toolCallId"], b["toolCallId"], "same call, same id");
    assert_eq!(b["status"], "completed");
    // The reduced text, not the raw output: an editor showing something the
    // model never saw would be debugging a different session (docs/15).
    assert_eq!(
        b["content"][0]["content"]["text"],
        "test result: ok. 12 passed"
    );
}

#[test]
fn a_failed_tool_is_failed_not_completed() {
    let u = map::session_update(&Event::ToolResult {
        call_id: CallId::new(),
        output: ReducedOutput {
            text: "error[E0308]: mismatched types".into(),
            tokens_raw: 100,
            tokens_kept: 20,
            strategy: "cargo_build_v1".into(),
        },
        raw_ref: None,
        duration_ms: 10,
        is_error: true,
    })
    .unwrap();
    assert_eq!(json(&u)["status"], "failed");
}

#[test]
fn a_permission_request_is_a_request_not_a_session_update() {
    // If this were a notification the loop would run the tool without waiting
    // for an answer — the one bug in this mapping that costs more than a
    // rendering glitch.
    let event = Event::PermissionRequest {
        call_id: CallId::new(),
        tool: "bash".into(),
        action: "rm -rf build/".into(),
        options: vec!["allow".into(), "deny".into()],
    };
    assert!(
        map::session_update(&event).is_none(),
        "a permission request must not be sent as a session update"
    );

    let req = map::permission_request(&acp::SessionId::new("sess-1"), &event).expect("mapped");
    let j = serde_json::to_value(&req).unwrap();
    assert_eq!(j["sessionId"], "sess-1");
    assert_eq!(j["toolCall"]["status"], "pending");
    assert_eq!(j["toolCall"]["title"], "bash: rm -rf build/");
    assert_eq!(j["toolCall"]["kind"], "execute");
    let ids: Vec<&str> = j["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["allow_once", "allow_always", "reject_once", "reject_always"]
    );
}

#[test]
fn every_acp_permission_answer_maps_back_to_a_gate_decision() {
    assert_eq!(
        map::decision_of("allow_once"),
        Some((PermDecision::Allow, false))
    );
    assert_eq!(
        map::decision_of("allow_always"),
        Some((PermDecision::AllowRemember, false))
    );
    assert_eq!(
        map::decision_of("reject_once"),
        Some((PermDecision::Deny, false))
    );
    // ACP's fourth option has no `PermDecision`: a remembered *denial* is a
    // policy change, not a turn answer. It comes back flagged so the bridge
    // cannot silently downgrade it to a one-off deny.
    assert_eq!(
        map::decision_of("reject_always"),
        Some((PermDecision::Deny, true))
    );
    assert_eq!(map::decision_of("something_else"), None);
    assert_eq!(map::decided_by(), Actor::User);
}

#[test]
fn turn_boundaries_and_bookkeeping_have_no_acp_equivalent() {
    // Not a gap: ACP models a prompt turn's end in the `session/prompt`
    // response's stopReason, and a compaction is invisible to an editor.
    for event in [
        Event::TurnStarted {
            model: ModelRef("local/test".into()),
            parent: None,
        },
        Event::TurnFinished {
            reason: StopReason::EndTurn,
            usage: Usage::default(),
            cost_micros: 0,
        },
    ] {
        assert!(
            map::session_update(&event).is_none(),
            "{:?} should not map",
            event.kind()
        );
    }
}

// ── Unknown-event tolerance ──────────────────────────────────────────────────

#[test]
fn an_event_from_a_newer_version_maps_to_nothing_rather_than_failing() {
    // docs/03: unknown kinds MUST be ignored-and-preserved. For the ACP bridge
    // that means "an update I do not render", never "the editor lost the
    // session" — and a newer server is exactly when an editor is most in use.
    let raw = serde_json::json!({
        "v": 1,
        "session_id": "01930000-0000-7000-8000-000000000001",
        "seq": 9,
        "at": "2026-01-15T12:00:00Z",
        "event": "plan_updated",
        "steps": ["a", "b"]
    });
    let env: Envelope = serde_json::from_value(raw).expect("unknown kinds parse");
    assert!(env.event.is_unknown());
    assert!(map::session_update(&env.event).is_none());

    // And it does not stop the events around it from being rendered.
    let log = vec![
        wrap(
            1,
            Event::UserMessage {
                content: text("hi"),
                source: ClientKind::Acp,
            },
        ),
        env,
        wrap(
            3,
            Event::AssistantMessage {
                content: text("hello"),
                usage: Usage::default(),
            },
        ),
    ];
    assert_eq!(
        map::replay_updates(&log).len(),
        2,
        "the known events still map"
    );
}

#[test]
fn a_live_stream_sends_deltas_and_a_replay_sends_folded_messages() {
    // Sending both would print the assistant's answer twice in the editor.
    let log = vec![
        wrap(1, Event::AssistantDelta { text: "he".into() }),
        wrap(2, Event::AssistantDelta { text: "llo".into() }),
        wrap(
            3,
            Event::AssistantMessage {
                content: text("hello"),
                usage: Usage::default(),
            },
        ),
    ];
    assert_eq!(map::live_updates(&log).len(), 2);
    assert_eq!(map::replay_updates(&log).len(), 1);
}

#[test]
fn an_unrecognised_tool_gets_the_neutral_kind() {
    // MCP tools and plugin tools are not in our native set and never will be.
    assert_eq!(
        serde_json::to_value(map::tool_kind("some_mcp_tool")).unwrap(),
        "other"
    );
    assert_eq!(
        serde_json::to_value(map::tool_kind("grep")).unwrap(),
        "search"
    );
    assert_eq!(
        serde_json::to_value(map::tool_kind("edit_file")).unwrap(),
        "edit"
    );
}
