//! # panday-reducer
//!
//! Every tool result passes through here before it enters context
//! (docs/15-reducer.md, ADR-007). This seed ships the trait plus a working
//! generic fallback: head/tail windows with error-line floating — strategy
//! stack layer 3. Structural compressors (cargo/git/test) are M15.2.

use panday_types::event::ReducedOutput;
use panday_types::model::TaskClass;

/// Context the reducer needs to price its decisions (docs/15 §accounting).
#[derive(Debug, Clone)]
pub struct ReduceCtx {
    pub tool: String,
    pub task: Option<TaskClass>,
    /// Estimated remaining turns this result will ride along for.
    pub expected_reads: u32,
    /// Marginal price per (fresh) input token, micro-credits.
    pub price_per_token_micros: u64,
    /// Profile: conservative (interactive) | aggressive (background subagents).
    pub aggressive: bool,
}

pub mod accounting;
pub mod artifact;
pub mod reads;
pub mod spill;
pub mod structural;

pub use accounting::{value_of, CacheState, Pricing, Savings, SessionSavings};
pub use artifact::{expand, ArtifactError, ArtifactStore, LineRange, MemoryArtifactStore};
pub use reads::{hunk_diff, ReadLedger, ReadOutcome};
pub use spill::{Reduction, SpillingReducer};
pub use structural::{detect, Shape, StructuralReducer};

pub trait Reducer: Send + Sync {
    fn reduce(&self, raw: &str, ctx: &ReduceCtx) -> ReducedOutput;
}

/// ~4 chars/token; good enough for sizing decisions (never for billing).
pub fn approx_tokens(s: &str) -> u32 {
    (s.len() as u32).div_ceil(4)
}

/// Layer-3 generic fallback: keep head + tail windows, float error-looking
/// lines into the kept region, elide the middle with an expandable marker.
pub struct GenericReducer {
    pub head_lines: usize,
    pub tail_lines: usize,
    pub max_kept_error_lines: usize,
    /// Lines kept *after* a matched error line.
    ///
    /// Errors are almost never one line. A Rust diagnostic puts its location
    /// on the following line (`--> file.rs:142:23`), a Python traceback puts
    /// the assertion under the header, and a test runner puts `left`/`right`
    /// below the panic. Floating only the matching line keeps the word
    /// "error" and drops the part an engineer actually needed — the exact
    /// failure docs/15 calls "negative value at any compression ratio".
    pub error_context_lines: usize,
}

impl Default for GenericReducer {
    fn default() -> Self {
        Self {
            head_lines: 30,
            tail_lines: 30,
            max_kept_error_lines: 40,
            error_context_lines: 3,
        }
    }
}

const ERROR_MARKERS: [&str; 7] = [
    "error",
    "err!",
    "fail",
    "panic",
    "warning",
    "exception",
    "fatal",
];

fn looks_like_error(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ERROR_MARKERS.iter().any(|m| lower.contains(m))
}

impl Reducer for GenericReducer {
    fn reduce(&self, raw: &str, ctx: &ReduceCtx) -> ReducedOutput {
        let tokens_raw = approx_tokens(raw);
        let lines: Vec<&str> = raw.lines().collect();
        let (head_n, tail_n) = if ctx.aggressive {
            (self.head_lines / 2, self.tail_lines / 2)
        } else {
            (self.head_lines, self.tail_lines)
        };

        if lines.len() <= head_n + tail_n {
            return ReducedOutput {
                text: raw.to_string(),
                tokens_raw,
                tokens_kept: tokens_raw,
                strategy: "passthrough".into(),
            };
        }

        let middle = &lines[head_n..lines.len() - tail_n];

        // Float error lines *with the lines that explain them*, preserving
        // original order and never keeping a line twice when two errors sit
        // close together.
        let mut keep = vec![false; middle.len()];
        let mut kept_count = 0usize;
        for (i, line) in middle.iter().enumerate() {
            if !looks_like_error(line) {
                continue;
            }
            let end = (i + 1 + self.error_context_lines).min(middle.len());
            for slot in keep.iter_mut().take(end).skip(i) {
                if !*slot {
                    if kept_count >= self.max_kept_error_lines {
                        break;
                    }
                    *slot = true;
                    kept_count += 1;
                }
            }
            if kept_count >= self.max_kept_error_lines {
                break;
            }
        }

        let floated: Vec<&str> = middle
            .iter()
            .copied()
            .zip(keep.iter())
            .filter_map(|(l, k)| k.then_some(l))
            .collect();

        let elided = middle.len() - floated.len();
        let mut out = String::new();
        out.push_str(&lines[..head_n].join("\n"));
        if !floated.is_empty() {
            out.push_str("\n[… error/warning lines floated from elided region …]\n");
            out.push_str(&floated.join("\n"));
        }
        out.push_str(&format!(
            "\n[… {elided} lines elided — expand_artifact(raw_ref, {head_n}..{}) for ranges …]\n",
            lines.len() - tail_n
        ));
        out.push_str(&lines[lines.len() - tail_n..].join("\n"));

        let tokens_kept = approx_tokens(&out);
        ReducedOutput {
            text: out,
            tokens_raw,
            tokens_kept,
            strategy: "generic_headtail_v1".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ReduceCtx {
        ReduceCtx {
            tool: "bash".into(),
            task: None,
            expected_reads: 4,
            price_per_token_micros: 3,
            aggressive: false,
        }
    }

    #[test]
    fn short_output_passes_through() {
        let r = GenericReducer::default().reduce("ok\nfine\n", &ctx());
        assert_eq!(r.strategy, "passthrough");
        assert_eq!(r.tokens_raw, r.tokens_kept);
    }

    #[test]
    fn long_output_shrinks_but_keeps_the_error() {
        let mut raw = String::new();
        for i in 0..500 {
            raw.push_str(&format!("line {i} all good here\n"));
        }
        raw.insert_str(
            raw.len() / 2,
            "error[E0308]: mismatched types at src/lib.rs:42\n",
        );
        for i in 0..40 {
            raw.push_str(&format!("tail {i}\n"));
        }
        let r = GenericReducer::default().reduce(&raw, &ctx());
        assert!(r.tokens_kept < r.tokens_raw / 2, "should cut >50%");
        // retention: the failure fact must survive compression (docs/15 §retention)
        assert!(r.text.contains("E0308"), "the error line must survive");
        assert!(r.text.contains("elided"), "must mark the elision");
    }
}
