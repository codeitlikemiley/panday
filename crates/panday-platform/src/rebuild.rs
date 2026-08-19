//! Rebuilding the ledger from the event log (M3.5, docs/03 §Milestones, docs/17 §the ledger).
//!
//! > "Ledger-rebuild-from-log: property test that replaying any session yields the ledger totals
//! > the live path recorded."
//!
//! docs/17 calls the ledger **provable**: "`source` points into the event log; a dispute is settled
//! by replaying the session and recomputing". This module is the recompute. If it ever disagrees
//! with the live path, one of them is wrong and the log is the one that is not — which is the whole
//! content of ADR-002.
//!
//! ## The two rules that make the fold correct
//!
//! 1. **Usage comes from `AssistantMessage` only.** `TurnFinished.usage` is a redundant turn
//!    summary (docs/03), so counting both double-bills every turn. `panday replay --costs` folds
//!    the same way for the same reason, and the two now agree by construction rather than by
//!    coincidence.
//! 2. **The model comes from `TurnStarted`.** A session can change model mid-session (failover, a
//!    router decision, a user switching), and pricing every turn at the session's *first* model is
//!    how a rebuild silently disagrees with the live path on exactly the sessions that failed over.

use panday_types::event::{Envelope, Event};
use panday_types::model::{ModelRef, Usage};
use panday_types::pricing::CostModel;

/// What a rebuild found.
#[derive(Debug, Clone, PartialEq)]
pub struct Rebuilt {
    /// Total in micro-credits, negative for consumption — the same sign convention as the ledger,
    /// so a comparison needs no translation and cannot be got backwards.
    pub total_micros: i64,
    /// Turns priced.
    pub turns: u32,
    /// Turns whose model had no configured price. Reported rather than skipped silently: a rebuild
    /// that quietly ignored an unpriced model would "agree" with a live path that had also ignored
    /// it, and neither would be right.
    pub unpriced_turns: u32,
    pub usage: Usage,
}

/// Recompute a session's cost from its log alone.
pub fn from_log(events: &[Envelope], prices: &dyn CostModel) -> Rebuilt {
    let mut out = Rebuilt {
        total_micros: 0,
        turns: 0,
        unpriced_turns: 0,
        usage: Usage::default(),
    };
    // The model in force. `None` until a `TurnStarted` says — an `AssistantMessage` before one is a
    // malformed log, and pricing it against a guess would be inventing evidence.
    let mut model: Option<ModelRef> = None;

    for envelope in events {
        match &envelope.event {
            Event::TurnStarted { model: m, .. } => {
                model = Some(m.clone());
                out.turns += 1;
            }
            Event::AssistantMessage { usage, .. } => {
                out.usage.add(*usage);
                match model.as_ref().and_then(|m| prices.cost_micros(m, *usage)) {
                    Some(cost) => out.total_micros -= cost as i64,
                    None => out.unpriced_turns += 1,
                }
            }
            _ => {}
        }
    }
    out
}

/// Compare a rebuild with what the live path recorded.
///
/// Returns the discrepancy in micro-credits: zero means the ledger is provable for this session.
/// Signed, because *which* side is short matters — over-billing and under-billing are different
/// incidents with different responses.
pub fn discrepancy(rebuilt: &Rebuilt, ledger_total_micros: i64) -> i64 {
    ledger_total_micros - rebuilt.total_micros
}
