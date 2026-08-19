//! Reduce-and-spill: the pipeline the harness actually calls (docs/15 M15.1).
//!
//! ```text
//! raw ──▶ Reducer ──▶ ReducedOutput ──┐
//!   └───▶ ArtifactStore (full raw) ───┴──▶ Reduction { output, raw_ref }
//! ```
//!
//! Spilling is what makes reduction *reversible*, and reversibility is what
//! makes aggressive reduction safe. A reducer that discards is a reducer you
//! must configure timidly.

use crate::artifact::{ArtifactError, ArtifactStore};
use crate::{ReduceCtx, Reducer};
use panday_types::event::ReducedOutput;
use panday_types::id::ArtifactRef;

/// A reduction plus the handle to everything it left out.
#[derive(Debug, Clone, PartialEq)]
pub struct Reduction {
    pub output: ReducedOutput,
    /// `None` only when nothing was dropped — see [`SpillingReducer`].
    pub raw_ref: Option<ArtifactRef>,
}

/// Wraps any [`Reducer`] with artifact spill.
pub struct SpillingReducer<R> {
    inner: R,
    store: std::sync::Arc<dyn ArtifactStore>,
    /// Below this, don't spill even if a byte was trimmed — see `should_spill`.
    min_spill_bytes: usize,
}

impl<R: Reducer> SpillingReducer<R> {
    pub fn new(inner: R, store: std::sync::Arc<dyn ArtifactStore>) -> Self {
        Self {
            inner,
            store,
            min_spill_bytes: 512,
        }
    }

    pub fn min_spill_bytes(mut self, n: usize) -> Self {
        self.min_spill_bytes = n;
        self
    }

    /// Spill only when the raw output is *both* materially reduced and big
    /// enough to be worth a round trip.
    ///
    /// Storing a blob nobody will fetch costs storage and a hash for nothing;
    /// the model will never call `expand_artifact` on a result it can already
    /// see in full. When the reducer passed the text through unchanged there
    /// is by definition nothing to expand.
    fn should_spill(&self, raw: &str, output: &ReducedOutput) -> bool {
        output.text.len() < raw.len() && raw.len() >= self.min_spill_bytes
    }

    pub fn reduce_and_spill(&self, raw: &str, ctx: &ReduceCtx) -> Result<Reduction, ArtifactError> {
        let output = self.inner.reduce(raw, ctx);

        let raw_ref = if self.should_spill(raw, &output) {
            Some(self.store.put(raw.as_bytes(), Some("text/plain".into()))?)
        } else {
            None
        };

        Ok(Reduction { output, raw_ref })
    }
}

/// Still a `Reducer`, so it drops into anything expecting one — the spill is
/// simply invisible through that interface.
impl<R: Reducer> Reducer for SpillingReducer<R> {
    fn reduce(&self, raw: &str, ctx: &ReduceCtx) -> ReducedOutput {
        self.inner.reduce(raw, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::{expand, LineRange, MemoryArtifactStore};
    use crate::GenericReducer;
    use std::sync::Arc;

    fn ctx() -> ReduceCtx {
        ReduceCtx {
            tool: "bash".into(),
            task: None,
            expected_reads: 1,
            price_per_mtok_micros: 0,
            aggressive: false,
        }
    }

    fn big_output() -> String {
        (0..500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_reduced_result_spills_its_raw_and_stays_expandable() {
        // The core M15.1 claim: nothing the reducer drops is actually lost.
        let store = Arc::new(MemoryArtifactStore::new());
        let r = SpillingReducer::new(GenericReducer::default(), store.clone());

        let raw = big_output();
        let reduction = r.reduce_and_spill(&raw, &ctx()).unwrap();

        assert!(reduction.output.tokens_kept < reduction.output.tokens_raw);
        let handle = reduction.raw_ref.expect("a reduced result must spill");

        // A line the reduction elided is still retrievable, verbatim.
        assert!(!reduction.output.text.contains("line 250"));
        let recovered = expand(store.as_ref(), &handle, LineRange::new(250, 251)).unwrap();
        assert!(recovered.contains("line 250"), "{recovered}");
    }

    #[test]
    fn small_untouched_output_does_not_spill() {
        // Nothing was dropped, so there is nothing to expand; a blob here is
        // pure cost.
        let store = Arc::new(MemoryArtifactStore::new());
        let r = SpillingReducer::new(GenericReducer::default(), store.clone());

        let reduction = r
            .reduce_and_spill("all good\n2 tests passed", &ctx())
            .unwrap();
        assert_eq!(reduction.raw_ref, None);
        assert!(store.is_empty(), "stored a blob nobody can use");
    }

    #[test]
    fn the_spilled_bytes_are_the_raw_not_the_reduction() {
        // Spilling the reduced form would make expansion a lie.
        let store = Arc::new(MemoryArtifactStore::new());
        let r = SpillingReducer::new(GenericReducer::default(), store.clone());

        let raw = big_output();
        let handle = r.reduce_and_spill(&raw, &ctx()).unwrap().raw_ref.unwrap();
        assert_eq!(store.get(&handle).unwrap(), raw.as_bytes());
        assert_eq!(handle.size, raw.len() as u64);
    }

    #[test]
    fn the_elision_marker_points_at_a_range_that_actually_expands() {
        // The marker tells the model what to ask for; if those numbers do not
        // resolve, the escape hatch is decorative.
        let store = Arc::new(MemoryArtifactStore::new());
        let r = SpillingReducer::new(GenericReducer::default(), store.clone());

        let raw = big_output();
        let reduction = r.reduce_and_spill(&raw, &ctx()).unwrap();
        let text = &reduction.output.text;

        let marker = text
            .lines()
            .find(|l| l.contains("expand_artifact"))
            .expect("a reduced result must tell the model how to recover the rest");

        let range = marker
            .split_whitespace()
            .find_map(LineRange::parse)
            .expect("the marker must carry a parseable range");

        let recovered = expand(
            store.as_ref(),
            &reduction.raw_ref.unwrap(),
            LineRange::new(range.start, range.start + 3),
        )
        .unwrap();
        assert!(recovered.contains(&format!("line {}", range.start)));
    }

    #[test]
    fn identical_results_share_one_blob() {
        let store = Arc::new(MemoryArtifactStore::new());
        let r = SpillingReducer::new(GenericReducer::default(), store.clone());
        let raw = big_output();

        let a = r.reduce_and_spill(&raw, &ctx()).unwrap();
        let b = r.reduce_and_spill(&raw, &ctx()).unwrap();
        assert_eq!(a.raw_ref, b.raw_ref);
        assert_eq!(store.len(), 1, "re-running a command must not re-store it");
    }
}
