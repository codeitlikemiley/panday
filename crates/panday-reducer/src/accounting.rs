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

// `Pricing` lives in `panday-types` (M21.2): the reducer estimates savings with
// it, the gateway meters COGS with it, and the ledger (M11.4) will bill with it.
// A price table owned by any one of those three would make the other two depend
// on it sideways.
pub use panday_types::pricing::Pricing;

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
