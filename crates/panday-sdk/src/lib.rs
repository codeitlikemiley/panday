//! # panday-sdk
//!
//! The typed face of the platform (docs/10-sdk.md).
//!
//! Layout follows docs/02 ("client library: providers, middleware, agent
//! builder"):
//!
//! - [`providers`] — provider dialects and the HTTP seam (the wire layer the
//!   gateway's adapters share, docs/10: "write once, use both sides");
//! - [`middleware`] — composable retry/timeout over any [`ModelClient`];
//! - [`gateway`] — the transport to a `panday-gateway` instance (M10.2).
//!
//! Sessions (M10.3) and the embedded agent (M10.5) build on these.

pub mod gateway;
pub mod metrics;
pub mod middleware;
pub mod oauth;
pub mod providers;
pub mod sessions;
pub mod telemetry;

pub use gateway::{connect, GatewayTransport};
pub use middleware::{ModelClientExt, Retry, RetryPolicy, Timeout};
/// `#[panday_sdk::tool]` (M10.4). Re-exported here because docs/10 puts the macro in
/// the SDK's surface — it is the embedded-agent story's front door — while the code it
/// generates targets `panday-harness`, which the SDK does not depend on. A crate using
/// the macro therefore depends on both, which is what the embedded agent does anyway
/// (docs/10 Layer 4: "This embeds `panday-harness`").
pub use panday_macros::tool;
pub use sessions::{After, ClientMessage, SessionStream, SessionsClient};

use async_trait::async_trait;
use panday_types::model::{ChatRequest, StreamItem};

/// Anything that can serve a chat request as a stream of items: the gateway
/// transport in production, a scripted fake in harness tests (docs/13 M13.1),
/// the local daemon offline.
#[async_trait]
pub trait ModelClient: Send + Sync {
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError>;
}

/// Boxed stream of items. (Kept runtime-agnostic: it's just an async
/// iterator; tokio arrives with the transports, not the traits.)
pub type ItemStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<StreamItem, PandayError>> + Send>>;

/// Stable error vocabulary mirroring the wire codes (docs/10 §Errors).
#[derive(Debug, thiserror::Error)]
pub enum PandayError {
    #[error("rate limited; retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64 },
    #[error("budget exceeded; balance {balance_micros} credit-micros")]
    BudgetExceeded { balance_micros: i64 },
    #[error("entitlement denied: plan {plan} lacks {needed}")]
    EntitlementDenied { plan: String, needed: String },
    #[error("no model available; tried {tried:?}")]
    ModelUnavailable { tried: Vec<String> },
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("provider error ({upstream}): {message} (retryable: {retryable})")]
    Provider {
        upstream: String,
        message: String,
        retryable: bool,
    },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl PandayError {
    /// Retryability is a method, not a guess.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            PandayError::RateLimited { .. }
                | PandayError::Provider {
                    retryable: true,
                    ..
                }
        )
    }
}
