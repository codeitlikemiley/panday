//! # panday-gateway (lib)
//!
//! Provider adapter contract (docs/11-gateway.md). Adapters are a sealed
//! set — deliberately NOT a plugin surface. First real adapter: M11.1
//! (openai_compat → llama-server).

pub mod adapters;
pub mod cache;
pub mod circuit;
pub mod console;
pub mod gateway;
pub mod ingress;
pub mod openapi;

pub use cache::{CacheKey, CachedResponse, ExactCache, MemoryExactCache, NoCache};
pub use circuit::{BreakerConfig, Breakers, State as CircuitState};
pub use gateway::{
    BudgetGate, CollectRoutes, CollectUsage, DiscardRoutes, DiscardUsage, Gateway, LiveModel,
    NoBudget, RouteAudit, RouteRecord, UsageRecord, UsageSink,
};
pub use ingress::{IngressRequest, IngressState};

use async_trait::async_trait;
use panday_sdk::providers::RemoteModel;
use panday_sdk::{ItemStream, PandayError};
use panday_types::model::ChatRequest;

/// Capabilities an adapter/model offers; the router matches `Caps` needs
/// against these.
#[derive(Debug, Clone, Default)]
pub struct AdapterCaps {
    pub max_context: u32,
    pub tools: bool,
    pub vision: bool,
    pub cache_style: CacheStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheStyle {
    #[default]
    None,
    /// Anthropic-style explicit breakpoints.
    Explicit,
    /// OpenAI-style automatic prefix caching.
    AutomaticPrefix,
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self, model: &str) -> AdapterCaps;
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError>;

    /// Models this credential can call. Default is empty so test spies stay
    /// spies; production adapters ask the provider.
    async fn list_models(&self) -> Result<Vec<RemoteModel>, PandayError> {
        Ok(Vec::new())
    }
}
