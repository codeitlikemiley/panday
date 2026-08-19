//! # panday-types
//!
//! The platform's shared vocabulary. See `docs/03-protocol.md` for the
//! protocol this crate is the source of truth for, and `docs/02-workspace.md`
//! for the dependency rules (near-zero deps; breaking this crate is a
//! platform-wide event).

pub mod capability;
pub mod event;
pub mod id;
pub mod model;
pub mod pricing;
pub mod scorecard;

pub use capability::{CapabilityProfile, Provenance};
pub use event::{Envelope, Event, PROTOCOL_VERSION};
pub use id::{AccountId, ArtifactRef, CallId, RequestId, SessionId, TurnId};
pub use model::{
    CacheHints, CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason,
    StreamItem, TaskClass, ToolDef, Usage,
};

/// Timestamps are UTC, RFC 3339 on the wire.
pub type Timestamp = time::OffsetDateTime;

/// Loosely-typed JSON payloads (tool args, provider extras).
pub type Json = serde_json::Value;
