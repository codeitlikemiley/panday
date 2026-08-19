//! Dollar accounting for reductions (docs/15 §the accounting model, M15.4).
//!
//! > "Metrics emitted per event: `tokens_raw`, `tokens_kept`,
//! > `est_dollars_saved` — **the dashboard number is dollars, never percent**
//! > (ADR-007). An `rtk`-style 90% that saves $0.002 is reported as $0.002."
//!
//! That sentence is the whole reason this module exists. A compression ratio is
//! a number that always looks good; the JetBrains benchmark measured rtk at
//! 60–90% reduction and ~0% net cost change, because the channels it compressed
//! were not the ones being paid for. So savings here are computed in
//! micro-dollars against the session's *actual* cache state, and a reduction
//! that saves nothing reports nothing however impressive its ratio.

use panday_types::model::Usage;

/// Per-million-token prices, in micro-dollars, for one model.
///
/// Micro-dollars (1e-6 USD) rather than floats: money that is summed thousands
/// of times per session should not accumulate binary rounding error, and the
/// ledger (docs/17) is integer-based for the same reason.
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

/// How the reduced text will be paid for, which decides what removing it saves.
#[derive(Debug, Clone, Copy)]
pub struct CacheState {
    /// How many more turns this result would ride along for.
    ///
    /// docs/15: "tokens that would land in the rolling window get re-sent every
    /// turn (≈ turns_remaining × price, cache-read-discounted after first
    /// send)".
    pub turns_remaining: u32,
    /// Whether the rolling window is actually being cached. When it is not,
    /// every re-send is full price and reduction is worth ~10× more.
    pub rolling_window_cached: bool,
}

impl Default for CacheState {
    fn default() -> Self {
        Self {
            turns_remaining: 1,
            rolling_window_cached: true,
        }
    }
}

/// How much a strategy might have cost in lost information.
///
/// docs/15: "structural compressors carry retention tests → near zero; generic
/// elision on *error* output → high penalty (keep more)." Expressed in
/// micro-dollars so it subtracts from the saving directly — a reduction whose
/// risk outweighs its saving reports a *negative* value, which is the honest
/// answer and the one a dashboard should show.
pub fn information_risk_micros(strategy: &str, is_error_output: bool, tokens_removed: u64) -> u64 {
    let per_mtok: u64 = match (strategy, is_error_output) {
        // Retention-tested compressors: the facts are asserted to survive.
        (s, _) if s.starts_with("cargo_") || s.starts_with("pytest_") || s.starts_with("git_") => 0,
        // Nothing was dropped.
        ("passthrough", _) | ("read_unchanged_v1", _) => 0,
        // Generic elision over ERROR output is where a reducer eats the failing
        // test's name — docs/15 calls that negative-value at any ratio.
        (_, true) => 4_000_000,
        (_, false) => 200_000,
    };
    per_mtok.saturating_mul(tokens_removed) / 1_000_000
}

/// What one reduction was worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Savings {
    pub tokens_raw: u32,
    pub tokens_kept: u32,
    /// Gross value of the tokens removed, before risk.
    pub gross_micros: u64,
    pub risk_micros: u64,
    /// The reportable number. Signed: a reduction can be worth less than the
    /// information it cost.
    pub net_micros: i64,
}

impl Savings {
    /// Dollars, for a human.
    pub fn net_dollars(&self) -> f64 {
        self.net_micros as f64 / 1_000_000.0
    }

    /// The ratio — available, but deliberately not the headline (ADR-007).
    pub fn ratio(&self) -> f64 {
        if self.tokens_raw == 0 {
            return 0.0;
        }
        1.0 - (self.tokens_kept as f64 / self.tokens_raw as f64)
    }
}

/// Price a single reduction.
pub fn value_of(
    output: &panday_types::event::ReducedOutput,
    pricing: &Pricing,
    cache: CacheState,
    is_error_output: bool,
) -> Savings {
    let removed = output.tokens_raw.saturating_sub(output.tokens_kept) as u64;

    // Each re-send after the first is charged at the cache-read rate when the
    // window is cached — which is exactly the discount that made rtk's raw
    // ratios worth ~nothing.
    let first_send = pricing.input_per_mtok_micros.saturating_mul(removed) / 1_000_000;
    let resends = cache.turns_remaining.saturating_sub(1) as u64;
    let resend_pct = if cache.rolling_window_cached {
        pricing.cache_read_pct as u64
    } else {
        100
    };
    let resend_cost = pricing
        .input_per_mtok_micros
        .saturating_mul(removed)
        .saturating_mul(resends)
        .saturating_mul(resend_pct)
        / 100_000_000;

    let gross = first_send + resend_cost;
    let risk = information_risk_micros(&output.strategy, is_error_output, removed);

    Savings {
        tokens_raw: output.tokens_raw,
        tokens_kept: output.tokens_kept,
        gross_micros: gross,
        risk_micros: risk,
        net_micros: gross as i64 - risk as i64,
    }
}

/// Per-session roll-up (docs/15 M15.4: "per-session savings report event").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSavings {
    pub reductions: u32,
    pub tokens_raw: u64,
    pub tokens_kept: u64,
    pub gross_micros: u64,
    pub risk_micros: u64,
    pub net_micros: i64,
}

impl SessionSavings {
    pub fn add(&mut self, s: Savings) {
        self.reductions += 1;
        self.tokens_raw += s.tokens_raw as u64;
        self.tokens_kept += s.tokens_kept as u64;
        self.gross_micros += s.gross_micros;
        self.risk_micros += s.risk_micros;
        self.net_micros += s.net_micros;
    }

    pub fn net_dollars(&self) -> f64 {
        self.net_micros as f64 / 1_000_000.0
    }

    /// The dashboard line. Dollars lead; the ratio is parenthetical.
    ///
    /// Deliberately formatted so a large ratio cannot be mistaken for a large
    /// saving: "$0.002 saved (90% smaller)" reads correctly, "90% reduction"
    /// does not.
    pub fn dashboard_line(&self) -> String {
        let ratio = if self.tokens_raw == 0 {
            0.0
        } else {
            1.0 - (self.tokens_kept as f64 / self.tokens_raw as f64)
        };
        format!(
            "${:.4} saved across {} reductions ({:.0}% smaller)",
            self.net_dollars(),
            self.reductions,
            ratio * 100.0
        )
    }
}
