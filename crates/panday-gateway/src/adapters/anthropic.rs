//! `anthropic` as a gateway adapter (docs/11, M11.2).
//!
//! The dialect lives in `panday_sdk::providers::anthropic`; what is here is
//! the gateway's own concern — the capabilities the router matches against.

use crate::{AdapterCaps, CacheStyle, ProviderAdapter};
use panday_sdk::providers::anthropic::AnthropicClient;
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::model::ChatRequest;

pub struct Anthropic {
    client: AnthropicClient,
}

impl Anthropic {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: AnthropicClient::new(api_key),
        }
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: AnthropicClient::with_base_url(base_url, api_key),
        }
    }

    /// Claude Code / Claude Pro-Max subscription, not a console API key.
    pub fn oauth(access_token: impl Into<String>) -> Self {
        Self {
            client: AnthropicClient::oauth(access_token),
        }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn capabilities(&self, model: &str) -> AdapterCaps {
        AdapterCaps {
            // Every current Messages-API model is at least 200k. This is a
            // floor the router can rely on, not a catalog — per-model limits
            // arrive with the model registry (docs/18).
            max_context: 200_000,
            tools: true,
            // Opus/Sonnet accept images; Haiku-class text models are the
            // exception, so key off the name rather than claiming blanket
            // support the router would then mis-route on.
            vision: !model.contains("haiku"),
            // Explicit breakpoints — this is the ONLY adapter with them
            // (ADR-008), and the reason CacheStyle exists at all.
            cache_style: CacheStyle::Explicit,
        }
    }

    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        self.client.chat(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_explicit_cache_breakpoints() {
        let a = Anthropic::new("sk-test");
        assert_eq!(a.name(), "anthropic");
        let caps = a.capabilities("claude-sonnet-5");
        assert_eq!(
            caps.cache_style,
            CacheStyle::Explicit,
            "breakpoints are what distinguish this adapter (ADR-008)"
        );
        assert!(caps.tools);
        assert!(caps.vision);
        assert!(caps.max_context >= 200_000);
    }

    #[test]
    fn text_only_models_do_not_claim_vision() {
        assert!(!Anthropic::new("k").capabilities("claude-haiku-4-5").vision);
    }
}
