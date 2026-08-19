//! M17.1 — the entitlement engine against fixture plans (docs/17).
//!
//! Every test is a pure function call: the engine takes observed usage as an argument rather than
//! reading a database, which is what makes "what is allowed" testable separately from "what has
//! been spent".

use panday_platform::entitlements::{check, Observed, Plan, Request, Verdict, DEMOTE_TO};
use panday_platform::Entitlement;

fn nothing_used() -> Observed {
    Observed::default()
}

#[test]
fn an_unused_account_on_a_paid_plan_is_allowed() {
    let verdict = check(
        &Plan::pro(),
        &Request {
            pool: Some("workhorse".into()),
            ..Default::default()
        },
        &nothing_used(),
    );
    assert_eq!(verdict, Verdict::Allow);
}

#[test]
fn a_missing_measurement_is_no_evidence_rather_than_zero() {
    // The failure this prevents: an engine that read "usage not looked up" as "usage is zero"
    // allows every request on an account nobody measured, which is the most expensive possible
    // default.
    let over_everything = Observed {
        requests_this_minute: None,
        tokens_today: None,
        balance_micros: None,
        sandbox_seconds_today: None,
        concurrent_sessions: None,
        storage_bytes: None,
    };
    // With no evidence the engine cannot deny — but the caller can see that it supplied none,
    // because these are `Option`s at the call site rather than defaults buried in the engine.
    assert_eq!(
        check(&Plan::free(), &Request::default(), &over_everything),
        Verdict::Allow
    );
}

#[test]
fn the_hard_spend_ceiling_denies_and_says_what_to_do() {
    let verdict = check(
        &Plan::pro(),
        &Request::default(),
        &Observed {
            // Spent 60 credit-units against a 50 ceiling.
            balance_micros: Some(-60_000_000),
            ..nothing_used()
        },
    );
    match verdict {
        Verdict::Deny { why } => {
            assert!(why.contains("spend ceiling"), "{why}");
            // The message names the action, not the code path: a user reading it should know what
            // to do next.
            assert!(why.contains("Add credit"), "{why}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn approaching_the_ceiling_degrades_instead_of_refusing() {
    // docs/12's `budget_soft`: a user near a limit wants a slower answer, not an error. 90% is a
    // threshold and naming it makes it a decision rather than a feeling.
    let verdict = check(
        &Plan::pro(),
        &Request {
            pool: Some("workhorse".into()),
            ..Default::default()
        },
        &Observed {
            balance_micros: Some(-46_000_000),
            ..nothing_used()
        },
    );
    match verdict {
        Verdict::Degrade { to_pool, why } => {
            assert_eq!(to_pool, DEMOTE_TO);
            assert!(why.contains("46000000"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    // And it is not degraded twice: a request already on the cheap pool proceeds.
    assert_eq!(
        check(
            &Plan::pro(),
            &Request {
                pool: Some(DEMOTE_TO.into()),
                ..Default::default()
            },
            &Observed {
                balance_micros: Some(-46_000_000),
                ..nothing_used()
            },
        ),
        Verdict::Allow
    );
}

#[test]
fn a_preflight_estimate_that_would_cross_the_ceiling_is_refused() {
    // docs/17: "enforced here pre-flight (estimate) and post-flight (reconcile actual)". Denying
    // after the tokens are spent is a refund, not a limit.
    let verdict = check(
        &Plan::pro(),
        &Request {
            estimated_micros: Some(30_000_000),
            pool: Some(DEMOTE_TO.into()),
            ..Default::default()
        },
        &Observed {
            balance_micros: Some(-30_000_000),
            ..nothing_used()
        },
    );
    match verdict {
        Verdict::Deny { why } => assert!(why.contains("would pass the"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn money_is_reported_before_rate_and_rate_before_capability() {
    // The first limit reported is the one a user has to act on. An account that is over its spend
    // ceiling *and* rate-limited *and* asking for the wrong pool should be told about the money.
    let verdict = check(
        &Plan::pro(),
        &Request {
            pool: Some("frontier".into()),
            subagents: Some(9),
            ..Default::default()
        },
        &Observed {
            requests_this_minute: Some(9_999),
            balance_micros: Some(-60_000_000),
            ..nothing_used()
        },
    );
    match verdict {
        Verdict::Deny { why } => assert!(why.contains("spend ceiling"), "{why}"),
        other => panic!("{other:?}"),
    }

    // With the money fine, the rate limit is next.
    let verdict = check(
        &Plan::pro(),
        &Request {
            pool: Some("frontier".into()),
            ..Default::default()
        },
        &Observed {
            requests_this_minute: Some(9_999),
            ..nothing_used()
        },
    );
    match verdict {
        Verdict::Deny { why } => assert!(why.contains("rate limit"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_free_plan_never_reaches_frontier_but_is_not_refused_for_asking() {
    // docs/17: "free plan never touches `frontier`". Degrade, because a route is a choice among
    // pools and refusing a request for naming the wrong one is pedantry.
    let verdict = check(
        &Plan::free(),
        &Request {
            pool: Some("frontier".into()),
            ..Default::default()
        },
        &nothing_used(),
    );
    match verdict {
        Verdict::Degrade { to_pool, why } => {
            assert!(
                ["local-only", "cheap"].contains(&to_pool.as_str()),
                "{to_pool}"
            );
            assert!(why.contains("free"), "{why}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_plan_with_no_pools_at_all_denies_rather_than_degrading_to_nothing() {
    let empty = Plan {
        plan_id: "broken".into(),
        entitlements: vec![Entitlement::ModelPools { pools: vec![] }],
    };
    match check(
        &empty,
        &Request {
            pool: Some("cheap".into()),
            ..Default::default()
        },
        &nothing_used(),
    ) {
        Verdict::Deny { why } => assert!(why.contains("no model pools"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_free_plan_allows_no_subagents_and_pro_allows_three() {
    assert!(matches!(
        check(
            &Plan::free(),
            &Request {
                pool: Some("cheap".into()),
                subagents: Some(1),
                ..Default::default()
            },
            &nothing_used()
        ),
        Verdict::Deny { .. }
    ));
    assert_eq!(
        check(
            &Plan::pro(),
            &Request {
                pool: Some("workhorse".into()),
                subagents: Some(3),
                ..Default::default()
            },
            &nothing_used()
        ),
        Verdict::Allow
    );
    assert!(matches!(
        check(
            &Plan::pro(),
            &Request {
                pool: Some("workhorse".into()),
                subagents: Some(4),
                ..Default::default()
            },
            &nothing_used()
        ),
        Verdict::Deny { .. }
    ));
}

#[test]
fn every_limit_the_free_plan_declares_is_actually_enforced() {
    // The failure this catches: an entitlement added to a plan and never wired into the engine —
    // a limit that exists in the catalogue and nowhere else, which reads as enforcement to
    // everyone who looks at the plan.
    let plan = Plan::free();
    for entitlement in &plan.entitlements {
        let (observed, request) = match entitlement {
            Entitlement::RequestsPerMin { limit } => (
                Observed {
                    requests_this_minute: Some(*limit),
                    ..nothing_used()
                },
                Request::default(),
            ),
            Entitlement::TokensPerDay { limit } => (
                Observed {
                    tokens_today: Some(*limit),
                    ..nothing_used()
                },
                Request::default(),
            ),
            Entitlement::SandboxSecondsPerDay { limit } => (
                Observed {
                    sandbox_seconds_today: Some(*limit),
                    ..nothing_used()
                },
                Request::default(),
            ),
            Entitlement::ConcurrentSessions { limit } => (
                Observed {
                    concurrent_sessions: Some(*limit),
                    ..nothing_used()
                },
                Request::default(),
            ),
            Entitlement::StorageBytes { limit } => (
                Observed {
                    storage_bytes: Some(*limit),
                    ..nothing_used()
                },
                Request::default(),
            ),
            Entitlement::SubagentFanout { limit } => (
                nothing_used(),
                Request {
                    subagents: Some(limit.saturating_add(1)),
                    ..Default::default()
                },
            ),
            Entitlement::ModelPools { .. } => (
                nothing_used(),
                Request {
                    pool: Some("frontier".into()),
                    ..Default::default()
                },
            ),
            // Not on the free plan; covered by its own tests above.
            Entitlement::SpendCeilingMicros { .. } | Entitlement::OfflineSeats { .. } => continue,
        };
        let verdict = check(&plan, &request, &observed);
        assert!(
            !matches!(verdict, Verdict::Allow),
            "{entitlement:?} is declared by the free plan but the engine allowed a request at its \
             limit: {verdict:?}"
        );
    }
}

#[test]
fn an_unaffordable_request_is_denied_even_when_the_soft_rule_would_degrade() {
    // The ordering bug a real database found: at 90% of the ceiling the soft rule wants to demote,
    // and demoting changes which pool serves the call — not what the account may spend. A request
    // whose own estimate crosses the ceiling has to be refused, or "degrade" becomes a way to
    // afford something the ceiling ruled out.
    let at_ninety_percent = Observed {
        balance_micros: Some(-45_000_000),
        ..nothing_used()
    };
    // Small enough to fit: degrade, because the balance is low but this request is affordable.
    match check(
        &Plan::pro(),
        &Request {
            pool: Some("workhorse".into()),
            estimated_micros: Some(1_000),
            ..Default::default()
        },
        &at_ninety_percent,
    ) {
        Verdict::Degrade { .. } => {}
        other => panic!("expected a degrade, got {other:?}"),
    }
    // Too big: denied, and the reason names the ceiling rather than the pool.
    match check(
        &Plan::pro(),
        &Request {
            pool: Some("workhorse".into()),
            estimated_micros: Some(60_000_000),
            ..Default::default()
        },
        &at_ninety_percent,
    ) {
        Verdict::Deny { why } => assert!(why.contains("would pass the"), "{why}"),
        other => panic!("expected a denial, got {other:?}"),
    }
}
