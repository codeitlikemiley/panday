//! Offline sync (docs/18 §Sync, M18.6).
//!
//! > "Offline sessions accumulate in the local event store. On reconnect (if the user links an
//! > account — optional, not required): logs push up (append-only merge is trivial — sessions are
//! > single-writer, ADR-002), local ledger entries reconcile into the account ledger (usage.model
//! > entries with `provider_cost_micros: 0` — they still count against fair-use metering for free
//! > tiers). Conflict-free by construction; no CRDT machinery needed."
//!
//! The three properties that make this safe to run from a laptop with a flaky connection:
//!
//! - **Idempotent.** Events are keyed `(session_id, seq)` and ledger entries carry an idempotency
//!   key derived from the session. Pushing the same log twice — reconnect, crash, reconnect — is a
//!   no-op, not a double bill.
//! - **Verified, not trusted.** The log arrives from a machine the customer controls. Its account,
//!   its gaplessness and its single-session-ness are checked here; the ledger effect is *recomputed*
//!   from the events by `rebuild::from_log` rather than taken from anything the client asserts.
//! - **Zero money, real usage.** Local inference costs nothing, so the entries are zero-amount and
//!   carry the token counts. A free tier that recorded nothing for offline work could not enforce
//!   fair use, and a tier that invented a cost would be billing for electricity it did not buy.

use crate::pg::{LedgerEntry, PgError};
use crate::rebuild;
use panday_types::event::{Envelope, Event};
use panday_types::pricing::CostModel;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Db(#[from] PgError),
    #[error("the log is empty")]
    Empty,
    #[error("the log holds {0} sessions; one push is one session (ADR-002: single writer)")]
    MixedSessions(usize),
    #[error("gap in the log: seq {expected} is missing (found {found})")]
    Gap { expected: u64, found: u64 },
    #[error("this log belongs to another account")]
    WrongAccount,
}

/// What a push did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub session_id: Uuid,
    /// Events this push stored.
    pub stored: u32,
    /// Events already present. Non-zero on a re-push, and that is a healthy number rather than a
    /// problem: it is what idempotency looks like from the outside.
    pub already_present: u32,
    /// Ledger entries written, which is 0 or 1 — one entry per session, not per turn.
    pub ledger_entries: u32,
    /// Events this build could not interpret.
    ///
    /// They are stored and re-served unchanged (docs/03 §unknown kinds) — a laptop on a newer
    /// version must not lose events by syncing to an older server — but they contribute nothing to
    /// the ledger, and that is worth saying out loud. A log that is *entirely* unknown syncs
    /// "successfully" while reconciling to zero, and the only way anyone notices is this number.
    pub unknown_events: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Push one offline session's log into an account.
pub async fn push(
    pool: &PgPool,
    account_id: Uuid,
    events: &[Envelope],
    prices: &dyn CostModel,
) -> Result<SyncReport, SyncError> {
    let session_id = check(events)?;

    let mut stored = 0u32;
    let mut already_present = 0u32;
    let mut unknown_events = 0u32;

    // One transaction for the events, so a connection dropped mid-push leaves the session either
    // fully stored or not stored at all — a partially-synced log would look like a gap to the next
    // push, and gaps are what this refuses.
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;
    for envelope in events {
        let done = sqlx::query(
            "INSERT INTO session_events (session_id, seq, account_id, at, kind, envelope)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (session_id, seq) DO NOTHING",
        )
        .bind(session_id)
        .bind(envelope.seq as i64)
        .bind(account_id)
        .bind(envelope.at)
        .bind({
            let kind = kind_of(&envelope.event);
            if kind == "unknown" {
                unknown_events += 1;
            }
            kind
        })
        .bind(serde_json::to_value(envelope).map_err(|e| PgError::Query(e.to_string()))?)
        .execute(&mut *tx)
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;

        if done.rows_affected() == 1 {
            stored += 1;
        } else {
            already_present += 1;
        }
    }
    tx.commit()
        .await
        .map_err(|e| PgError::Query(e.to_string()))?;

    // Recomputed from the events, never taken from the client. The laptop is the customer's
    // machine; what it says about its own bill is a claim, and the log is the evidence.
    let rebuilt = rebuild::from_log(events, prices);

    let entry = LedgerEntry {
        id: Uuid::new_v4(),
        account_id,
        kind: "usage.model".into(),
        // Local inference costs us nothing. Zero rather than absent, because "this session was
        // free" and "this session was not recorded" are different facts and fair-use metering
        // needs the first one (docs/18 §Sync).
        amount_micros: rebuilt.total_micros,
        quantity: serde_json::json!({
            "input_tokens": rebuilt.usage.input_tokens,
            "output_tokens": rebuilt.usage.output_tokens,
            "cache_read_tokens": rebuilt.usage.cache_read_tokens,
            "turns": rebuilt.turns,
            "unpriced_turns": rebuilt.unpriced_turns,
            "provider_cost_micros": 0,
            "offline": true,
        }),
        source: serde_json::json!({ "session_id": session_id, "sync": true }),
        // One key per session, so a re-push cannot write a second entry no matter how many times
        // the connection drops.
        idempotency_key: format!("sync:{session_id}"),
    };

    let ledger_entries = match crate::pg::append(pool, &entry).await {
        Ok(()) => 1,
        // Already reconciled. Not an error: the events were stored idempotently and so was this.
        Err(PgError::Duplicate(_)) => 0,
        Err(e) => return Err(e.into()),
    };

    Ok(SyncReport {
        session_id,
        stored,
        already_present,
        ledger_entries,
        unknown_events,
        input_tokens: rebuilt.usage.input_tokens,
        output_tokens: rebuilt.usage.output_tokens,
    })
}

/// One session, gapless, in order. The checks a server owes itself about a file that arrived from
/// somebody else's laptop.
fn check(events: &[Envelope]) -> Result<Uuid, SyncError> {
    let first = events.first().ok_or(SyncError::Empty)?;
    let session_id = first.session_id.0;

    let sessions: std::collections::BTreeSet<Uuid> =
        events.iter().map(|e| e.session_id.0).collect();
    if sessions.len() > 1 {
        return Err(SyncError::MixedSessions(sessions.len()));
    }

    for (expected, envelope) in (first.seq..).zip(events.iter()) {
        if envelope.seq != expected {
            return Err(SyncError::Gap {
                expected,
                found: envelope.seq,
            });
        }
    }
    Ok(session_id)
}

/// One session's events, in order — what a dashboard or a replay reads back.
pub async fn session(
    pool: &PgPool,
    account_id: Uuid,
    session_id: Uuid,
) -> Result<Vec<Envelope>, SyncError> {
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT envelope FROM session_events
         WHERE account_id = $1 AND session_id = $2 ORDER BY seq",
    )
    .bind(account_id)
    .bind(session_id)
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    Ok(rows
        .into_iter()
        .filter_map(|(value,)| serde_json::from_value(value).ok())
        .collect())
}

/// Sessions this account has synced, newest first.
pub async fn sessions(
    pool: &PgPool,
    account_id: Uuid,
    limit: i64,
) -> Result<Vec<(Uuid, i64)>, SyncError> {
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT session_id, count(*) FROM session_events
         WHERE account_id = $1 GROUP BY session_id ORDER BY max(synced_at) DESC LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;
    Ok(rows)
}

/// The event's kind as one word, for the denormalised column.
fn kind_of(event: &Event) -> &'static str {
    match event {
        Event::TurnStarted { .. } => "turn_started",
        Event::TurnFinished { .. } => "turn_finished",
        Event::UserMessage { .. } => "user_message",
        Event::AssistantDelta { .. } => "assistant_delta",
        Event::AssistantMessage { .. } => "assistant_message",
        Event::ToolCall { .. } => "tool_call",
        Event::ToolResult { .. } => "tool_result",
        Event::PermissionRequest { .. } => "permission_request",
        Event::PermissionDecision { .. } => "permission_decision",
        Event::Compaction { .. } => "compaction",
        Event::SubagentSpawned { .. } => "subagent_spawned",
        Event::SubagentFinished { .. } => "subagent_finished",
        Event::SessionForked { .. } => "session_forked",
        Event::Error { .. } => "error",
        // An event kind this build does not know is stored and re-served unchanged (docs/03
        // §unknown kinds): a laptop running a newer version must not lose events by syncing to an
        // older server.
        Event::Unknown { .. } => "unknown",
    }
}

/// The HTTP surface (M18.6).
///
/// One endpoint, JSONL in: exactly the file `panday local` already writes, posted verbatim. A JSON
/// array would mean the laptop had to parse and re-encode its own log to send it, which is one more
/// place for a log to change shape between the machine that produced it and the ledger that
/// believes it.
pub mod http {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::Arc;

    #[derive(Clone)]
    pub struct SyncState {
        pub pool: PgPool,
        pub prices: Arc<dyn CostModel>,
        /// Resolves a bearer token to an account. The same keys that authenticate inference
        /// (M17.3), because a customer should not need a second kind of credential to see their own
        /// sessions.
        pub auth: Arc<dyn panday_gateway::ingress::Authenticator>,
    }

    /// Big enough for a long offline session, small enough that a body is not a denial of service.
    /// A session that exceeds it should sync in parts — the push is idempotent and resumable by
    /// design, so splitting is free.
    pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

    pub fn router(state: SyncState) -> Router {
        Router::new()
            .route("/v1/sync/sessions", post(push_session))
            .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
            .with_state(state)
    }

    async fn push_session(
        State(state): State<SyncState>,
        headers: HeaderMap,
        body: String,
    ) -> Response {
        let bearer = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("")
            .trim();

        let Ok(caller) = state.auth.authenticate(bearer).await else {
            // One 401 for every failure, exactly as the model plane does (M17.3).
            return (StatusCode::UNAUTHORIZED, "invalid API key").into_response();
        };
        if !caller.allows("sessions") {
            return (
                StatusCode::UNAUTHORIZED,
                "this key does not carry the `sessions` scope",
            )
                .into_response();
        }

        let mut events = Vec::new();
        for (i, line) in body.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Envelope>(line) {
                Ok(envelope) => events.push(envelope),
                Err(e) => {
                    return (StatusCode::BAD_REQUEST, format!("line {}: {e}", i + 1))
                        .into_response()
                }
            }
        }

        match push(
            &state.pool,
            caller.account.0,
            &events,
            state.prices.as_ref(),
        )
        .await
        {
            Ok(report) => Json(serde_json::json!({
                "session_id": report.session_id,
                "stored": report.stored,
                "already_present": report.already_present,
                "ledger_entries": report.ledger_entries,
                "unknown_events": report.unknown_events,
                "input_tokens": report.input_tokens,
                "output_tokens": report.output_tokens,
            }))
            .into_response(),
            // A malformed log is the client's mistake; everything else is ours. Collapsing the two
            // would make a laptop retry forever against a log it can never fix.
            Err(e @ (SyncError::Empty | SyncError::Gap { .. } | SyncError::MixedSessions(_))) => {
                (StatusCode::BAD_REQUEST, e.to_string()).into_response()
            }
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }
}
