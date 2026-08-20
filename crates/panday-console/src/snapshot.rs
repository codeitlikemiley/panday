//! Content-free operator snapshot. No tokens, no prompts.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub listen: String,
    pub providers: Vec<String>,
    pub grok: Cred,
    pub claude: Cred,
    pub models: Vec<ModelRow>,
    pub pools: Vec<PoolRow>,
    pub recent: Vec<CallRow>,
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
