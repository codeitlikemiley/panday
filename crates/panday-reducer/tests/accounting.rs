//! M15.4 — dollar accounting (docs/15).
//!
//! The tests here are mostly about *not* being impressed by ratios. docs/15 is
//! blunt about why: rtk measured 60–90% reduction and the independent benchmark
//! measured ~0% net cost change, because the compressed channels were not the
//! ones being paid for. So every case below asks "what did that save in money",
//! and several answer "nothing".

use panday_reducer::accounting::{information_risk_micros, value_of};
use panday_reducer::{CacheState, Pricing, SessionSavings};
use panday_types::event::ReducedOutput;
use panday_types::model::Usage;

fn reduced(raw: u32, kept: u32, strategy: &str) -> ReducedOutput {
    ReducedOutput {
        text: String::new(),
        tokens_raw: raw,
        tokens_kept: kept,
        strategy: strategy.into(),
    }
}

// ---------------------------------------------------------------------------
// Pricing
// ---------------------------------------------------------------------------

#[test]
fn cache_reads_cost_about_a_tenth_of_fresh_input() {
    // The ADR-008 lever. If this ratio is wrong every savings figure is wrong.
    let p = Pricing::anthropic_sonnet_class();

    let fresh = p.cost_micros(Usage {
        input_tokens: 1_000_000,
        ..Default::default()
    });
    let cached = p.cost_micros(Usage {
        input_tokens: 1_000_000,
        cache_read_tokens: 1_000_000,
        ..Default::default()
    });

    assert_eq!(fresh, 3_000_000, "1M fresh input at $3/Mtok");
    assert_eq!(cached, 300_000, "the same tokens served from cache");
    assert_eq!(fresh / cached, 10);
}

#[test]
fn a_cache_write_costs_more_than_fresh_input_on_anthropic() {
    // The trap ADR-007 names: "naive compression that churns a cached prefix
    // *loses* money". A write premium is why.
    let p = Pricing::anthropic_sonnet_class();

    let fresh = p.cost_micros(Usage {
        input_tokens: 1_000_000,
        ..Default::default()
    });
    let write_5m = p.cost_micros(Usage {
        input_tokens: 1_000_000,
        cache_write_tokens: 1_000_000,
        ..Default::default()
    });
    let write_1h = p.cost_micros(Usage {
        input_tokens: 1_000_000,
        cache_write_1h_tokens: 1_000_000,
        ..Default::default()
    });

    assert!(write_5m > fresh, "a 5m write carries a premium");
    assert_eq!(write_1h, fresh * 2, "a 1h write is ~2x");
}

#[test]
fn an_automatic_caching_provider_has_no_write_premium() {
    // ADR-007: "no write premium on automatic-caching providers".
    let p = Pricing::openai_compat_class();
    let fresh = p.cost_micros(Usage {
        input_tokens: 1_000_000,
        ..Default::default()
    });
    let written = p.cost_micros(Usage {
        input_tokens: 1_000_000,
        cache_write_tokens: 1_000_000,
        ..Default::default()
    });
    assert_eq!(fresh, written);
}

#[test]
fn cache_counts_are_treated_as_subsets_not_additions() {
    // The `Usage` CONVENTION. Treating them as additive would double-count the
    // cached portion and inflate every cost.
    let p = Pricing::anthropic_sonnet_class();
    let all_cached = p.cost_micros(Usage {
        input_tokens: 1_000,
        cache_read_tokens: 1_000,
        ..Default::default()
    });
    let none_cached = p.cost_micros(Usage {
        input_tokens: 1_000,
        ..Default::default()
    });
    assert!(all_cached < none_cached);
    assert_eq!(all_cached, none_cached / 10);
}

// ---------------------------------------------------------------------------
// Savings
// ---------------------------------------------------------------------------

#[test]
fn a_ninety_percent_reduction_that_saves_nothing_reports_nothing() {
    // docs/15's own example, and the single most important test in this file:
    // "An `rtk`-style 90% that saves $0.002 is reported as $0.002."
    //
    // On a LOCAL model there is no marginal token cost at all, so even a
    // spectacular ratio is worth exactly zero.
    let s = value_of(
        &reduced(10_000, 1_000, "generic_headtail_v1"),
        &Pricing::local(),
        CacheState::default(),
        false,
    );

    assert!(s.ratio() > 0.89, "the ratio really is ~90%: {}", s.ratio());
    assert_eq!(
        s.gross_micros, 0,
        "a local model has no marginal token cost"
    );
    assert!(
        s.net_micros <= 0,
        "a 90% reduction on a free model must not report a saving: {s:?}"
    );
}

#[test]
fn the_same_reduction_is_worth_more_when_it_rides_along_for_more_turns() {
    // docs/15: tokens in the rolling window are "re-sent every turn (≈
    // turns_remaining × price, cache-read-discounted after first send)".
    let out = reduced(10_000, 1_000, "cargo_test_v1");
    let p = Pricing::anthropic_sonnet_class();

    let one_turn = value_of(
        &out,
        &p,
        CacheState {
            turns_remaining: 1,
            rolling_window_cached: true,
        },
        false,
    );
    let twenty = value_of(
        &out,
        &p,
        CacheState {
            turns_remaining: 20,
            rolling_window_cached: true,
        },
        false,
    );

    assert!(
        twenty.net_micros > one_turn.net_micros,
        "riding along for 20 turns should be worth more: {} vs {}",
        twenty.net_micros,
        one_turn.net_micros
    );
}

#[test]
fn an_uncached_rolling_window_makes_reduction_worth_about_ten_times_more() {
    // The other half of the cache-awareness point: when re-sends are full
    // price, removing tokens is worth ~10x what it is worth when they are
    // cache-read discounted.
    let out = reduced(10_000, 0, "cargo_test_v1");
    let p = Pricing::anthropic_sonnet_class();
    let turns = CacheState {
        turns_remaining: 11,
        rolling_window_cached: true,
    };
    let uncached = CacheState {
        turns_remaining: 11,
        rolling_window_cached: false,
    };

    let cached_value = value_of(&out, &p, turns, false).gross_micros;
    let uncached_value = value_of(&out, &p, uncached, false).gross_micros;

    assert!(
        uncached_value > cached_value * 4,
        "uncached {uncached_value} should dwarf cached {cached_value}"
    );
}

#[test]
fn eliding_error_output_can_be_worth_less_than_nothing() {
    // docs/15: "a reducer that eats the failing test's name is negative-value
    // at any ratio". The accounting has to be able to SAY that, not just
    // report a smaller positive number.
    let s = value_of(
        &reduced(1_000, 100, "generic_headtail_v1"),
        // A cheap model, so the token saving is small...
        &Pricing::openai_compat_class(),
        CacheState::default(),
        // ...and the output was an error, so the risk is high.
        true,
    );

    assert!(
        s.net_micros < 0,
        "eliding error output cheaply should report a LOSS: {s:?}"
    );
    assert!(s.risk_micros > s.gross_micros);
}

#[test]
fn a_retention_tested_compressor_carries_almost_no_risk() {
    // Structural compressors ship with retention fixtures, so the facts are
    // asserted to survive — docs/15 puts their risk at "near zero".
    let removed = 5_000;
    assert_eq!(information_risk_micros("cargo_test_v1", true, removed), 0);
    assert_eq!(information_risk_micros("pytest_v1", true, removed), 0);
    assert_eq!(information_risk_micros("git_status_v1", false, removed), 0);
    // Generic elision does not get that credit.
    assert!(information_risk_micros("generic_headtail_v1", false, removed) > 0);
    // And over error output it is penalised much harder.
    assert!(
        information_risk_micros("generic_headtail_v1", true, removed)
            > information_risk_micros("generic_headtail_v1", false, removed) * 5
    );
}

#[test]
fn passthrough_removes_nothing_and_therefore_risks_nothing() {
    let s = value_of(
        &reduced(500, 500, "passthrough"),
        &Pricing::anthropic_sonnet_class(),
        CacheState::default(),
        true,
    );
    assert_eq!(s.gross_micros, 0);
    assert_eq!(s.risk_micros, 0);
    assert_eq!(s.net_micros, 0);
    assert_eq!(s.ratio(), 0.0);
}

// ---------------------------------------------------------------------------
// The session report
// ---------------------------------------------------------------------------

#[test]
fn the_session_report_leads_with_dollars_not_percent() {
    // ADR-007: "the dashboard number is dollars, never percent". A ratio in the
    // lead position is how a $0.002 saving gets celebrated as a 90% win.
    let mut total = SessionSavings::default();
    let p = Pricing::anthropic_sonnet_class();
    for _ in 0..5 {
        total.add(value_of(
            &reduced(10_000, 1_000, "cargo_test_v1"),
            &p,
            CacheState {
                turns_remaining: 10,
                rolling_window_cached: true,
            },
            false,
        ));
    }

    let line = total.dashboard_line();
    assert!(line.starts_with('$'), "dollars must lead: {line}");
    assert!(line.contains("5 reductions"), "{line}");
    assert!(
        line.contains("smaller)"),
        "the ratio is parenthetical: {line}"
    );
    assert_eq!(total.reductions, 5);
    assert!(total.net_dollars() > 0.0);
}

#[test]
fn a_session_of_worthless_reductions_reports_zero_dollars() {
    // The honest outcome for a local-only session, however good the ratios look.
    let mut total = SessionSavings::default();
    for _ in 0..20 {
        total.add(value_of(
            &reduced(50_000, 500, "generic_headtail_v1"),
            &Pricing::local(),
            CacheState::default(),
            false,
        ));
    }

    assert_eq!(total.gross_micros, 0);
    assert!(
        total.net_dollars() <= 0.0,
        "a local session must not report a dollar saving: {}",
        total.dashboard_line()
    );
    // The ratio is still enormous, which is exactly why it is not the headline.
    assert!(
        total.dashboard_line().contains("99% smaller"),
        "{}",
        total.dashboard_line()
    );
}
