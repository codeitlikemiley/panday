//! Structural compressors — strategy-stack layer 1 (docs/15).
//!
//! > "parse and re-emit canonical terse forms. v1 set, chosen by measured
//! > frequency in coding sessions: cargo build/test, git status/diff/log,
//! > test runners, package managers, ls/find/glob."
//!
//! The rtk move, with the correction the JetBrains benchmark forced: these
//! sit at the harness boundary and see *every* tool result, not just Bash.
//!
//! Each compressor keeps the facts a human opened the output for and collapses
//! the rest to counts. The retention fixtures are the contract — docs/15 is
//! explicit that "a compressor PR without retention fixtures is rejected".

use crate::{approx_tokens, ReduceCtx, Reducer};
use panday_types::event::ReducedOutput;

/// Which compressor a piece of output belongs to.
///
/// Detection is by **content**, not by the command, because the harness sees
/// `bash` for everything — the command line is not in `ReduceCtx`. Sniffing
/// the output is also more honest: `make test` that shells out to cargo
/// should still get the cargo treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    CargoTest,
    CargoBuild,
    Pytest,
    GitStatus,
    Unknown,
}

pub fn detect(raw: &str) -> Shape {
    let head: String = raw.lines().take(400).collect::<Vec<_>>().join("\n");

    // `test result:` is libtest's summary line and is the only unambiguous
    // marker — "running N tests" also appears in other runners' output.
    if head.contains("test result:") {
        return Shape::CargoTest;
    }
    if head.contains("=== test session starts") || head.contains("test session starts") {
        return Shape::Pytest;
    }
    // A compiler diagnostic plus cargo's own progress lines.
    if (head.contains("error[E") || head.contains("warning:") || head.contains("error:"))
        && (head.contains("Compiling ") || head.contains("could not compile"))
    {
        return Shape::CargoBuild;
    }
    if looks_like_git_status(&head) {
        return Shape::GitStatus;
    }
    Shape::Unknown
}

fn looks_like_git_status(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 3 {
        return false;
    }
    let porcelain = lines.iter().filter(|l| is_porcelain_line(l)).count();
    porcelain * 100 / lines.len() > 80
}

/// One `git status --porcelain` entry: `XY path`.
///
/// The status field must contain an actual status letter. An earlier version
/// accepted two spaces, which made **every indented text file** look like git
/// status — a Rust source file was detected as a status listing and rewritten
/// by that compressor, destroying the code. Detection false-positives are the
/// worst failure mode a structural compressor has: the output is not merely
/// under-compressed, it is *wrong*, and confidently so.
fn is_porcelain_line(line: &str) -> bool {
    let bytes: Vec<char> = line.chars().take(3).collect();
    if bytes.len() < 3 || bytes[2] != ' ' {
        return false;
    }
    let is_code = |c: char| matches!(c, 'M' | 'A' | 'D' | 'R' | 'C' | 'U' | '?' | '!');
    let valid = |c: char| c == ' ' || is_code(c);

    valid(bytes[0])
        && valid(bytes[1])
        // At least one real status letter: "  path" is indentation, not status.
        && (is_code(bytes[0]) || is_code(bytes[1]))
        && line.len() > 3
        && !line[3..].starts_with(' ')
}

/// Dispatches to a structural compressor, falling back to `inner`.
///
/// Layer 1 first, generic last — docs/15's "applied in order, first
/// sufficient wins".
pub struct StructuralReducer<F> {
    fallback: F,
}

impl<F: Reducer> StructuralReducer<F> {
    pub fn new(fallback: F) -> Self {
        Self { fallback }
    }
}

impl<F: Reducer> Reducer for StructuralReducer<F> {
    fn reduce(&self, raw: &str, ctx: &ReduceCtx) -> ReducedOutput {
        let out = match detect(raw) {
            Shape::CargoTest => Some(cargo_test(raw)),
            Shape::CargoBuild => Some(cargo_build(raw)),
            Shape::Pytest => Some(pytest(raw)),
            Shape::GitStatus => Some(git_status(raw)),
            Shape::Unknown => None,
        };

        match out {
            // A compressor that made things bigger has failed at its one job;
            // fall back rather than ship a "reduction" that costs tokens.
            Some((text, strategy)) if text.len() < raw.len() => ReducedOutput {
                tokens_raw: approx_tokens(raw),
                tokens_kept: approx_tokens(&text),
                text,
                strategy,
            },
            _ => self.fallback.reduce(raw, ctx),
        }
    }
}

fn finish(kept: Vec<String>, strategy: &str) -> (String, String) {
    (kept.join("\n"), strategy.to_string())
}

// ---------------------------------------------------------------------------
// cargo test
// ---------------------------------------------------------------------------

/// Failures verbatim, passes as a count.
///
/// The wall of `... ok` is the single largest waste in a coding session, and
/// it carries no information a count does not.
fn cargo_test(raw: &str) -> (String, String) {
    let mut kept = Vec::new();
    let mut passed = 0usize;
    let mut in_failure_block = false;

    for line in raw.lines() {
        let t = line.trim();

        if t.ends_with("... ok") || t.ends_with("... ignored") {
            passed += 1;
            continue;
        }
        // Cargo's progress chatter.
        if t.starts_with("Compiling ")
            || t.starts_with("Finished ")
            || t.starts_with("Running ")
            || t.starts_with("Fresh ")
        {
            continue;
        }

        if t == "failures:" {
            in_failure_block = true;
        }
        // Everything about a failure is kept verbatim: the name, the panic,
        // the left/right values. This is the whole reason the output exists.
        if in_failure_block
            || t.contains("FAILED")
            || t.starts_with("test result:")
            || t.contains("panicked at")
            || t.starts_with("assertion")
            || t.starts_with("left:")
            || t.starts_with("right:")
            || t.starts_with("error")
        {
            kept.push(line.to_string());
        }
    }

    if passed > 0 {
        kept.push(format!("[{passed} passing tests elided]"));
    }
    finish(kept, "cargo_test_v1")
}

// ---------------------------------------------------------------------------
// cargo build / check
// ---------------------------------------------------------------------------

/// Diagnostics with their indented context; dependency-compile chatter gone.
///
/// The generic fallback only managed 36% here because a diagnostic is mostly
/// signal — the win is dropping the `Compiling dep_N` preamble, not trimming
/// the error.
fn cargo_build(raw: &str) -> (String, String) {
    let mut kept = Vec::new();
    let mut compiling = 0usize;
    let mut notes = 0usize;
    let mut in_diagnostic = false;

    for line in raw.lines() {
        let t = line.trim_start();

        if t.starts_with("Compiling ") || t.starts_with("Fresh ") || t.starts_with("Finished ") {
            compiling += 1;
            in_diagnostic = false;
            continue;
        }

        let starts_diagnostic = t.starts_with("error[")
            || t.starts_with("error:")
            || t.starts_with("warning:")
            || t.starts_with("error: could not compile");

        if starts_diagnostic {
            in_diagnostic = true;
            kept.push(line.to_string());
            continue;
        }

        if in_diagnostic {
            // A diagnostic's body: the `-->` location, the source excerpt and
            // its gutter. The location is the most useful line in the whole
            // output and sits BELOW the header.
            if t.starts_with("-->")
                || t.starts_with('|')
                || t.starts_with('=')
                || t.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                kept.push(line.to_string());
                continue;
            }
            if t.starts_with("note:") {
                // Notes are mostly boilerplate; keep the first, count the rest.
                notes += 1;
                if notes <= 1 {
                    kept.push(line.to_string());
                }
                continue;
            }
            if t.is_empty() {
                in_diagnostic = false;
            }
        }
    }

    if compiling > 0 {
        kept.push(format!("[{compiling} dependency-compile lines elided]"));
    }
    if notes > 1 {
        kept.push(format!("[{} further notes elided]", notes - 1));
    }
    finish(kept, "cargo_build_v1")
}

// ---------------------------------------------------------------------------
// pytest / jest
// ---------------------------------------------------------------------------

fn pytest(raw: &str) -> (String, String) {
    let mut kept = Vec::new();
    let mut passed = 0usize;
    let mut in_failures = false;

    for line in raw.lines() {
        let t = line.trim();

        if t.contains("PASSED") || t.ends_with("✓") {
            passed += 1;
            continue;
        }
        if t.contains("FAILURES") {
            in_failures = true;
        }
        if in_failures
            || t.contains("FAILED")
            || t.contains("ERROR")
            || t.starts_with('E')
            || t.contains(" passed")
            || t.contains(" failed")
            || t.contains("AssertionError")
        {
            kept.push(line.to_string());
        }
    }

    if passed > 0 {
        kept.push(format!("[{passed} passing tests elided]"));
    }
    finish(kept, "pytest_v1")
}

// ---------------------------------------------------------------------------
// git status
// ---------------------------------------------------------------------------

/// Counts per status, with a sample of paths.
///
/// "120 modified files" is what a human takes from `git status`; the full
/// list is one `expand_artifact` away when it matters.
fn git_status(raw: &str) -> (String, String) {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for line in raw.lines() {
        if line.len() < 4 {
            continue;
        }
        let (code, path) = line.split_at(2);
        groups
            .entry(code.trim().to_string())
            .or_default()
            .push(path.trim().to_string());
    }

    fn label(code: &str) -> &str {
        match code {
            "M" => "modified",
            "A" => "added",
            "D" => "deleted",
            "R" => "renamed",
            "??" => "untracked",
            other => other,
        }
    }

    let mut kept = Vec::new();
    for (code, paths) in &groups {
        kept.push(format!("{} ({}):", label(code), paths.len()));
        // A sample, plus the tail — the most recently touched paths are
        // usually the interesting ones.
        for p in paths.iter().take(5) {
            kept.push(format!("  {p}"));
        }
        if paths.len() > 6 {
            kept.push(format!("  [{} more]", paths.len() - 6));
        }
        if paths.len() > 5 {
            kept.push(format!("  {}", paths[paths.len() - 1]));
        }
    }
    finish(kept, "git_status_v1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_each_shape() {
        assert_eq!(
            detect("running 3 tests\ntest a ... ok\ntest result: ok. 3 passed"),
            Shape::CargoTest
        );
        assert_eq!(
            detect("   Compiling foo v0.1.0\nerror[E0308]: mismatched types"),
            Shape::CargoBuild
        );
        assert_eq!(
            detect("============ test session starts ============\ncollected 3 items"),
            Shape::Pytest
        );
        assert_eq!(
            detect(" M src/a.rs\n M src/b.rs\n?? src/c.rs"),
            Shape::GitStatus
        );
        assert_eq!(
            detect("just some prose\nwith no structure at all"),
            Shape::Unknown
        );
    }

    #[test]
    fn indented_source_code_is_not_mistaken_for_git_status() {
        // Regression: an earlier heuristic accepted two leading spaces as a
        // status field, so every indented file was "detected" as git status
        // and rewritten by that compressor — silently destroying the content.
        let rust = (0..40)
            .map(|i| format!("    pub fn generated_{i}(&self) -> u32 {{ {i} }}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(detect(&rust), Shape::Unknown);

        let yaml = "  name: build\n  runs-on: ubuntu\n  steps:\n    - uses: checkout";
        assert_eq!(detect(yaml), Shape::Unknown);

        let markdown = "  - one\n  - two\n  - three\n  - four";
        assert_eq!(detect(markdown), Shape::Unknown);
    }

    #[test]
    fn unknown_shapes_fall_back_rather_than_guessing() {
        let r = StructuralReducer::new(crate::GenericReducer::default());
        let raw = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = r.reduce(&raw, &ctx());
        assert_eq!(out.strategy, "generic_headtail_v1");
    }

    #[test]
    fn a_compressor_that_would_grow_the_output_falls_back() {
        // Short output can compress to something longer than it started;
        // shipping that would cost tokens in the name of saving them.
        let r = StructuralReducer::new(crate::GenericReducer::default());
        let out = r.reduce(" M a\n M b\n?? c", &ctx());
        assert!(out.tokens_kept <= out.tokens_raw);
    }

    fn ctx() -> ReduceCtx {
        ReduceCtx {
            tool: "bash".into(),
            task: None,
            expected_reads: 1,
            price_per_token_micros: 0,
            aggressive: false,
        }
    }
}
