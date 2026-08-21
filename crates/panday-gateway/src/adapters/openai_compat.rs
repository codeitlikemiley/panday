//! `openai_compat` as a gateway adapter (docs/11).
//!
//! The dialect, SSE decoding and HTTP transport live in
//! `panday_sdk::providers::openai_compat` — docs/10: "the same stack runs
//! inside the gateway's adapters (write once, use both sides)". What is left
//! here is the part that is genuinely the gateway's: the declared
//! capabilities the router matches against, and the adapter's identity in the
//! sealed set.

use crate::{AdapterCaps, CacheStyle, ProviderAdapter};
use panday_sdk::providers::openai_compat::OpenAiCompatClient;
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::model::ChatRequest;

/// One adapter, many bases: Together, Fireworks, Groq, vLLM, llama-server,
/// mistral.rs. `local` is this adapter pinned to loopback with no auth.
pub struct OpenAiCompat {
    client: OpenAiCompatClient,
}

impl OpenAiCompat {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: OpenAiCompatClient::new(base_url, api_key),
        }
    }

    /// The `local` tier: loopback llama-server, no auth (docs/11).
    pub fn local(base_url: impl Into<String>) -> Self {
        Self::new(base_url, None)
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for OpenAiCompat {
    fn name(&self) -> &'static str {
        "openai_compat"
    }

    fn capabilities(&self, _model: &str) -> AdapterCaps {
        AdapterCaps {
            // Context length is a property of the loaded GGUF, not the
            // dialect, and this family exposes no reliable way to ask. The
            // router must not treat this as authoritative; a real catalog
            // arrives with the local model registry (docs/18).
            max_context: 0,
            tools: true,
            vision: false,
            // Automatic prefix caching where the server supports it; never
            // explicit breakpoints (ADR-007/008).
            cache_style: CacheStyle::AutomaticPrefix,
        }
    }

    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        self.client.chat(req).await
    }

    async fn list_models(&self) -> Result<Vec<panday_sdk::providers::RemoteModel>, PandayError> {
        self.client.list_models().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_automatic_prefix_caching_and_tool_support() {
        let adapter = OpenAiCompat::local("http://127.0.0.1:8080");
        let caps = adapter.capabilities("qwen3.5-4b");
        assert_eq!(adapter.name(), "openai_compat");
        assert!(caps.tools);
        // Never explicit breakpoints — those are Anthropic's (ADR-008).
        assert_eq!(caps.cache_style, CacheStyle::AutomaticPrefix);
    }
}
