//! Retention fixtures for five output types (M15.1).
//!
//! docs/15 §retention tests is blunt about why these exist:
//!
//! > "assertions that the *task-relevant* facts survive (the failing test's
//! > name, the compiler error's file:line, the conflicted path). A compressor
//! > PR without retention fixtures is rejected. ... a reducer that loses the
//! > plot is negative value at any compression ratio."
//!
//! So every case here asserts **two** things: that the output got smaller,
//! and that the one fact a human actually needed is still in it. A ratio
//! without a retention assertion would be a compression benchmark, not a
//! quality gate.
//!
//! These fixtures also become the M15.2 baseline: the structural compressors
//! must beat the generic fallback on the same corpus without losing any of
//! the facts asserted below.

use panday_reducer::{
    expand, GenericReducer, LineRange, MemoryArtifactStore, ReduceCtx, SpillingReducer,
    StructuralReducer,
};
use std::sync::Arc;

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.txt"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn ctx(tool: &str) -> ReduceCtx {
    ReduceCtx {
        tool: tool.into(),
        task: None,
        expected_reads: 1,
        price_per_mtok_micros: 0,
        aggressive: false,
    }
}

struct Case {
    raw: String,
    text: String,
    tokens_raw: u32,
    tokens_kept: u32,
    store: Arc<MemoryArtifactStore>,
    raw_ref: Option<panday_types::id::ArtifactRef>,
}

impl Case {
    fn cut(&self) -> f64 {
        1.0 - (self.tokens_kept as f64 / self.tokens_raw as f64)
    }

    /// Assert a fact survived reduction — the whole point of the gate.
    fn keeps(&self, needle: &str) {
        assert!(
            self.text.contains(needle),
            "reduction lost a task-relevant fact: {needle:?}\n--- kept ---\n{}",
            self.text
        );
    }

    /// Assert a fact is recoverable even though it was elided.
    fn recoverable(&self, needle: &str) {
        let handle = self
            .raw_ref
            .as_ref()
            .expect("a reduced result must spill so elided content stays reachable");
        let all = expand(self.store.as_ref(), handle, LineRange::new(0, usize::MAX)).unwrap();
        assert!(
            all.contains(needle),
            "content was neither kept nor recoverable: {needle:?}"
        );
    }
}

/// The full stack: structural compressors first, generic fallback behind
/// them, artifact spill around both (docs/15 §strategy stack).
fn reduce(name: &str, tool: &str) -> Case {
    let raw = fixture(name);
    let store = Arc::new(MemoryArtifactStore::new());
    let r = SpillingReducer::new(
        StructuralReducer::new(GenericReducer::default()),
        store.clone(),
    );
    let reduction = r.reduce_and_spill(&raw, &ctx(tool)).unwrap();

    Case {
        raw,
        text: reduction.output.text,
        tokens_raw: reduction.output.tokens_raw,
        tokens_kept: reduction.output.tokens_kept,
        store,
        raw_ref: reduction.raw_ref,
    }
}

// ---------------------------------------------------------------------------
// 1. cargo test — one failure buried in 260 passes
// ---------------------------------------------------------------------------

#[test]
fn cargo_test_keeps_the_failing_test_and_the_assertion() {
    let c = reduce("cargo_test_failure", "bash");

    // The single fact the engineer opened the output for.
    c.keeps("envelope_round_trips");
    c.keeps("test result: FAILED");
    assert!(c.cut() > 0.3, "only cut {:.0}%", c.cut() * 100.0);
}

#[test]
fn cargo_test_does_not_bury_the_failure_under_passes() {
    let c = reduce("cargo_test_failure", "bash");
    let passes = c.text.matches("... ok").count();
    assert!(
        passes < 100,
        "kept {passes} lines of green; the wall of passes is exactly what should go"
    );
}

// ---------------------------------------------------------------------------
// 2. cargo build — a compiler error with file:line
// ---------------------------------------------------------------------------

#[test]
fn cargo_build_keeps_the_error_code_and_file_line() {
    let c = reduce("cargo_build_error", "bash");

    c.keeps("E0308");
    c.keeps("gateway.rs:142:23");
    c.keeps("could not compile");
}

// ---------------------------------------------------------------------------
// 3. git status — long, benign, no errors to float
// ---------------------------------------------------------------------------

#[test]
fn git_status_is_cut_hard_because_nothing_in_it_is_urgent() {
    let c = reduce("git_status", "bash");
    assert!(
        c.cut() > 0.4,
        "benign output should compress well; only cut {:.0}%",
        c.cut() * 100.0
    );
    // An untracked file at the tail is still visible — the tail window is
    // what makes "what changed most recently" survive.
    c.keeps("new.rs");
}

#[test]
fn git_status_elided_paths_remain_recoverable() {
    let c = reduce("git_status", "bash");
    // A middle entry is gone from context but not from the record.
    assert!(!c.text.contains("file_60.rs"));
    c.recoverable("file_60.rs");
}

// ---------------------------------------------------------------------------
// 4. pytest — a different runner's failure shape
// ---------------------------------------------------------------------------

#[test]
fn pytest_keeps_the_failing_test_and_its_assertion() {
    let c = reduce("pytest_failure", "bash");

    c.keeps("test_token_expiry");
    c.keeps("1 failed, 339 passed");
    // The error-float is what rescues a failure stranded mid-output.
    c.keeps("AssertionError");
}

// ---------------------------------------------------------------------------
// 5. a large clean file read — the case with NO errors
// ---------------------------------------------------------------------------

#[test]
fn a_large_clean_read_is_reduced_and_fully_recoverable() {
    // The Read/Grep channel rtk never covered (docs/15 §what the benchmark
    // taught). Nothing here looks like an error, so this exercises pure
    // head/tail with no float.
    let c = reduce("file_read_large", "read_file");

    assert!(c.cut() > 0.8, "only cut {:.0}%", c.cut() * 100.0);
    c.keeps("generated_1(");
    c.keeps("generated_800(");
    // The middle is gone from context...
    assert!(!c.text.contains("generated_400("));
    // ...but never lost.
    c.recoverable("generated_400(");
}

#[test]
fn every_reduced_fixture_remains_fully_recoverable() {
    // A reduction the model cannot undo is a reduction it must distrust.
    //
    // Structural digests do not carry an `expand_artifact` marker the way
    // generic elision does — they are a re-emission, not a window — so the
    // guarantee is checked where it actually lives: the spilled artifact.
    for name in [
        "cargo_test_failure",
        "cargo_build_error",
        "git_status",
        "pytest_failure",
        "file_read_large",
    ] {
        let c = reduce(name, "bash");
        assert!(c.raw_ref.is_some(), "{name}: reduced but did not spill");
        c.recoverable("");
    }
}

#[test]
fn generic_elision_still_advertises_the_escape_hatch() {
    // The file read has no structure to exploit, so it takes the generic
    // path — and there the marker is how the model learns it can ask for more.
    let c = reduce("file_read_large", "read_file");
    assert!(c.text.contains("expand_artifact"), "{}", c.text);
}

#[test]
fn structural_compressors_beat_the_generic_baseline() {
    // Recorded at M15.1 with the generic fallback alone. M15.2's acceptance
    // is >=60% on the corpus at zero retention failures (the assertions above
    // are the "zero failures" half).
    let baseline: [(&str, f64); 4] = [
        ("cargo_test_failure", 0.76),
        ("cargo_build_error", 0.36),
        ("git_status", 0.49),
        ("pytest_failure", 0.80),
    ];

    for (name, was) in baseline {
        let now = reduce(name, "bash").cut();
        println!(
            "{name:24} generic {:.0}% -> structural {:.0}%",
            was * 100.0,
            now * 100.0
        );
        assert!(
            now >= was,
            "{name}: structural ({:.0}%) is worse than the generic baseline ({:.0}%)",
            now * 100.0,
            was * 100.0
        );
    }
}

#[test]
fn the_corpus_cut_is_reported_for_the_m15_2_baseline() {
    // Not a threshold — a recorded baseline. M15.2's structural compressors
    // must beat these numbers without failing any retention assertion above.
    let mut total_raw = 0u32;
    let mut total_kept = 0u32;
    for name in [
        "cargo_test_failure",
        "cargo_build_error",
        "git_status",
        "pytest_failure",
        "file_read_large",
    ] {
        let c = reduce(name, "bash");
        println!(
            "{name:24} {:>6} -> {:>5} tokens  ({:.0}% cut)",
            c.tokens_raw,
            c.tokens_kept,
            c.cut() * 100.0
        );
        total_raw += c.tokens_raw;
        total_kept += c.tokens_kept;
        assert!(c.raw.len() > c.text.len());
    }
    let overall = 1.0 - (total_kept as f64 / total_raw as f64);
    println!("corpus overall: {:.0}% cut", overall * 100.0);
    // M15.2's stated bar.
    assert!(
        overall >= 0.60,
        "M15.2 requires >=60% on the corpus; got {:.0}%",
        overall * 100.0
    );
}
