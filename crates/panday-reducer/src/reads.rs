//! File-read dedup and diff-awareness — strategy-stack layer 2 (docs/15).
//!
//! > "re-reading a file the context already holds returns 'unchanged since seq
//! > N' or a hunk-diff against the held version. (This is the Read/Grep
//! > coverage rtk lacked — and it must cooperate with the cache: the *original*
//! > read stays verbatim in the rolling window; the dedup applies to the new
//! > result only.)"
//!
//! That parenthesis is the whole design constraint. Rewriting the *original*
//! read to a stub would churn a cached prefix, and ADR-008 prices that at 1x
//! instead of ~0.1x — a "saving" that costs money. So nothing already sent is
//! touched; only the new result shrinks.
//!
//! This is the channel the JetBrains benchmark showed rtk never covered:
//! agents re-read the same files constantly, and in a long session those
//! re-reads dominate.

use crate::artifact::content_hash;
use std::collections::HashMap;
use std::sync::Mutex;

/// What a re-read should return.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadOutcome {
    /// Never seen: send it verbatim.
    First(String),
    /// Byte-identical to what context already holds.
    Unchanged { seq: u64 },
    /// Changed since `seq`; here is the difference.
    Changed { seq: u64, diff: String },
}

impl ReadOutcome {
    /// The text to hand the model.
    pub fn text(&self, path: &str) -> String {
        match self {
            ReadOutcome::First(body) => body.clone(),
            ReadOutcome::Unchanged { seq } => {
                format!("{path}: unchanged since seq {seq}. The contents are already in context.")
            }
            ReadOutcome::Changed { seq, diff } => {
                format!("{path}: changed since seq {seq}.\n{diff}")
            }
        }
    }

    pub fn strategy(&self) -> &'static str {
        match self {
            ReadOutcome::First(_) => "read_first_v1",
            ReadOutcome::Unchanged { .. } => "read_unchanged_v1",
            ReadOutcome::Changed { .. } => "read_diff_v1",
        }
    }
}

/// Remembers what the context already holds, per file.
///
/// Scoped to one session: two sessions must not share, or one would be told
/// "unchanged" about content it has never seen.
#[derive(Default)]
pub struct ReadLedger {
    held: Mutex<HashMap<String, Held>>,
}

struct Held {
    hash: String,
    lines: Vec<String>,
    seq: u64,
}

impl ReadLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a read and say what should be returned for it.
    ///
    /// `seq` is the event-log position the content is being sent at, so
    /// "unchanged since seq N" points at something the model can actually
    /// find in its own transcript.
    pub fn observe(&self, path: &str, body: &str, seq: u64) -> ReadOutcome {
        let hash = content_hash(body.as_bytes());
        let mut held = self.held.lock().unwrap();

        match held.get_mut(path) {
            None => {
                held.insert(
                    path.to_string(),
                    Held {
                        hash,
                        lines: body.lines().map(str::to_string).collect(),
                        seq,
                    },
                );
                ReadOutcome::First(body.to_string())
            }
            Some(previous) if previous.hash == hash => ReadOutcome::Unchanged { seq: previous.seq },
            Some(previous) => {
                let new_lines: Vec<String> = body.lines().map(str::to_string).collect();
                let diff = hunk_diff(&previous.lines, &new_lines);
                let at = previous.seq;

                // The held version becomes the new one: the next re-read
                // should diff against what the model most recently saw, not
                // against the original.
                previous.hash = hash;
                previous.lines = new_lines;
                previous.seq = seq;

                ReadOutcome::Changed { seq: at, diff }
            }
        }
    }

    /// Forget a file — used when a write invalidates our belief about it.
    pub fn invalidate(&self, path: &str) {
        self.held.lock().unwrap().remove(path);
    }

    pub fn len(&self) -> usize {
        self.held.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A minimal unified-style diff with ±2 lines of context.
///
/// docs/15: "diffs keep hunks, drop context lines beyond ±2". Hand-rolled over
/// an LCS table rather than adding a diff crate — the input is two line vectors
/// and the output is for a model to read, not to `patch`.
pub fn hunk_diff(old: &[String], new: &[String]) -> String {
    const CONTEXT: usize = 2;

    // Guard the quadratic table: a diff of two enormous files is neither
    // cheap nor useful, and the honest answer is to say so.
    if old.len() * new.len() > 4_000_000 {
        return format!(
            "[files too large to diff: {} lines -> {} lines; re-read explicitly if needed]",
            old.len(),
            new.len()
        );
    }

    let ops = lcs_ops(old, new);

    // Mark which ops to print: every change, plus CONTEXT either side.
    let mut keep = vec![false; ops.len()];
    for (i, op) in ops.iter().enumerate() {
        if matches!(op, Op::Add(_) | Op::Del(_)) {
            let from = i.saturating_sub(CONTEXT);
            let to = (i + CONTEXT + 1).min(ops.len());
            for slot in keep.iter_mut().take(to).skip(from) {
                *slot = true;
            }
        }
    }

    let mut out = String::new();
    let mut skipped = 0usize;
    for (i, op) in ops.iter().enumerate() {
        if !keep[i] {
            skipped += 1;
            continue;
        }
        if skipped > 0 {
            out.push_str(&format!("… {skipped} unchanged lines …\n"));
            skipped = 0;
        }
        match op {
            Op::Same(l) => out.push_str(&format!("  {l}\n")),
            Op::Add(l) => out.push_str(&format!("+ {l}\n")),
            Op::Del(l) => out.push_str(&format!("- {l}\n")),
        }
    }
    if skipped > 0 {
        out.push_str(&format!("… {skipped} unchanged lines …\n"));
    }
    if out.is_empty() {
        // Different hashes but no line difference: a trailing-newline change.
        out.push_str("[whitespace-only change]\n");
    }
    out
}

enum Op {
    Same(String),
    Add(String),
    Del(String),
}

/// Longest-common-subsequence backtrace, producing an edit script.
fn lcs_ops(old: &[String], new: &[String]) -> Vec<Op> {
    let (n, m) = (old.len(), new.len());
    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old[i] == new[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    let mut ops = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old[i] == new[j] {
            ops.push(Op::Same(old[i].clone()));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            ops.push(Op::Del(old[i].clone()));
            i += 1;
        } else {
            ops.push(Op::Add(new[j].clone()));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Del(old[i].clone()));
        i += 1;
    }
    while j < m {
        ops.push(Op::Add(new[j].clone()));
        j += 1;
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn a_first_read_is_verbatim() {
        let l = ReadLedger::new();
        let body = "fn main() {}\n";
        assert_eq!(
            l.observe("src/main.rs", body, 5),
            ReadOutcome::First(body.to_string())
        );
    }

    #[test]
    fn an_identical_re_read_points_at_the_original_seq() {
        let l = ReadLedger::new();
        l.observe("a.rs", "same", 5);
        // Read again at a later seq: the answer must cite where the content
        // actually IS in the transcript, not where it was asked for again.
        assert_eq!(
            l.observe("a.rs", "same", 11),
            ReadOutcome::Unchanged { seq: 5 }
        );
    }

    #[test]
    fn the_dedup_message_tells_the_model_where_to_look() {
        let l = ReadLedger::new();
        l.observe("a.rs", "x", 5);
        let text = l.observe("a.rs", "x", 9).text("a.rs");
        assert!(text.contains("unchanged since seq 5"), "{text}");
        assert!(text.contains("already in context"), "{text}");
    }

    #[test]
    fn a_changed_re_read_returns_a_diff_not_the_whole_file() {
        let l = ReadLedger::new();
        let before = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let after = before.replace("line 50", "line 50 CHANGED");

        l.observe("big.rs", &before, 5);
        let outcome = l.observe("big.rs", &after, 9);

        let ReadOutcome::Changed { seq, diff } = &outcome else {
            panic!("expected a diff, got {outcome:?}");
        };
        assert_eq!(*seq, 5);
        assert!(diff.contains("+ line 50 CHANGED"));
        assert!(diff.contains("- line 50"));
        assert!(
            diff.len() < after.len() / 2,
            "the diff should be much smaller than the file"
        );
        // Unchanged bulk is summarised, not printed.
        assert!(diff.contains("unchanged lines"));
    }

    #[test]
    fn the_next_diff_is_against_the_most_recent_version() {
        // Otherwise the model would be shown changes it has already seen.
        let l = ReadLedger::new();
        l.observe("a.rs", "one", 1);
        l.observe("a.rs", "two", 2);
        let outcome = l.observe("a.rs", "three", 3);

        let ReadOutcome::Changed { seq, diff } = outcome else {
            panic!("expected a diff");
        };
        assert_eq!(seq, 2, "must diff against the last version sent");
        assert!(diff.contains("- two"));
        assert!(
            !diff.contains("one"),
            "the original is not re-diffed: {diff}"
        );
    }

    #[test]
    fn different_files_do_not_shadow_each_other() {
        let l = ReadLedger::new();
        l.observe("a.rs", "same body", 1);
        // Same content, different path: still a first read.
        assert!(matches!(
            l.observe("b.rs", "same body", 2),
            ReadOutcome::First(_)
        ));
    }

    #[test]
    fn invalidating_forgets_a_file() {
        // A write means our belief about the file is stale.
        let l = ReadLedger::new();
        l.observe("a.rs", "x", 1);
        l.invalidate("a.rs");
        assert!(matches!(l.observe("a.rs", "x", 2), ReadOutcome::First(_)));
    }

    #[test]
    fn a_diff_keeps_context_but_drops_the_rest() {
        let old = lines("a\nb\nc\nd\ne\nf\ng\nh\ni\nj");
        let new = lines("a\nb\nc\nd\nE\nf\ng\nh\ni\nj");
        let diff = hunk_diff(&old, &new);

        assert!(diff.contains("+ E"));
        assert!(diff.contains("- e"));
        // ±2 context lines around the change.
        assert!(diff.contains("  c") && diff.contains("  d"));
        assert!(diff.contains("  f") && diff.contains("  g"));
        // Far-away lines are elided.
        assert!(!diff.contains("  a"), "{diff}");
        assert!(!diff.contains("  j"), "{diff}");
    }

    #[test]
    fn an_added_block_shows_as_additions() {
        let old = lines("one\ntwo");
        let new = lines("one\ninserted\ntwo");
        let diff = hunk_diff(&old, &new);
        assert!(diff.contains("+ inserted"));
        assert!(!diff.contains("- "), "nothing was deleted: {diff}");
    }

    #[test]
    fn enormous_files_report_rather_than_hang() {
        // The LCS table is quadratic; a 3000x3000 diff is neither cheap nor
        // useful, and pretending otherwise would stall a turn.
        let old: Vec<String> = (0..3_000).map(|i| format!("a{i}")).collect();
        let new: Vec<String> = (0..3_000).map(|i| format!("b{i}")).collect();
        let diff = hunk_diff(&old, &new);
        assert!(diff.contains("too large to diff"), "{diff}");
    }
}
