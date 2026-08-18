//! Per-dialect conformance fixtures (docs/11, M11.2).
//!
//! > "Conformance fixtures per dialect: recorded request/response pairs
//! > replayed in CI; a provider API drift breaks a fixture, not production."
//!
//! Each `.sse` file under `tests/fixtures/<dialect>/` is a recorded response
//! body. Replaying them proves the two adapters agree on the IR they produce,
//! which is the property the router and the ledger actually depend on — the
//! gateway is only interchangeable if `openai_compat` and `anthropic` are
//! indistinguishable downstream.
//!
//! No network and no model: the fixtures are the provider.

use panday_sdk::providers::{anthropic, openai_compat, sse::SseDecoder};
use panday_types::model::{StopReason, StreamItem, Usage};

fn fixture(dialect: &str, name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dialect)
        .join(format!("{name}.sse"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Replay a recording through a dialect, one byte at a time.
///
/// Byte-at-a-time is not paranoia: a real socket splits records at arbitrary
/// offsets, and feeding whole files would never exercise that.
fn replay_openai(name: &str) -> Vec<StreamItem> {
    let bytes = fixture("openai_compat", name);
    let mut decoder = SseDecoder::new();
    let mut translator = openai_compat::ChunkTranslator::new();
    let mut items = Vec::new();
    for b in &bytes {
        for record in decoder.push(&[*b]) {
            items.extend(translator.push(&record).expect("record must translate"));
        }
    }
    if let Some(rest) = decoder.finish() {
        items.extend(translator.push(&rest).expect("trailing record"));
    }
    items.extend(translator.eof());
    items
}

fn replay_anthropic(name: &str) -> Vec<StreamItem> {
    let bytes = fixture("anthropic", name);
    let mut decoder = SseDecoder::new();
    let mut translator = anthropic::EventTranslator::new();
    let mut items = Vec::new();
    for b in &bytes {
        for record in decoder.push(&[*b]) {
            items.extend(translator.push(&record).expect("record must translate"));
        }
    }
    if let Some(rest) = decoder.finish() {
        items.extend(translator.push(&rest).expect("trailing record"));
    }
    items.extend(translator.eof());
    items
}

fn text(items: &[StreamItem]) -> String {
    items
        .iter()
        .filter_map(|i| match i {
            StreamItem::Delta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn usage(items: &[StreamItem]) -> Usage {
    items
        .iter()
        .find_map(|i| match i {
            StreamItem::Usage { usage } => Some(*usage),
            _ => None,
        })
        .expect("every recording must yield usage or the call bills as free")
}

fn tool_calls(items: &[StreamItem]) -> Vec<(String, Option<String>, String)> {
    let mut out: Vec<(String, Option<String>, String)> = Vec::new();
    for item in items {
        match item {
            StreamItem::ToolCallStart {
                id,
                name,
                provider_id,
            } => out.push((id.to_string(), provider_id.clone(), name.clone())),
            StreamItem::ToolCallDelta { .. } => {}
            _ => {}
        }
    }
    out
}

fn tool_args(items: &[StreamItem]) -> String {
    items
        .iter()
        .filter_map(|i| match i {
            StreamItem::ToolCallDelta { args_fragment, .. } => Some(args_fragment.as_str()),
            _ => None,
        })
        .collect()
}

fn done(items: &[StreamItem]) -> StopReason {
    items
        .iter()
        .find_map(|i| match i {
            StreamItem::Done { reason } => Some(*reason),
            _ => None,
        })
        .expect("a stream must terminate")
}

// ---------------------------------------------------------------------------
// Cross-dialect equivalence — the property that makes providers swappable
// ---------------------------------------------------------------------------

#[test]
fn both_dialects_produce_the_same_ir_for_the_same_completion() {
    let a = replay_openai("text_completion");
    let b = replay_anthropic("text_completion");

    assert_eq!(text(&a), "The test fails.");
    assert_eq!(text(&b), "The test fails.");
    assert_eq!(done(&a), StopReason::EndTurn);
    assert_eq!(done(&b), StopReason::EndTurn);

    // The recordings describe the same token economics stated in each
    // provider's own convention: 1000 total input of which 800 were cached
    // (openai), and 100 fresh + 700 read + 200 written (anthropic).
    // Normalized, both must report a 1000-token input.
    assert_eq!(usage(&a).input_tokens, 1000);
    assert_eq!(usage(&b).input_tokens, 1000);
    assert_eq!(usage(&a).output_tokens, 50);
    assert_eq!(usage(&b).output_tokens, 50);
}

#[test]
fn both_dialects_surface_a_tool_call_the_same_way() {
    let a = replay_openai("tool_call");
    let b = replay_anthropic("tool_call");

    for (dialect, items) in [("openai_compat", &a), ("anthropic", &b)] {
        let calls = tool_calls(items);
        assert_eq!(calls.len(), 1, "{dialect}: one tool call");
        assert_eq!(calls[0].2, "run_tests", "{dialect}: name");
        assert_eq!(
            tool_args(items),
            r#"{"package":"panday-types"}"#,
            "{dialect}: fragments must reassemble"
        );
        assert_eq!(done(items), StopReason::ToolUse, "{dialect}: stop reason");
    }

    // Each dialect's opaque id must survive into the IR verbatim — this is
    // what lets the harness answer the call on the next turn.
    assert_eq!(tool_calls(&a)[0].1.as_deref(), Some("call_abc123"));
    assert_eq!(
        tool_calls(&b)[0].1.as_deref(),
        Some("toolu_01ABCdefGHIjklMNOpqr")
    );
}

// ---------------------------------------------------------------------------
// Usage normalization — the ledger's correctness depends on exactly this
// ---------------------------------------------------------------------------

#[test]
fn anthropic_disjoint_counts_are_normalized_into_a_total_input() {
    // Anthropic reports input_tokens as the FRESH remainder only. Failing to
    // add the cache counts under-reports input by the cached portion, which
    // on a long agent session is most of it.
    let u = usage(&replay_anthropic("text_completion"));
    assert_eq!(u.input_tokens, 1000, "100 fresh + 700 read + 200 written");
    assert_eq!(u.cache_read_tokens, 700);
    assert_eq!(u.cache_write_tokens, 200);
    assert_eq!(u.cache_write_1h_tokens, 0);

    // The convention the ledger relies on (panday_types::model::Usage).
    let subset = u.cache_read_tokens + u.cache_write_tokens + u.cache_write_1h_tokens;
    assert!(
        subset <= u.input_tokens,
        "cache counts must be a SUBSET of input_tokens"
    );
}

#[test]
fn openai_cached_tokens_are_already_a_subset_and_are_not_double_counted() {
    let u = usage(&replay_openai("text_completion"));
    assert_eq!(u.input_tokens, 1000, "must stay the total, not 1800");
    assert_eq!(u.cache_read_tokens, 800);
    // Automatic prefix caching carries no write premium (ADR-007).
    assert_eq!(u.cache_write_tokens, 0);
    assert_eq!(u.cache_write_1h_tokens, 0);
}

#[test]
fn anthropic_splits_cache_writes_by_ttl_tier() {
    // The tiers price differently — 1.25x at 5m, 2x at 1h (ADR-007/008) — so
    // collapsing them would misprice the bill even with the right total.
    let u = usage(&replay_anthropic("extended_cache_ttl"));
    assert_eq!(u.cache_write_tokens, 100, "5m tier");
    assert_eq!(u.cache_write_1h_tokens, 300, "1h tier");
    assert_eq!(u.cache_read_tokens, 400);
    assert_eq!(u.input_tokens, 50 + 400 + 100 + 300);
}

// ---------------------------------------------------------------------------
// Stream discipline, per dialect
// ---------------------------------------------------------------------------

#[test]
fn every_recording_terminates_exactly_once() {
    for items in [
        replay_openai("text_completion"),
        replay_openai("tool_call"),
        replay_anthropic("text_completion"),
        replay_anthropic("tool_call"),
        replay_anthropic("extended_cache_ttl"),
    ] {
        let dones = items
            .iter()
            .filter(|i| matches!(i, StreamItem::Done { .. }))
            .count();
        assert_eq!(dones, 1, "a turn must not appear to end twice");
    }
}

#[test]
fn usage_always_precedes_done() {
    // A consumer that stops reading at Done must still have seen the bill.
    for items in [
        replay_openai("text_completion"),
        replay_anthropic("text_completion"),
        replay_anthropic("extended_cache_ttl"),
    ] {
        let u = items
            .iter()
            .position(|i| matches!(i, StreamItem::Usage { .. }))
            .expect("usage");
        let d = items
            .iter()
            .position(|i| matches!(i, StreamItem::Done { .. }))
            .expect("done");
        assert!(u < d, "usage must be emitted before the terminator");
    }
}

#[test]
fn anthropic_ping_and_content_block_stop_produce_nothing() {
    // The text fixture contains both; neither may appear as content.
    let items = replay_anthropic("text_completion");
    assert_eq!(text(&items), "The test fails.", "no stray frames leaked in");
}

#[test]
fn an_anthropic_mid_stream_error_frame_is_reported_not_treated_as_content() {
    // The API reports overload AFTER a 200, so this must not look like text.
    let mut t = anthropic::EventTranslator::new();
    let err = t
        .push(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#)
        .unwrap_err();
    assert!(
        err.is_retryable(),
        "overload is transient; try the next target"
    );

    let mut t = anthropic::EventTranslator::new();
    let err = t
        .push(r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#)
        .unwrap_err();
    assert!(
        !err.is_retryable(),
        "a bad request fails identically on replay"
    );
}

#[test]
fn an_unknown_anthropic_event_type_does_not_break_the_stream() {
    // Same forward-compatibility discipline as AEP (docs/03).
    let mut t = anthropic::EventTranslator::new();
    assert!(t
        .push(r#"{"type":"some_future_event","payload":{"x":1}}"#)
        .expect("must not fail")
        .is_empty());
}
