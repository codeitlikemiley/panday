//! Content-free operator snapshot. No tokens, no prompts.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub listen: String,
    pub providers: Vec<String>,
    pub grok: Cred,
    pub claude: Cred,
    /// Outbound credentials currently loaded (last4 only, never the secret).
    pub accounts: Vec<AccountRow>,
    /// `failover` or `round_robin`.
    pub rotate: String,
    pub models: Vec<ModelRow>,
    pub pools: Vec<PoolRow>,
    pub recent: Vec<CallRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountRow {
    pub id: String,
    pub provider: String,
    pub kind: String,
    pub label: String,
    pub last4: String,
    /// Calls made against this credential in the current window (docs/25 M25.6).
    pub used: u64,
    /// Operator-declared calls per window. `None` means nobody has declared one
    /// — which is why `remaining_pct` is also `None` rather than 1.0. Rendering
    /// "100% left" for a credential no one has measured would be a lie.
    pub ceiling: Option<u64>,
    pub window_secs: Option<u64>,
    pub remaining_pct: Option<f64>,
    /// A 429 arrived with nothing left in the window. Clears when it rolls.
    pub exhausted: bool,
    /// The provider's *own* short-window headroom from its response headers
    /// (docs/25 M25.7). A different number from `remaining_pct`, which is the
    /// operator's declared grant over a longer period — so both are shown
    /// rather than merged. `None` for OAuth subscriptions, which publish no
    /// such header and which we do not scrape.
    pub headroom_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cred {
    pub present: bool,
    pub fresh: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelRow {
    pub id: String,
    pub context: u32,
    pub provenance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PoolRow {
    pub name: String,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallRow {
    pub model: String,
    pub provider: String,
    pub pool: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}
