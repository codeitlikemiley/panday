//! Golden-file tests for the AEP wire format.
//!
//! docs/02-workspace.md §Testing philosophy:
//!   "The event protocol gets **golden-file tests**: serialized fixtures
//!    checked in; any diff is a reviewed protocol change (see 03 versioning)."
//!
//! The corpus below is the *source*; `tests/fixtures/*.json` are the *locked
//! output*. A change to either that the other does not agree with fails CI.
//! That is the entire point: you cannot alter the wire format by accident.
//!
//! Regenerate after an intentional protocol change (and explain it in the
//! commit, per docs/03 §Versioning discipline):
//!
//! ```text
//! FERRUM_UPDATE_GOLDEN=1 cargo test -p ferrum-types --test golden
//! ```

use ferrum_types::event::{Actor, ClientKind, Envelope, Event, PermDecision, ReducedOutput};
use ferrum_types::id::{AccountId, ArtifactRef, CallId, RequestId, SessionId, TurnId};
use ferrum_types::model::{ContentBlock, ModelRef, StopReason, Usage};
use ferrum_types::{Timestamp, PROTOCOL_VERSION};
use std::path::{Path, PathBuf};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Deterministic fixture inputs
//
// Fixtures must be byte-stable across runs and machines, so every id and
// timestamp here is hardcoded. `SessionId::new()` (UUIDv7 from the clock) is
// exactly what a fixture must never contain.
// ---------------------------------------------------------------------------

fn uuid(n: u128) -> Uuid {
    // A fixed UUIDv7-shaped value: version nibble 7, variant bits 0b10.
    Uuid::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0000u128 | n)
}

fn session() -> SessionId {
    SessionId(uuid(0x01))
}
fn turn() -> TurnId {
    TurnId(uuid(0x02))
}
fn call() -> CallId {
    CallId(uuid(0x03))
}
fn child_session() -> SessionId {
    SessionId(uuid(0x04))
}

fn at() -> Timestamp {
    time::macros::datetime!(2026-01-15 12:00:00 UTC)
}

fn artifact() -> ArtifactRef {
    ArtifactRef {
        hash: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".into(),
        size: 40_960,
        media_type: Some("text/plain".into()),
    }
}

fn usage() -> Usage {
    // Cache counts are SUBSETS of input_tokens (see the CONVENTION note on
    // `Usage` in src/model.rs). 1200 = 900 cache-read + 200 write + 100 fresh.
    Usage {
        input_tokens: 1200,
        output_tokens: 300,
        cache_read_tokens: 900,
        cache_write_tokens: 200,
        cache_write_1h_tokens: 0,
    }
}

/// One envelope per `Event` variant. The file name is the fixture name.
fn corpus() -> Vec<(&'static str, Envelope)> {
    let env = |event: Event| Envelope {
        v: PROTOCOL_VERSION,
        session_id: session(),
        seq: 1,
        turn_id: Some(turn()),
        at: at(),
        event,
    };

    vec![
        (
            "user_message",
            Envelope {
                seq: 1,
                turn_id: None,
                ..env(Event::UserMessage {
                    content: vec![ContentBlock::Text {
                        text: "fix the failing test".into(),
                    }],
                    source: ClientKind::Cli,
                })
            },
        ),
        (
            "assistant_delta",
            Envelope {
                seq: 3,
                ..env(Event::AssistantDelta {
                    text: "Looking at ".into(),
                })
            },
        ),
        (
            "assistant_message",
            Envelope {
                seq: 4,
                ..env(Event::AssistantMessage {
                    content: vec![ContentBlock::Text {
                        text: "The assertion compares stale state.".into(),
                    }],
                    usage: usage(),
                })
            },
        ),
        (
            "tool_call",
            Envelope {
                seq: 5,
                ..env(Event::ToolCall {
                    call_id: call(),
                    tool: "run_tests".into(),
                    args: serde_json::json!({ "package": "ferrum-types" }),
                })
            },
        ),
        (
            "tool_result",
            Envelope {
                seq: 6,
                ..env(Event::ToolResult {
                    call_id: call(),
                    output: ReducedOutput {
                        text: "1 failed: envelope_round_trips".into(),
                        tokens_raw: 10_240,
                        tokens_kept: 460,
                        strategy: "cargo_test_digest".into(),
                    },
                    raw_ref: Some(artifact()),
                    duration_ms: 8_412,
                    is_error: true,
                })
            },
        ),
        (
            "tool_result_inline",
            // No `raw_ref`: small output that was never spilled. Locks the
            // `skip_serializing_if` behaviour so the absent field stays absent.
            Envelope {
                seq: 7,
                ..env(Event::ToolResult {
                    call_id: call(),
                    output: ReducedOutput {
                        text: "ok".into(),
                        tokens_raw: 2,
                        tokens_kept: 2,
                        strategy: "passthrough".into(),
                    },
                    raw_ref: None,
                    duration_ms: 12,
                    is_error: false,
                })
            },
        ),
        (
            "permission_request",
            Envelope {
                seq: 8,
                ..env(Event::PermissionRequest {
                    call_id: call(),
                    tool: "edit_file".into(),
                    action: "write crates/ferrum-types/src/event.rs".into(),
                    options: vec!["allow".into(), "allow_remember".into(), "deny".into()],
                })
            },
        ),
        (
            "permission_decision",
            Envelope {
                seq: 9,
                ..env(Event::PermissionDecision {
                    call_id: call(),
                    decision: PermDecision::AllowRemember,
                    by: Actor::User,
                })
            },
        ),
        (
            "permission_decision_by_policy",
            // `Actor::Policy` is the only struct-variant actor; locks its shape.
            Envelope {
                seq: 10,
                ..env(Event::PermissionDecision {
                    call_id: call(),
                    decision: PermDecision::Allow,
                    by: Actor::Policy {
                        rule: "dev-profile:allow-edit-in-workspace".into(),
                    },
                })
            },
        ),
        (
            "compaction",
            Envelope {
                seq: 11,
                ..env(Event::Compaction {
                    from_seq: 2,
                    to_seq: 10,
                    summary_ref: artifact(),
                    tokens_before: 48_000,
                    tokens_after: 3_200,
                })
            },
        ),
        (
            "turn_started",
            Envelope {
                seq: 2,
                ..env(Event::TurnStarted {
                    model: ModelRef("anthropic/claude-sonnet-4-5".into()),
                    parent: None,
                })
            },
        ),
        (
            "turn_started_subagent",
            // A child turn carries `parent`; locks the optional-field shape.
            Envelope {
                seq: 12,
                ..env(Event::TurnStarted {
                    model: ModelRef::auto(),
                    parent: Some(TurnId(uuid(0x05))),
                })
            },
        ),
        (
            "turn_finished",
            Envelope {
                seq: 13,
                ..env(Event::TurnFinished {
                    reason: StopReason::EndTurn,
                    usage: usage(),
                    cost_micros: 4_200,
                })
            },
        ),
        (
            "subagent_spawned",
            Envelope {
                seq: 14,
                ..env(Event::SubagentSpawned {
                    child: child_session(),
                    brief: "audit the reducer fixtures".into(),
                })
            },
        ),
        (
            "subagent_finished",
            Envelope {
                seq: 15,
                ..env(Event::SubagentFinished {
                    child: child_session(),
                    result_ref: artifact(),
                })
            },
        ),
        (
            "session_forked",
            Envelope {
                seq: 16,
                ..env(Event::SessionForked { from_seq: 9 })
            },
        ),
        (
            "error",
            Envelope {
                seq: 17,
                ..env(Event::Error {
                    code: "provider_overloaded".into(),
                    message: "upstream returned 529 after 3 attempts".into(),
                    retryable: true,
                })
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn updating() -> bool {
    std::env::var_os("FERRUM_UPDATE_GOLDEN").is_some()
}

/// Canonical on-disk form: pretty JSON, one trailing newline. Pretty because
/// a protocol change should read as a reviewable diff, not a wall of one line.
fn render(env: &Envelope) -> String {
    let mut s = serde_json::to_string_pretty(env).expect("envelope must serialize");
    s.push('\n');
    s
}

#[test]
fn fixtures_match_corpus() {
    let dir = fixture_dir();
    if updating() {
        std::fs::create_dir_all(&dir).expect("create fixture dir");
    }

    let mut stale = Vec::new();
    for (name, envelope) in corpus() {
        let path = dir.join(format!("{name}.json"));
        let actual = render(&envelope);

        if updating() {
            std::fs::write(&path, &actual).unwrap_or_else(|e| panic!("write {name}: {e}"));
            continue;
        }

        let expected = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                stale.push(format!("  {name}: cannot read fixture ({e})"));
                continue;
            }
        };

        if expected != actual {
            stale.push(format!(
                "  {name}: fixture differs from corpus\n\
                 --- fixture (checked in)\n{expected}\n\
                 --- corpus (current code)\n{actual}"
            ));
        }
    }

    assert!(
        stale.is_empty(),
        "golden fixtures are out of date — this is a PROTOCOL CHANGE.\n\
         Review it, then regenerate with:\n  \
         FERRUM_UPDATE_GOLDEN=1 cargo test -p ferrum-types --test golden\n\
         and note the change per docs/03 §Versioning discipline.\n\n{}",
        stale.join("\n")
    );
}

/// Every fixture must parse back into the exact envelope that produced it.
/// Round-tripping through the *file* (not just through a string in memory)
/// is what proves a checked-in fixture is still readable by today's code.
#[test]
fn fixtures_round_trip_from_disk() {
    if updating() {
        return; // files are being rewritten; nothing to verify yet
    }
    for (name, expected) in corpus() {
        let path = fixture_dir().join(format!("{name}.json"));
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
        let parsed: Envelope =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {name}: {e}"));
        assert_eq!(parsed, expected, "{name} did not round-trip");
    }
}

/// The fixture directory must contain nothing the corpus does not produce —
/// otherwise a renamed event silently leaves an orphan behind and the suite
/// keeps passing while the protocol has drifted.
#[test]
fn no_orphan_fixtures() {
    if updating() {
        return;
    }
    let known: std::collections::BTreeSet<String> = corpus()
        .iter()
        .map(|(name, _)| format!("{name}.json"))
        .collect();

    let mut orphans = Vec::new();
    for entry in std::fs::read_dir(fixture_dir()).expect("fixture dir must exist") {
        let file = entry.expect("read dir entry").file_name();
        let file = file.to_string_lossy().to_string();
        if file.ends_with(".json") && !known.contains(&file) {
            orphans.push(file);
        }
    }
    orphans.sort();
    assert!(
        orphans.is_empty(),
        "orphan fixtures with no corpus entry: {orphans:?}\n\
         Delete them, or add the matching corpus case."
    );
}

/// Attribution ids are part of the vocabulary even though no AEP event
/// carries them yet (they ride on `CallMeta`, docs/10). Locking their
/// `transparent` encoding here keeps a future derive change from silently
/// reshaping the ledger's join keys.
#[test]
fn id_encoding_is_transparent() {
    let account = AccountId(uuid(0x0a));
    let request = RequestId(uuid(0x0b));
    assert_eq!(
        serde_json::to_value(account).unwrap(),
        serde_json::Value::String("01930000-0000-7000-8000-00000000000a".into())
    );
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        serde_json::Value::String("01930000-0000-7000-8000-00000000000b".into())
    );
    // Display is the human short form, NOT the wire form (docs/03 §Identifiers).
    assert_eq!(account.to_string(), "acct_0193000000007000800000000000000a");
}
