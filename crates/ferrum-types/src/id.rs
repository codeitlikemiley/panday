//! Identifiers. UUIDv7 everywhere (time-ordered, PG-index-friendly).
//! Human-facing short forms are display encodings, not second ids.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
            /// Display prefix for human-facing short forms (e.g. logs, URLs).
            pub const PREFIX: &'static str = $prefix;
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}_{}", $prefix, self.0.simple())
            }
        }
    };
}

id_type!(
    /// One conversation/agent session; owns an append-only event log.
    SessionId,
    "sess"
);
id_type!(
    /// One model-driven turn within a session.
    TurnId,
    "turn"
);
id_type!(
    /// One tool invocation.
    CallId,
    "call"
);
id_type!(
    /// One gateway request (idempotency + ledger source).
    RequestId,
    "req"
);
id_type!(
    /// A billing/tenancy account.
    AccountId,
    "acct"
);

/// Content-addressed handle into object storage. Events carry these instead
/// of large payloads (docs/03, docs/15).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// sha256 of content, hex.
    pub hash: String,
    /// Size in bytes of the raw artifact.
    pub size: u64,
    /// MIME type where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}
