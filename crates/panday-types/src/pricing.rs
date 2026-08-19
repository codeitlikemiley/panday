//! Model prices, in micro-dollars per million tokens.
//!
//! Three components need the same numbers — the reducer estimates dollars saved
//! (docs/15/ADR-007), the gateway meters COGS (docs/21), the ledger bills
//! (docs/17) — so the type lives at the bottom of the dependency graph with the
//! rest of the protocol vocabulary.
//!
//! Micro-dollars (1e-6 USD) rather than floats: money that is summed thousands
//! of times per session should not accumulate binary rounding error, and the
//! ledger is integer-based for the same reason.

use crate::model::{ModelRef, Usage};
use std::collections::BTreeMap;

/// Per-million-token prices, in micro-dollars, for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pricing {
    /// Fresh input, per million tokens.
    pub input_per_mtok_micros: u64,
    /// Output, per million tokens.
    pub output_per_mtok_micros: u64,
    /// Cache reads as a percentage of fresh input — ~10% at both major
    /// providers (ADR-007/008).
    pub cache_read_pct: u32,
    /// Cache writes at the short TTL, as a percentage of fresh input.
    /// Anthropic charges ~125%; automatic-caching providers charge 100%
    /// (i.e. no premium).
    pub cache_write_pct: u32,
    /// Cache writes at the extended TTL — Anthropic ~200%.
    pub cache_write_1h_pct: u32,
}

impl Pricing {
    /// An Anthropic-style model with explicit caching and a write premium.
    pub fn anthropic_sonnet_class() -> Self {
        Self {
            input_per_mtok_micros: 3_000_000,
            output_per_mtok_micros: 15_000_000,
            cache_read_pct: 10,
            cache_write_pct: 125,
            cache_write_1h_pct: 200,
        }
    }

    /// An automatic-prefix-caching model: reads are discounted, writes are not
    /// surcharged (ADR-007).
    pub fn openai_compat_class() -> Self {
        Self {
            input_per_mtok_micros: 500_000,
            output_per_mtok_micros: 1_500_000,
            cache_read_pct: 10,
            cache_write_pct: 100,
            cache_write_1h_pct: 100,
        }
    }

    /// A local model: no marginal token cost.
    ///
    /// Important that this is zero rather than "cheap": a reduction that saves
    /// tokens on a local model saves **no money**, and reporting a dollar figure
    /// for it would be the exact error ADR-007 warns about.
    pub fn local() -> Self {
        Self {
            input_per_mtok_micros: 0,
            output_per_mtok_micros: 0,
            cache_read_pct: 10,
            cache_write_pct: 100,
            cache_write_1h_pct: 100,
        }
    }

    /// Price a `Usage`, honouring the convention that cache counts are subsets
    /// of `input_tokens` (see `panday_types::model::Usage`).
    pub fn cost_micros(&self, usage: Usage) -> u64 {
        let cached = usage
            .cache_read_tokens
            .saturating_add(usage.cache_write_tokens)
            .saturating_add(usage.cache_write_1h_tokens);
        let fresh = usage.input_tokens.saturating_sub(cached);

        let per = |tokens: u64, pct: u32| -> u64 {
            // Integer maths throughout: mtok price × tokens × pct / (1e6 × 100).
            self.input_per_mtok_micros
                .saturating_mul(tokens)
                .saturating_mul(pct as u64)
                / 100_000_000
        };

        per(fresh, 100)
            + per(usage.cache_read_tokens, self.cache_read_pct)
            + per(usage.cache_write_tokens, self.cache_write_pct)
            + per(usage.cache_write_1h_tokens, self.cache_write_1h_pct)
            + self
                .output_per_mtok_micros
                .saturating_mul(usage.output_tokens)
                / 1_000_000
    }
}

/// Where a dollar figure comes from.
///
/// `None` means *this model has no configured price*, which is not the same
/// claim as "it is free" — and the difference matters, because a metering
/// pipeline that treats an unknown price as zero under-reports COGS silently.
/// Callers must distinguish the two (`panday_unpriced_calls_total` exists for
/// exactly this).
pub trait CostModel: Send + Sync {
    fn cost_micros(&self, model: &ModelRef, usage: Usage) -> Option<u64>;
}

/// Prices keyed by the exact `provider/model` id.
///
/// Exact ids only, no prefix matching: `anthropic/claude-opus-4-1` and
/// `anthropic/claude-sonnet-4-5` differ 5x in price, and a prefix rule that
/// quietly priced one as the other would be a billing bug that looks like a
/// rounding error. The model catalog (M12.2) is where these come from
/// eventually; until then a caller configures what it knows.
#[derive(Debug, Clone, Default)]
pub struct PriceTable(BTreeMap<String, Pricing>);

impl PriceTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, model: impl Into<String>, pricing: Pricing) -> Self {
        self.0.insert(model.into(), pricing);
        self
    }

    pub fn get(&self, model: &ModelRef) -> Option<&Pricing> {
        self.0.get(&model.0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl CostModel for PriceTable {
    fn cost_micros(&self, model: &ModelRef, usage: Usage) -> Option<u64> {
        Some(self.get(model)?.cost_micros(usage))
    }
}

/// Prices nothing. The default, because the alternative default — guessing a
/// price from the provider prefix — would put invented numbers on a cost
/// dashboard, and an invented COGS figure is worse than a missing one.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPrices;

impl CostModel for NoPrices {
    fn cost_micros(&self, _model: &ModelRef, _usage: Usage) -> Option<u64> {
        None
    }
}
