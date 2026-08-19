//! # panday-platform (lib)
//!
//! The commercial spine: entitlements + the append-only ledger
//! (docs/17-platform.md, ADR-009). Stripe is a projection; this is truth.

pub mod entitlements;
pub mod keys;
pub mod ledger;
pub mod pg;
pub mod rebuild;
pub mod registry;
pub mod tenancy;

use panday_types::{AccountId, Json, Timestamp};
use serde::{Deserialize, Serialize};

/// Typed limits evaluated at the gateway/harness/sandbox edges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entitlement {
    RequestsPerMin { limit: u32 },
    TokensPerDay { limit: u64 },
    SpendCeilingMicros { limit: u64 },
    ModelPools { pools: Vec<String> },
    SandboxSecondsPerDay { limit: u64 },
    ConcurrentSessions { limit: u32 },
    SubagentFanout { limit: u8 },
    StorageBytes { limit: u64 },
    OfflineSeats { limit: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    UsageModel,
    UsageSandbox,
    UsageStorage,
    GrantPlan,
    GrantPurchase,
    AdjustRefund,
}

/// Append-only. `amount_micros` negative = consumption, in credit-micros.
/// `source` points back into the event log so any entry is replayable
/// (docs/17 §ledger; property test M17.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub id: uuid::Uuid,
    pub account: AccountId,
    #[serde(with = "time::serde::rfc3339")]
    pub at: Timestamp,
    pub kind: LedgerKind,
    pub amount_micros: i64,
    pub quantity: Json,
    pub source: Json,
    /// Request-scoped: retries cannot double-bill.
    pub idempotency_key: String,
}

/// Price table entry: how usage converts to credit-micros. Versioned in-repo;
/// cache splits priced at their real multipliers so reducer savings reach COGS.
///
/// Multipliers reflect provider reality: reads ~0.1x everywhere; write
/// surcharges exist only on explicit-breakpoint providers (Anthropic:
/// ~1.25x for 5m TTL, ~2x for 1h TTL). For providers with automatic caching
/// and no write premium (OpenAI-style), set both write multipliers to 1.0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPrice {
    pub model: String,
    pub input_per_mtok_micros: u64,
    pub output_per_mtok_micros: u64,
    pub cache_read_multiplier: f32,     // ~0.1
    pub cache_write_multiplier: f32, // 5m-TTL writes: ~1.25 (Anthropic), 1.0 (no-premium providers)
    pub cache_write_1h_multiplier: f32, // 1h-TTL writes: ~2.0 (Anthropic)
    pub margin_multiplier: f32,
}

impl ModelPrice {
    /// Usage convention (see `panday_types::model::Usage`): cache counts are
    /// SUBSETS of `input_tokens`; the fresh remainder prices at 1x.
    pub fn cost_micros(&self, u: &panday_types::model::Usage) -> u64 {
        let cached = u.cache_read_tokens + u.cache_write_tokens + u.cache_write_1h_tokens;
        debug_assert!(
            cached <= u.input_tokens,
            "Usage cache counts must be subsets of input_tokens (adapter normalization bug)"
        );
        let fresh = u.input_tokens.saturating_sub(cached);
        let per_in = self.input_per_mtok_micros as f64 / 1e6;
        let inp = fresh as f64 * per_in
            + u.cache_read_tokens as f64 * per_in * self.cache_read_multiplier as f64
            + u.cache_write_tokens as f64 * per_in * self.cache_write_multiplier as f64
            + u.cache_write_1h_tokens as f64 * per_in * self.cache_write_1h_multiplier as f64;
        let out = u.output_tokens as f64 * self.output_per_mtok_micros as f64 / 1e6;
        ((inp + out) * self.margin_multiplier as f64).round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use panday_types::model::Usage;

    #[test]
    fn cache_reads_are_cheap_cache_writes_cost_extra() {
        let p = ModelPrice {
            model: "x".into(),
            input_per_mtok_micros: 3_000_000, // 3 credits / Mtok
            output_per_mtok_micros: 15_000_000,
            cache_read_multiplier: 0.1,
            cache_write_multiplier: 1.25,
            cache_write_1h_multiplier: 2.0,
            margin_multiplier: 1.0,
        };
        let fresh = p.cost_micros(&Usage {
            input_tokens: 1_000_000,
            ..Default::default()
        });
        let cached = p.cost_micros(&Usage {
            input_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
            ..Default::default()
        });
        assert_eq!(fresh, 3_000_000);
        assert_eq!(cached, 300_000, "cache reads must price at ~0.1x");
        let written = p.cost_micros(&Usage {
            input_tokens: 1_000_000,
            cache_write_tokens: 1_000_000,
            ..Default::default()
        });
        assert_eq!(written, 3_750_000, "5m cache writes at 1.25x");
        let written_1h = p.cost_micros(&Usage {
            input_tokens: 1_000_000,
            cache_write_1h_tokens: 1_000_000,
            ..Default::default()
        });
        assert_eq!(
            written_1h, 6_000_000,
            "1h cache writes at 2x — a single multiplier cannot price both tiers"
        );
    }
}
