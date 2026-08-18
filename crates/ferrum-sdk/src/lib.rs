//! # ferrum-sdk
//!
//! The typed face of the platform (docs/10-sdk.md). This seed defines the
//! client trait and the error vocabulary; transports (M10.2), sessions
//! (M10.3) and the embedded agent (M10.5) build on these.

use async_trait::async_trait;
use ferrum_types::model::{ChatRequest, StreamItem};

/// Anything that can serve a chat request as a stream of items: the gateway
/// transport in production, a scripted fake in harness tests (docs/13 M13.1),
/// the local daemon offline.
#[async_trait]
pub trait ModelClient: Send + Sync {
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, FerrumError>;
}

/// Boxed stream of items. (Kept runtime-agnostic: it's just an async
/// iterator; tokio arrives with the transports, not the traits.)
pub type ItemStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<StreamItem, FerrumError>> + Send>>;

/// Stable error vocabulary mirroring the wire codes (docs/10 §Errors).
#[derive(Debug, thiserror::Error)]
pub enum FerrumError {
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

impl FerrumError {
    /// Retryability is a method, not a guess.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            FerrumError::RateLimited { .. }
                | FerrumError::Provider {
                    retryable: true,
                    ..
                }
        )
    }
}
