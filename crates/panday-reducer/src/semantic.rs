//! Layer 5 — semantic summarization (M15.6, docs/15 §strategy stack).
//!
//! > "**Semantic summarization** (opt-in, budget-gated): a `cheap`-pool model
//! > (later: our own tuned 2-4B summarizer — Model 2 in 19.5) digests what
//! > structure can't. This is the only strategy that costs tokens to save
//! > tokens — the accounting below decides when it's worth it."
//!
//! ## Why this is not a `Reducer`
//!
//! `Reducer::reduce` is a synchronous pure function, which is right for layers
//! 1–4: they parse text and re-emit it. Layer 5 makes a model call. Squeezing it
//! into the same trait would mean blocking inside `reduce` — on the harness's
//! runtime, in the middle of a turn — so the one strategy that needs IO lives at
//! its own seam and the loop awaits it explicitly.
//!
//! ## The gate is money, not size
//!
//! Every other layer is free, so "is the output big" is enough reason to run it.
//! This one spends tokens, so the only defensible trigger is the accounting from
//! docs/15:
//!
//! ```text
//! value = tokens_removed × expected_reads × marginal_price − summarization_cost − risk
//! ```
//!
//! On a local model `marginal_price` is zero, so the value of summarizing is
//! *negative* — it costs real cloud tokens to save tokens that were free. A tier
//! that ran anyway would be ADR-007's mistake with an extra API call. The gate
//! below refuses, and a test asserts it.

use crate::accounting::information_risk_micros;
use crate::{approx_tokens, ReduceCtx};
use panday_types::event::ReducedOutput;

/// What produces a summary. **This is the swap-in point for Model 2** (M19.5):
/// the tuned 2–4B summarizer implements this trait and nothing else changes.
///
/// `target_tokens` is a budget, not a suggestion — the caller has already priced
/// the saving at that size, and a summarizer that ignores it produces a reduction
/// whose value was calculated for a different output.
#[async_trait::async_trait]
pub trait Summarizer: Send + Sync {
    /// Provider/model label, for the `strategy` field and the metric label. A
    /// reduction has to say what produced it: "semantic" alone would make a
    /// regression untraceable to the model that caused it.
    fn name(&self) -> &str;

    /// Price of summarizing, in micro-dollars, for `input_tokens` of input.
    /// Asked *before* the call, because the decision to call depends on it.
    fn cost_micros(&self, input_tokens: u32, target_tokens: u32) -> u64;

    async fn summarize(&self, text: &str, target_tokens: u32) -> Result<String, SummarizeError>;
}

/// So a caller can hold `Box<dyn Summarizer>` — the harness stores one behind an
/// option and cannot be generic over it.
#[async_trait::async_trait]
impl Summarizer for Box<dyn Summarizer> {
    fn name(&self) -> &str {
        (**self).name()
    }
    fn cost_micros(&self, input_tokens: u32, target_tokens: u32) -> u64 {
        (**self).cost_micros(input_tokens, target_tokens)
    }
    async fn summarize(&self, text: &str, target_tokens: u32) -> Result<String, SummarizeError> {
        (**self).summarize(text, target_tokens).await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SummarizeError {
    #[error("summarizer unavailable: {0}")]
    Unavailable(String),
    #[error("summarizer timed out after {0}ms")]
    Timeout(u64),
}

#[derive(Debug, Clone, Copy)]
pub struct SemanticConfig {
    /// Fraction of the structural output to aim for.
    pub target_ratio: f32,
    /// Don't bother below this — a saving smaller than this is noise, and each
    /// call adds latency to a turn a human is waiting on.
    pub min_value_micros: u64,
    /// Inputs larger than this are left alone: a summarizer's own context has a
    /// limit, and truncating the input to fit would summarize the wrong half.
    pub max_input_tokens: u32,
    /// Never summarize below this, whatever the ratio says.
    pub floor_tokens: u32,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            target_ratio: 0.25,
            // A tenth of a cent. Below that the latency is the bigger cost.
            min_value_micros: 1_000,
            max_input_tokens: 100_000,
            floor_tokens: 200,
        }
    }
}

/// Why the tier ran, or didn't. Returned rather than logged so the caller can
/// record it in the event log — a reduction that silently did not happen is
/// indistinguishable from one that did nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Ran {
        /// Net of the summarizer's own cost.
        value_micros: i64,
    },
    /// The saving did not cover the call.
    NotWorthIt { value_micros: i64 },
    /// Already small enough that the target would be below the floor.
    AlreadySmall,
    /// Bigger than the summarizer's own context.
    TooLarge,
    /// Marginal price is zero — a local model. Summarizing would spend cloud
    /// money to save nothing (ADR-007).
    Free,
    /// The summarizer failed, and the structural output stands.
    Failed(String),
    /// The summary was not smaller, so it was discarded.
    NotSmaller,
}

impl Decision {
    pub fn ran(&self) -> bool {
        matches!(self, Decision::Ran { .. })
    }
}

pub struct SemanticTier<S> {
    summarizer: S,
    config: SemanticConfig,
}

impl<S: Summarizer> SemanticTier<S> {
    pub fn new(summarizer: S) -> Self {
        Self {
            summarizer,
            config: SemanticConfig::default(),
        }
    }

    pub fn with_config(summarizer: S, config: SemanticConfig) -> Self {
        Self { summarizer, config }
    }

    pub fn summarizer(&self) -> &S {
        &self.summarizer
    }

    /// Apply the tier to a structural reduction, or explain why not.
    ///
    /// Never returns an error: a summarizer that is down, slow or wrong must cost
    /// the turn nothing but the structural output it already had. This is the
    /// only layer with a network dependency, and a context pipeline that fails
    /// when a summarizer is unreachable would be a worse design than not having
    /// the layer at all.
    pub async fn apply(
        &self,
        reduced: ReducedOutput,
        ctx: &ReduceCtx,
    ) -> (ReducedOutput, Decision) {
        let kept = reduced.tokens_kept;
        let target = ((kept as f32) * self.config.target_ratio) as u32;

        if ctx.price_per_mtok_micros == 0 {
            return (reduced, Decision::Free);
        }
        if kept > self.config.max_input_tokens {
            return (reduced, Decision::TooLarge);
        }
        if target < self.config.floor_tokens || kept <= self.config.floor_tokens {
            return (reduced, Decision::AlreadySmall);
        }

        // docs/15's formula, with the summarizer's own price subtracted. The risk
        // penalty is the generic-elision one: a summary is a lossy re-write, and
        // over error output that is exactly where docs/15 says to keep more.
        let removed = kept.saturating_sub(target) as u64;
        // Per-million-token prices: divide once, at the end, so a realistic price
        // does not round to zero on the way in.
        let gross = removed
            .saturating_mul(ctx.expected_reads.max(1) as u64)
            .saturating_mul(ctx.price_per_mtok_micros)
            / 1_000_000;
        let cost = self.summarizer.cost_micros(kept, target);
        let risk =
            information_risk_micros(self.summarizer.name(), is_error_shaped(&reduced), removed);
        let value = gross as i64 - cost as i64 - risk as i64;

        if value < self.config.min_value_micros as i64 {
            return (
                reduced,
                Decision::NotWorthIt {
                    value_micros: value,
                },
            );
        }

        match self.summarizer.summarize(&reduced.text, target).await {
            Ok(summary) => {
                let summary_tokens = approx_tokens(&summary);
                if summary_tokens >= kept {
                    // A "summary" that grew is not a reduction. Providers do
                    // return the input verbatim when a prompt confuses them, and
                    // storing that would cost tokens twice for nothing.
                    return (reduced, Decision::NotSmaller);
                }
                (
                    ReducedOutput {
                        text: summary,
                        // `tokens_raw` stays the ORIGINAL raw count, not the
                        // structural output's: the accounting and the dashboard
                        // measure the whole pipeline's saving, and resetting it
                        // here would credit layer 5 for layer 1's work.
                        tokens_raw: reduced.tokens_raw,
                        tokens_kept: summary_tokens,
                        strategy: format!("semantic_v1:{}", self.summarizer.name()),
                    },
                    Decision::Ran {
                        value_micros: value,
                    },
                )
            }
            Err(e) => (reduced, Decision::Failed(e.to_string())),
        }
    }
}

/// Error output gets the higher risk penalty (docs/15). Judged on the text rather
/// than passed in, because by layer 5 the structural compressors have already
/// rewritten it and the caller's `is_error` flag describes the tool's exit status,
/// not what survived.
fn is_error_shaped(reduced: &ReducedOutput) -> bool {
    let head: String = reduced
        .text
        .chars()
        .take(4_000)
        .collect::<String>()
        .to_lowercase();
    ["error", "panic", "failed", "traceback", "assertion"]
        .iter()
        .any(|needle| head.contains(needle))
}
