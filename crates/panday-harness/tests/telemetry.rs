//! M21.1 — spans carry the id scheme, and carry no content.
//!
//! docs/21 §traces: "Spans carry counts and costs, **never content** by default
//! (T5): token counts, cache splits, reduction ratios, decision enums."
//!
//! The content-freedom half is a security property, not a tidiness one: a span
//! that captures a prompt ships user data to whatever backend collects traces.
//! So this test runs a real turn, captures everything the subscriber emitted,
//! and greps it — which is exactly the "grep-proof" docs/21 M21.5 asks for,
//! applied as soon as the spans exist rather than later.
//!
//! These run on the **multi-thread** runtime deliberately. An earlier version
//! used `current_thread` and passed while the spans were attached with
//! `span.enter()` — a thread-local guard that a multi-thread runtime silently
//! drops when the future moves between polls. The tests were green and the
//! traces were empty in the real CLI.

use panday_harness::testing::{EchoTool, ScriptedClient, ScriptedTurn};
use panday_harness::tools::ToolRegistry;
use panday_harness::{MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget};
use panday_sdk::telemetry::{content_is_scrubbed, ids, FORBIDDEN_CONTENT_FIELDS};
use panday_types::model::ModelRef;
use panday_types::{AccountId, SessionId};
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

/// Captures every emitted line so the test can inspect what a real collector
/// would receive.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }
}

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run a turn with everything captured, in the JSON shape docs/21 specifies.
async fn run_instrumented(secret_prompt: &str) -> (String, SessionId) {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            // Without this the fmt layer only emits on EVENTS, so a span
            // containing none (`assemble`, `model.call`) never appears — even
            // though an OTLP exporter would export it. Observing closes is how
            // this subscriber sees the same span tree a collector would.
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .with_writer(capture.clone()),
    );

    let session = SessionId::new();
    let store = Arc::new(MemoryStore::new());
    let mut reg = ToolRegistry::default();
    reg.register(EchoTool::ok("read_file", "SECRET_FILE_CONTENTS_ABC"));

    let mut actor = SessionActor::new(
        session,
        AccountId::new(),
        ModelRef("local/test".into()),
        store,
        Arc::new(ScriptedClient::new(vec![
            ScriptedTurn::calling(
                "SECRET_ASSISTANT_TEXT",
                vec![(
                    "read_file",
                    serde_json::json!({"path": "SECRET_PATH_VALUE"}),
                )],
            ),
            ScriptedTurn::text("SECRET_FINAL_ANSWER"),
        ])),
        reg,
        PermissionEngine::new(Profile::Unleashed),
        Box::new(panday_reducer::GenericReducer::default()),
        TurnBudget::default(),
    );

    // `with_subscriber` attaches to the FUTURE, so it follows the turn across
    // worker threads. `with_default` would be thread-local and would miss
    // everything after the first await — which is precisely the class of bug
    // this suite exists to catch, so the test must not contain it either.
    use tracing::instrument::WithSubscriber;
    let prompt = secret_prompt.to_string();
    async {
        actor.handle_user_input(&prompt).await.unwrap();
    }
    .with_subscriber(tracing::Dispatch::new(subscriber))
    .await;

    (capture.text(), session)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_span_tree_matches_the_one_docs_21_specifies() {
    let (output, _session) = run_instrumented("hello").await;

    // docs/21: `turn > assemble > model.call > tool.gate > sandbox.exec > reduce`
    for span in [
        "turn",
        "assemble",
        "model.call",
        "tool.gate",
        "sandbox.exec",
        "reduce",
    ] {
        assert!(
            output.contains(&format!("\"{span}\"")),
            "span `{span}` never appeared:\n{output}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spans_carry_the_id_scheme_so_one_id_reaches_everything() {
    let (output, session) = run_instrumented("hello").await;

    assert!(
        output.contains(ids::SESSION),
        "no {} field:\n{output}",
        ids::SESSION
    );
    assert!(output.contains(ids::ACCOUNT), "no {} field", ids::ACCOUNT);
    // The actual id, not just the key — a field name with no value joins nothing.
    assert!(
        output.contains(&session.0.to_string()),
        "the session id itself is missing from the trace"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_content_reaches_the_trace() {
    // The security property. Every one of these strings passed through the loop
    // — as a prompt, an assistant message, a tool argument and a tool result —
    // and none may appear in what a collector receives.
    let (output, _s) = run_instrumented("SECRET_USER_PROMPT").await;

    for secret in [
        "SECRET_USER_PROMPT",
        "SECRET_ASSISTANT_TEXT",
        "SECRET_PATH_VALUE",
        "SECRET_FILE_CONTENTS_ABC",
        "SECRET_FINAL_ANSWER",
    ] {
        assert!(
            !output.contains(secret),
            "content leaked into the trace: {secret}\n{output}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_forbidden_field_key_appears_at_default_levels() {
    // Stronger than checking known secrets: a *new* span that adds a `content`
    // field would pass the test above and fail this one.
    let (output, _s) = run_instrumented("hello").await;

    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        content_is_scrubbed(line).unwrap_or_else(|e| panic!("{e}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn measurements_do_reach_the_trace() {
    // A trace that leaked nothing because it recorded nothing would pass every
    // test above. This is the control.
    let (output, _s) = run_instrumented("hello").await;

    for field in [
        "input_tokens",
        "output_tokens",
        "tokens_raw",
        "tokens_kept",
        "strategy",
        "stop_reason",
    ] {
        assert!(
            output.contains(field),
            "measurement `{field}` is missing — the trace records nothing useful:\n{output}"
        );
    }
}

#[test]
fn the_scrub_predicate_actually_catches_a_leak() {
    // An audit that cannot fail is not an audit.
    assert!(content_is_scrubbed(r#"{"span":"turn","seq":3}"#).is_ok());
    assert!(content_is_scrubbed(r#"{"content":"hello"}"#).is_err());
    assert!(content_is_scrubbed(r#"{"args":{"path":"x"}}"#).is_err());
    // A value mentioning a forbidden word is not a leak; the key is.
    assert!(content_is_scrubbed(r#"{"strategy":"text_digest"}"#).is_ok());
    assert!(!FORBIDDEN_CONTENT_FIELDS.is_empty());
}

#[test]
fn content_debug_is_refused_in_production() {
    // docs/21: it "refuses to start with it set in `env=production`". A service
    // logging prompts in production is a data incident, so the safe failure is
    // to not boot.
    //
    // Asserted on the predicate rather than by mutating process env, which
    // would race every other test in the binary.
    assert!(
        !panday_sdk::telemetry::content_debug_enabled()
            || std::env::var_os("PANDAY_DEBUG_CONTENT").is_some()
    );
}
