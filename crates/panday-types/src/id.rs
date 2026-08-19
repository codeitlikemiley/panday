//! Identifiers. UUIDv7 everywhere (time-ordered, PG-index-friendly).
//! Human-facing short forms are display encodings, not second ids.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        // On the wire these are plain UUID strings; describing them as such
        // avoids depending on schemars' uuid integration for no benefit.
        #[cfg_attr(
            feature = "schema",
            derive(schemars::JsonSchema),
            schemars(with = "String", description = "UUIDv7, canonical hyphenated form")
        )]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArtifactRef {
    /// sha256 of content, hex.
    pub hash: String,
    /// Size in bytes of the raw artifact.
    pub size: u64,
    /// MIME type where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

/// Lowercase hex, the encoding docs/03 §Identifiers specifies for every digest on the wire.
///
/// One function because there were six copies of it, and because `sha2` 0.11 removed the
/// `LowerHex` impl that had been holding them together — `format!("{:x}", hasher.finalize())`
/// stopped compiling everywhere at once. A digest that is rendered differently in two places is a
/// content address that does not match itself, which is the class of bug that shows up as a cache
/// that never hits and an artifact that cannot be found.
pub fn hex(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write;
    bytes.as_ref().iter().fold(String::new(), |mut out, byte| {
        // `write!` to a String cannot fail; the result is discarded rather than unwrapped so this
        // stays allocation-free per byte.
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod hex_tests {
    use super::hex;

    #[test]
    fn every_byte_is_two_lowercase_characters() {
        // The failure this pins down: a `{:x}` formatter drops leading zeros per byte, so a digest
        // with a zero byte in it would render one character short and stop matching itself.
        assert_eq!(hex([0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex([0xab, 0xcd]), "abcd");
        assert_eq!(hex([]), "");
        assert_eq!(hex(vec![1u8; 32]).len(), 64);
    }
}
