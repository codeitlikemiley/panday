//! M21.5 — the content-scrub audit (docs/21).
//!
//! docs/21 asks for a "grep-proof that no content fields leak into spans/logs at
//! default levels". Two halves, because either one alone is easy to satisfy and
//! wrong:
//!
//! - **Static.** Every `tracing::` macro in the workspace is read and its field
//!   names checked against `FORBIDDEN_CONTENT_FIELDS`. This catches the leak the
//!   moment it is written, in a crate whose tests nobody thought to extend, and
//!   it catches the leak on a code path no test exercises — which is exactly
//!   where a debug log added during an incident tends to live.
//! - **Runtime.** `panday-gateway` and `panday-harness` already assert their own
//!   emitted spans are clean (M21.1). This file adds the metrics surface, which
//!   is scraped by more systems than a trace is.
//!
//! An intentional exception is written as `// scrub-audit: allow — <reason>` on
//! the line above. There are none today; the mechanism exists so that adding one
//! is a visible, reviewed act rather than a silent weakening of this test.

use panday_sdk::telemetry::FORBIDDEN_CONTENT_FIELDS;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/panday-sdk has two ancestors")
        .to_path_buf()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Byte offsets of every `tracing::<macro>!(` / `info_span!(` invocation.
fn macro_call_starts(src: &str) -> Vec<usize> {
    const MACROS: &[&str] = &[
        "trace!",
        "debug!",
        "info!",
        "warn!",
        "error!",
        "event!",
        "trace_span!",
        "debug_span!",
        "info_span!",
        "warn_span!",
        "error_span!",
        "span!",
    ];
    let mut out = Vec::new();
    for m in MACROS {
        let mut from = 0;
        while let Some(at) = src[from..].find(m) {
            let start = from + at;
            from = start + m.len();
            // `tracing::info!` and a bare `info!` both count; `my_info!` does not.
            let before = src[..start].chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            if src[from..].starts_with('(') {
                out.push(from);
            }
        }
    }
    out
}

/// The text between the macro's parentheses, respecting strings and nesting.
fn arg_text(src: &str, open_paren: usize) -> &str {
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    let mut i = open_paren;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else {
            match c {
                '"' => in_str = true,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return &src[open_paren + 1..i];
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    ""
}

/// Split on commas that are not inside a string, a nested call or a closure.
fn top_level_args(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut in_str, mut escaped, mut start) = (0i32, false, false, 0usize);
    for (i, c) in args.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&args[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&args[start..]);
    out
}

/// The field name an argument records, if any.
///
/// `foo = value`, `%foo`, `?foo`, and the shorthand `foo` all name a field; a
/// format string does not.
fn field_name(arg: &str) -> Option<String> {
    let arg = arg.trim();
    if arg.is_empty() || arg.starts_with('"') {
        return None;
    }
    let lhs = match arg.split_once('=') {
        // `a == b` is a value, not an assignment.
        Some((l, r)) if !r.starts_with('=') && !l.ends_with(['!', '<', '>']) => l,
        _ => arg,
    };
    let name = lhs.trim().trim_start_matches(['%', '?']).trim();
    // A dotted or called expression used bare (`%self.model.0`) records under
    // the last segment.
    let name = name.rsplit('.').next().unwrap_or(name);
    if name.is_empty() || name.contains(char::is_whitespace) || name.contains('(') {
        return None;
    }
    Some(name.trim_matches('"').to_string())
}

fn line_of(src: &str, offset: usize) -> usize {
    src[..offset].bytes().filter(|b| *b == b'\n').count() + 1
}

fn allowed_at(src: &str, line: usize) -> bool {
    src.lines()
        .nth(line.saturating_sub(2))
        .is_some_and(|l| l.contains("scrub-audit: allow"))
}

#[test]
fn no_tracing_call_in_the_workspace_records_a_content_field() {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_sources(&root.join("crates"), &mut files);
    rust_sources(&root.join("xtask"), &mut files);
    assert!(
        files.len() > 30,
        "expected the whole workspace, got {}",
        files.len()
    );

    let mut leaks = Vec::new();
    let mut checked = 0usize;
    // Anti-vacuity: the scan must actually reach the two files that carry the
    // instrumentation. A count alone would be satisfied by a scanner that read
    // the right number of the wrong things.
    let mut instrumented_files: Vec<String> = Vec::new();

    for file in &files {
        // A test may log content deliberately — asserting on it is the point of
        // `telemetry.rs`'s suites. The audit covers shipped code.
        let is_test = file.components().any(|c| c.as_os_str() == "tests");
        let src = std::fs::read_to_string(file).expect("utf-8 source");

        let calls = macro_call_starts(&src);
        if !calls.is_empty() && !is_test {
            instrumented_files.push(file.to_string_lossy().to_string());
        }
        for open in calls {
            checked += 1;
            let line = line_of(&src, open);
            if is_test || allowed_at(&src, line) {
                continue;
            }
            for arg in top_level_args(arg_text(&src, open)) {
                if let Some(name) = field_name(arg) {
                    if FORBIDDEN_CONTENT_FIELDS.contains(&name.as_str()) {
                        leaks.push(format!(
                            "{}:{line}: records `{name}` — {}",
                            file.strip_prefix(&root).unwrap_or(file).display(),
                            arg.trim()
                        ));
                    }
                }
            }
        }
    }

    // A scanner that quietly matched nothing would make this test pass for the
    // worst possible reason, so it has to prove it reached the instrumentation.
    assert!(
        checked >= 15,
        "the scanner found only {checked} tracing calls — it is probably broken"
    );
    for expected in [
        "panday-gateway/src/gateway.rs",
        "panday-harness/src/actor.rs",
        "panday-sdk/src/telemetry.rs",
    ] {
        assert!(
            instrumented_files.iter().any(|f| f.contains(expected)),
            "the scan never reached {expected}; found: {instrumented_files:?}"
        );
    }
    assert!(
        leaks.is_empty(),
        "content fields in spans/logs (docs/21 T5):\n  {}",
        leaks.join("\n  ")
    );
}

#[test]
fn the_scanner_actually_catches_a_leak() {
    // Without this, a scanner bug turns the audit above into a test that always
    // passes — the failure mode that makes a security check worse than none.
    let src = r#"
        fn f() {
            tracing::info!(tool = %name, "ran a tool");
            tracing::debug!(prompt = %req.text, "sending");
        }
    "#;
    let mut found = Vec::new();
    for open in macro_call_starts(src) {
        for arg in top_level_args(arg_text(src, open)) {
            if let Some(n) = field_name(arg) {
                if FORBIDDEN_CONTENT_FIELDS.contains(&n.as_str()) {
                    found.push(n);
                }
            }
        }
    }
    assert_eq!(found, vec!["prompt"], "the scanner missed a planted leak");
}

#[test]
fn the_scanner_does_not_flag_the_fields_we_do_record() {
    let src = r#"
        tracing::info_span!("gateway.chat", account_id = %id, model = %m.0, provider = tracing::field::Empty);
        tracing::debug!(tokens_raw = r.tokens_raw, strategy = %r.strategy, is_error = e, "reduced");
        tracing::warn!(retryable, "provider leg failed; walking the chain");
    "#;
    for open in macro_call_starts(src) {
        for arg in top_level_args(arg_text(src, open)) {
            if let Some(n) = field_name(arg) {
                assert!(
                    !FORBIDDEN_CONTENT_FIELDS.contains(&n.as_str()),
                    "false positive on `{n}`"
                );
            }
        }
    }
}

#[test]
fn an_allow_comment_is_honoured_only_on_the_line_above() {
    let src = "// scrub-audit: allow — dev-only\ntracing::debug!(prompt = %p);\ntracing::debug!(prompt = %p);\n";
    assert!(allowed_at(src, 2));
    assert!(!allowed_at(src, 3));
}

#[test]
fn the_metrics_scrape_carries_no_content_and_no_ids() {
    // A metrics endpoint is scraped by more systems than a trace collector is,
    // and it is usually the one exposed without auth (see the endpoint's own
    // note), so it gets the same T5 treatment.
    let m = panday_sdk::metrics::Metrics::new();
    m.observe_call(
        "anthropic",
        "claude-sonnet-4-5",
        "workhorse",
        &panday_types::model::Usage {
            input_tokens: 10,
            ..Default::default()
        },
        Some(0.001),
        std::time::Duration::from_millis(5),
    );
    m.observe_reduction("cargo_test_v1", 100, None);
    m.observe_sandbox_exec("t2_os_jail", std::time::Duration::from_millis(20));

    let text = m.render();
    panday_sdk::telemetry::content_is_scrubbed(&text).expect("scrape is content-free");
    for label in [
        "session_id",
        "account_id",
        "request_id",
        "turn_id",
        "call_id",
    ] {
        assert!(!text.contains(label), "{label} in the scrape:\n{text}");
    }
}

#[test]
fn every_metric_label_key_is_on_a_bounded_allowlist() {
    // The cardinality argument in `metrics.rs` only holds if the label *keys*
    // stay this short list — every one of them takes values from a fixed set.
    // A new key means a new cardinality decision, which is what this test forces
    // someone to make explicitly.
    const ALLOWED: &[&str] = &[
        "pool", "provider", "model", "rule", "task", "tier", "reason", "outcome", "code",
        "strategy", "kind", "le",
    ];
    let m = panday_sdk::metrics::Metrics::new();
    m.observe_call(
        "p",
        "m",
        "pool",
        &panday_types::model::Usage {
            input_tokens: 1,
            ..Default::default()
        },
        Some(0.0),
        std::time::Duration::from_millis(1),
    );
    m.observe_reduction("s", 1, Some(1.0));
    m.observe_sandbox_exec("t", std::time::Duration::from_millis(1));
    m.model_errors.inc(&["p", "m", "c"]);
    m.route_decisions.inc(&["r", "pool", "chat"]);
    m.turn_stop_reasons.inc(&["end_turn"]);
    m.circuit_open.set(&["p"], 1.0);
    m.unpriced_calls.inc(&["p", "m"]);

    for line in m.render().lines() {
        let Some(open) = line.find('{') else { continue };
        let close = line.rfind('}').expect("balanced braces");
        for pair in line[open + 1..close].split(',') {
            let key = pair.split('=').next().unwrap().trim();
            assert!(
                ALLOWED.contains(&key),
                "unreviewed metric label key `{key}` in: {line}"
            );
        }
    }
}
