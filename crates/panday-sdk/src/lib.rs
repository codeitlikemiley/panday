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
pub mod vault;

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
    // 0 means no upstream stated a wait (docs/25 M25.3), so saying "retry after
    // 0ms" would put the exact hammer instruction on the wire that the whole
    // Retry-After path exists to prevent.
    #[error("rate limited{}", retry_after_suffix(*retry_after_ms))]
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

/// The tail of a `RateLimited` message. Silence beats a fabricated zero.
fn retry_after_suffix(retry_after_ms: u64) -> String {
    if retry_after_ms == 0 {
        "; no retry delay stated upstream".to_string()
    } else {
        format!("; retry after {retry_after_ms}ms")
    }
}

impl PandayError {
    /// The wait a client should honour, in whole seconds, or `None` when no
    /// upstream stated one.
    ///
    /// `Retry-After` is delay-**seconds** (RFC 9110 §10.2.3) while we carry
    /// milliseconds, and the rounding direction is not a detail: too late only
    /// costs latency, too early costs another 429 — and several providers
    /// extend the penalty window when you retry inside it. So this rounds up,
    /// and a sub-second wait becomes 1 rather than 0.
    ///
    /// Deliberately unclamped. `MAX_HONOURED_RETRY_AFTER` bounds how long *we*
    /// will sleep; it says nothing about the upstream's window, and republishing
    /// a shortened number would schedule a retry storm at the moment it expires.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            PandayError::RateLimited { retry_after_ms } if *retry_after_ms > 0 => {
                Some(retry_after_ms.div_ceil(1000))
            }
            _ => None,
        }
    }

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
