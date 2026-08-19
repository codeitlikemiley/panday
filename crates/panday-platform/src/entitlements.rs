//! The entitlement engine (M17.1, docs/17 §plans & entitlements).
//!
//! > "Entitlements are typed limits evaluated at the gateway/harness/sandbox edges."
//!
//! ## Two answers, not one
//!
//! A check returns [`Verdict`], which distinguishes *denied* from *degraded*. docs/17 is explicit
//! that a budget stop is "a graceful session pause, not a 500", and docs/12's `budget_soft`
//! constraint demotes to a cheaper pool rather than refusing — so an engine that only said yes or
//! no would force every caller to invent the middle case, and they would each invent it
//! differently.
//!
//! ## What it deliberately does not do
//!
//! It does not read the database and it does not know about time windows. A limit like
//! `TokensPerDay` needs *today's* usage, which is a ledger query, and mixing "what is allowed"
//! with "what has been spent" makes both untestable: the engine takes the observed usage as an
//! argument. That is why every test here is a pure function call with no fixtures beyond a plan.

use crate::Entitlement;
use panday_types::model::ModelRef;

/// What an account has actually used, in the window a limit is expressed in.
///
/// Assembled by the caller from the ledger (M11.4). `None` fields mean "not measured here" and are
/// treated as no evidence rather than as zero: an engine that read a missing number as zero would
/// happily allow a request on an account whose usage nobody looked up.
#[derive(Debug, Clone, Copy, Default)]
pub struct Observed {
    pub requests_this_minute: Option<u32>,
    pub tokens_today: Option<u64>,
    /// Negative balances are possible: the ledger is a sum, and a burst can overshoot.
    pub balance_micros: Option<i64>,
    pub sandbox_seconds_today: Option<u64>,
    pub concurrent_sessions: Option<u32>,
    pub storage_bytes: Option<u64>,
}

/// What a caller wants to do.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub pool: Option<String>,
    pub model: Option<ModelRef>,
    /// Sub-agents this turn intends to spawn.
    pub subagents: Option<u8>,
    /// Estimated spend, for a pre-flight check (docs/17: "enforced here pre-flight (estimate)
    /// and post-flight (reconcile actual)").
    pub estimated_micros: Option<u64>,
}

/// The engine's answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Allow,
    /// Allowed, but not as asked. docs/12's `budget_soft`: demote rather than refuse, because a
    /// user near a limit wants a slower answer rather than an error.
    Degrade {
        to_pool: String,
        why: String,
    },
    /// Refused. `why` reaches the user, so it names the limit rather than the code path.
    Deny {
        why: String,
    },
}

impl Verdict {
    pub fn allowed(&self) -> bool {
        !matches!(self, Verdict::Deny { .. })
    }
}

/// A plan: an id and its entitlements.
#[derive(Debug, Clone)]
pub struct Plan {
    pub plan_id: String,
    pub entitlements: Vec<Entitlement>,
}

impl Plan {
    /// docs/17's free tier: local-first, cheap-pool trickle, no frontier.
    pub fn free() -> Self {
        Self {
            plan_id: "free".into(),
            entitlements: vec![
                Entitlement::RequestsPerMin { limit: 20 },
                Entitlement::TokensPerDay { limit: 100_000 },
                Entitlement::ModelPools {
                    pools: vec!["local-only".into(), "cheap".into()],
                },
                Entitlement::SandboxSecondsPerDay { limit: 600 },
                Entitlement::ConcurrentSessions { limit: 1 },
                Entitlement::SubagentFanout { limit: 0 },
                Entitlement::StorageBytes {
                    limit: 100 * 1024 * 1024,
                },
            ],
        }
    }

    /// docs/17's Pro: workhorse pools, generous credits.
    ///
    /// The numbers are placeholders and docs/17 says so — "to be priced against measured COGS".
    /// They are written here rather than in a config file because a placeholder in code gets
    /// reviewed when it changes, and the ledger will supply the real ones within a week of
    /// dogfooding.
    pub fn pro() -> Self {
        Self {
            plan_id: "pro".into(),
            entitlements: vec![
                Entitlement::RequestsPerMin { limit: 300 },
                Entitlement::TokensPerDay { limit: 20_000_000 },
                Entitlement::SpendCeilingMicros { limit: 50_000_000 },
                Entitlement::ModelPools {
                    pools: vec!["local-only".into(), "cheap".into(), "workhorse".into()],
                },
                Entitlement::SandboxSecondsPerDay { limit: 14_400 },
                Entitlement::ConcurrentSessions { limit: 4 },
                Entitlement::SubagentFanout { limit: 3 },
                Entitlement::StorageBytes {
                    limit: 10 * 1024 * 1024 * 1024,
                },
            ],
        }
    }

    fn get<T>(&self, f: impl Fn(&Entitlement) -> Option<T>) -> Option<T> {
        self.entitlements.iter().find_map(f)
    }
}

/// The pool a soft-budget account is demoted to (docs/12 §constraints `budget_soft`).
pub const DEMOTE_TO: &str = "cheap";

/// Evaluate a request against a plan and what has been observed.
///
/// Order matters and is deliberate: **money first, then rate, then capability.** A user over their
/// spend ceiling should be told that, not told they used the wrong pool — the first limit reported
/// is the one they have to act on, and reporting the least important one wastes a support ticket.
pub fn check(plan: &Plan, request: &Request, observed: &Observed) -> Verdict {
    // 1. Hard spend ceiling.
    if let (Some(limit), Some(balance)) = (
        plan.get(|e| match e {
            Entitlement::SpendCeilingMicros { limit } => Some(*limit),
            _ => None,
        }),
        observed.balance_micros,
    ) {
        let spent = balance.unsigned_abs();
        if balance < 0 && spent >= limit {
            return Verdict::Deny {
                why: format!(
                    "spend ceiling reached: {spent} of {limit} credit-micros used. \
                     Add credit or raise the ceiling to continue."
                ),
            };
        }
        // Near the ceiling: docs/12's `budget_soft`. Degrade rather than refuse — 90% is a
        // threshold, and the point of naming it is that it is a decision rather than a feeling.
        if balance < 0 && spent * 10 >= limit * 9 {
            let asked = request.pool.clone().unwrap_or_default();
            if asked != DEMOTE_TO {
                return Verdict::Degrade {
                    to_pool: DEMOTE_TO.to_string(),
                    why: format!(
                        "{spent} of {limit} credit-micros used; routing to `{DEMOTE_TO}` \
                         until the balance recovers"
                    ),
                };
            }
        }
    }

    // 2. A pre-flight estimate that would cross the ceiling on its own.
    if let (Some(limit), Some(balance), Some(estimate)) = (
        plan.get(|e| match e {
            Entitlement::SpendCeilingMicros { limit } => Some(*limit),
            _ => None,
        }),
        observed.balance_micros,
        request.estimated_micros,
    ) {
        let projected = balance.unsigned_abs().saturating_add(estimate);
        if balance <= 0 && projected > limit {
            return Verdict::Deny {
                why: format!(
                    "this request is estimated at {estimate} credit-micros, which would pass the \
                     {limit} ceiling"
                ),
            };
        }
    }

    // 3. Rate.
    if let (Some(limit), Some(seen)) = (
        plan.get(|e| match e {
            Entitlement::RequestsPerMin { limit } => Some(*limit),
            _ => None,
        }),
        observed.requests_this_minute,
    ) {
        if seen >= limit {
            return Verdict::Deny {
                why: format!("rate limit: {seen} requests this minute, plan allows {limit}"),
            };
        }
    }

    if let (Some(limit), Some(seen)) = (
        plan.get(|e| match e {
            Entitlement::TokensPerDay { limit } => Some(*limit),
            _ => None,
        }),
        observed.tokens_today,
    ) {
        if seen >= limit {
            return Verdict::Deny {
                why: format!("daily token limit reached: {seen} of {limit}"),
            };
        }
    }

    if let (Some(limit), Some(seen)) = (
        plan.get(|e| match e {
            Entitlement::SandboxSecondsPerDay { limit } => Some(*limit),
            _ => None,
        }),
        observed.sandbox_seconds_today,
    ) {
        if seen >= limit {
            return Verdict::Deny {
                why: format!("daily sandbox limit reached: {seen}s of {limit}s"),
            };
        }
    }

    if let (Some(limit), Some(seen)) = (
        plan.get(|e| match e {
            Entitlement::ConcurrentSessions { limit } => Some(*limit),
            _ => None,
        }),
        observed.concurrent_sessions,
    ) {
        if seen >= limit {
            return Verdict::Deny {
                why: format!("{seen} sessions already running, plan allows {limit}"),
            };
        }
    }

    if let (Some(limit), Some(seen)) = (
        plan.get(|e| match e {
            Entitlement::StorageBytes { limit } => Some(*limit),
            _ => None,
        }),
        observed.storage_bytes,
    ) {
        if seen >= limit {
            return Verdict::Deny {
                why: format!("storage full: {seen} of {limit} bytes"),
            };
        }
    }

    // 4. Capability.
    if let (Some(pools), Some(asked)) = (
        plan.get(|e| match e {
            Entitlement::ModelPools { pools } => Some(pools.clone()),
            _ => None,
        }),
        request.pool.as_deref(),
    ) {
        if !pools.iter().any(|p| p == asked) {
            // Degrade rather than deny when the plan has *some* pool available: docs/12's whole
            // point is that a route is a choice among pools, and refusing a request because the
            // caller named the wrong one would be pedantry.
            let fallback = pools.iter().find(|p| *p != asked).cloned();
            return match fallback {
                Some(to_pool) => Verdict::Degrade {
                    why: format!("plan `{}` does not include `{asked}`", plan.plan_id),
                    to_pool,
                },
                None => Verdict::Deny {
                    why: format!("plan `{}` includes no model pools at all", plan.plan_id),
                },
            };
        }
    }

    if let (Some(limit), Some(asked)) = (
        plan.get(|e| match e {
            Entitlement::SubagentFanout { limit } => Some(*limit),
            _ => None,
        }),
        request.subagents,
    ) {
        if asked > limit {
            return Verdict::Deny {
                why: format!("plan allows {limit} sub-agents, {asked} requested"),
            };
        }
    }

    Verdict::Allow
}
