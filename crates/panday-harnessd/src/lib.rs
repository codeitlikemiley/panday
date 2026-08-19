//! panday-harnessd — the hosted session service (docs/03 M3.3, docs/22 shape 2).
//!
//! Serves the AEP event stream over a WebSocket, with the only sync mechanism
//! docs/03 permits:
//!
//! > "Append-only, gapless `seq`. Clients resume with `?after_seq=N`; the
//! > server replays. There is no other sync mechanism and none is needed."
//!
//! That single sentence is the whole design. A client that drops mid-turn
//! reconnects with the last `seq` it saw and receives exactly what it missed —
//! no diffing, no reconciliation, no "are we in sync" handshake.

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use panday_harness::{EventStore, MemoryStore};
use panday_types::event::Envelope;
use panday_types::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

/// One live session: its durable log plus a fan-out channel for connected
/// clients.
struct Session {
    store: Arc<MemoryStore>,
    /// Live events. Lagging receivers are dropped rather than allowed to
    /// stall the actor — a slow client must never back-pressure the log.
    live: broadcast::Sender<Envelope>,
}

pub mod testing;

/// What turns client input into events.
///
/// A trait rather than an embedded `SessionActor` because docs/01 is explicit that
/// "libraries take traits, binaries do the wiring": `panday-harnessd` serves the
/// protocol, and which model, tools and sandbox a session gets is a deployment
/// decision. It is also what lets the M10.3 client suite drive a real socket
/// against a scripted session with no provider.
///
/// Implementations receive the state so they can `publish` — every effect a driver
/// has on a session is an appended event, which is what keeps the log the whole
/// truth (ADR-002).
#[async_trait::async_trait]
pub trait SessionDriver: Send + Sync {
    async fn user_input(&self, state: &AppState, session: SessionId, text: String);
    async fn decision(
        &self,
        state: &AppState,
        session: SessionId,
        call_id: panday_types::CallId,
        decision: panday_types::event::PermDecision,
    );
    async fn cancel(&self, _state: &AppState, _session: SessionId) {}
}

#[derive(Clone, Default)]
pub struct AppState {
    sessions: Arc<std::sync::Mutex<HashMap<SessionId, Arc<Session>>>>,
    /// Absent means read-only: the socket still streams events, and client input is
    /// refused rather than silently dropped — a client whose input vanished would
    /// wait forever for events that were never going to come.
    driver: Option<Arc<dyn SessionDriver>>,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire the thing that runs turns (M10.3).
    pub fn with_driver(mut self, driver: Arc<dyn SessionDriver>) -> Self {
        self.driver = Some(driver);
        self
    }

    pub fn accepts_input(&self) -> bool {
        self.driver.is_some()
    }

    /// Create a session and return its handle.
    pub fn create(&self) -> SessionId {
        let id = SessionId::new();
        let (live, _) = broadcast::channel(1024);
        self.sessions.lock().unwrap().insert(
            id,
            Arc::new(Session {
                store: Arc::new(MemoryStore::new()),
                live,
            }),
        );
        id
    }

    fn get(&self, id: SessionId) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(&id).cloned()
    }

    /// Append an event and fan it out to connected clients.
    ///
    /// Durable first: a client must never see an event that is not in the log,
    /// or a resume would appear to *lose* it (docs/13 §persist-before-proceed).
    pub async fn publish(&self, id: SessionId, envelope: Envelope) -> Result<(), String> {
        let session = self.get(id).ok_or("no such session")?;
        session
            .store
            .append(envelope.clone())
            .await
            .map_err(|e| e.to_string())?;
        // Err just means nobody is listening.
        let _ = session.live.send(envelope);
        Ok(())
    }

    pub async fn events(&self, id: SessionId, after_seq: u64) -> Vec<Envelope> {
        match self.get(id) {
            Some(s) => s.store.read_after(id, after_seq).await.unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Highest `seq` in the log, or 0 for an empty session.
    pub async fn head_seq(&self, id: SessionId) -> u64 {
        self.events(id, 0).await.last().map_or(0, |e| e.seq)
    }
}

#[derive(Serialize)]
pub struct CreatedSession {
    pub session_id: String,
    pub ws: String,
}

#[derive(Deserialize, Default)]
pub struct ResumeQuery {
    /// Resume point. Absent means "from the beginning".
    #[serde(default)]
    pub after_seq: Option<u64>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/{id}/events", get(replay_events))
        .route("/v1/sessions/{id}/ws", get(ws_upgrade))
        .route("/metrics", get(metrics_endpoint))
        .with_state(state)
}

/// `GET /metrics` — Prometheus text exposition (docs/21 §Metrics, M21.2).
///
/// Unauthenticated and unconditional: a metrics endpoint that needs a key is a
/// metrics endpoint nobody scrapes. It exposes counts and decisions only — never
/// content, never an id — so the deployment can bind it wherever it likes
/// (docs/22 puts it behind the mesh).
async fn metrics_endpoint() -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            panday_sdk::metrics::CONTENT_TYPE,
        )],
        panday_sdk::metrics::render(),
    )
        .into_response()
}

async fn create_session(State(state): State<AppState>) -> Response {
    let id = state.create();
    (
        axum::http::StatusCode::CREATED,
        Json(CreatedSession {
            session_id: id.0.to_string(),
            ws: format!("/v1/sessions/{}/ws", id.0),
        }),
    )
        .into_response()
}

fn parse_id(raw: &str) -> Option<SessionId> {
    uuid::Uuid::parse_str(raw).ok().map(SessionId)
}

/// SSE-less replay, the read-only fallback docs/03 mentions.
async fn replay_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ResumeQuery>,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return (axum::http::StatusCode::BAD_REQUEST, "malformed session id").into_response();
    };
    Json(state.events(id, q.after_seq.unwrap_or(0)).await).into_response()
}

async fn ws_upgrade(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ResumeQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return (axum::http::StatusCode::BAD_REQUEST, "malformed session id").into_response();
    };
    if state.get(id).is_none() {
        return (axum::http::StatusCode::NOT_FOUND, "no such session").into_response();
    }

    let after = q.after_seq.unwrap_or(0);
    let head = state.head_seq(id).await;
    if after > head {
        // The client claims to have seen further than the log goes. Resume
        // cannot fix that — and accepting it silently is worse than refusing:
        // every future event would be filtered as "already sent" and the
        // client would sit there receiving nothing, forever, with no error.
        return (
            axum::http::StatusCode::CONFLICT,
            format!(
                "after_seq={after} is beyond this session's log (head={head}); \
                 the client's resume point does not exist"
            ),
        )
            .into_response();
    }

    upgrade.on_upgrade(move |socket| serve(socket, state, id, after))
}

/// Replay-then-tail.
///
/// The subscription is taken **before** the replay reads the log, so an event
/// appended in between is delivered live rather than falling into the gap
/// between the two. Duplicates are filtered by `seq`; a gap could not be
/// recovered at all.
async fn serve(mut socket: WebSocket, state: AppState, id: SessionId, after_seq: u64) {
    use futures_util::StreamExt;

    let Some(session) = state.get(id) else {
        return;
    };
    let mut live = session.live.subscribe();

    // Dedup against what was actually replayed, not what was requested — the
    // two differ, and using the request is what caused the silent hang above.
    let mut last_sent = 0u64;
    for envelope in state.events(id, after_seq).await {
        last_sent = envelope.seq;
        if send(&mut socket, &envelope).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            incoming = socket.next() => match incoming {
                // The client hung up: nothing to clean up, because the log is
                // the state. Reconnecting with `after_seq` is the whole
                // recovery story (docs/03).
                None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => return,
                Some(Ok(WsMessage::Text(text))) => {
                    if let Err(reason) = handle_client_message(&state, id, &text).await {
                        // Closed with a reason rather than ignored: a client whose
                        // input silently vanished would wait forever for events
                        // that were never going to come.
                        let _ = socket
                            .send(WsMessage::Close(Some(axum::extract::ws::CloseFrame {
                                code: 1003,
                                reason: reason.into(),
                            })))
                            .await;
                        return;
                    }
                }
                Some(Ok(_)) => {}
            },
            event = live.recv() => match event {
                Ok(envelope) => {
                    // Already replayed from the log, or older than the
                    // client's resume point.
                    if envelope.seq <= last_sent || envelope.seq <= after_seq {
                        continue;
                    }
                    last_sent = envelope.seq;
                    if send(&mut socket, &envelope).await.is_err() {
                        return;
                    }
                }
                // A client too slow to keep up is disconnected rather than
                // silently skipped: it can reconnect with `after_seq` and get
                // every missed event from the log. Skipping would hand it a
                // gap it can never detect.
                Err(broadcast::error::RecvError::Lagged(_)) => return,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// Dispatch one client frame. `Err(reason)` closes the socket.
///
/// Input is driven, not queued: the driver appends events, and the same socket
/// delivers them — so a client sees the consequence of its own message in order,
/// and a disconnect loses nothing that the log did not already have.
async fn handle_client_message(state: &AppState, id: SessionId, text: &str) -> Result<(), String> {
    let message: panday_sdk::sessions::ClientMessage =
        serde_json::from_str(text).map_err(|e| format!("not a client message: {e}"))?;

    let Some(driver) = state.driver.clone() else {
        return Err("this server does not accept session input".into());
    };

    match message {
        panday_sdk::sessions::ClientMessage::UserInput { text } => {
            driver.user_input(state, id, text).await;
        }
        panday_sdk::sessions::ClientMessage::Decide { call_id, decision } => {
            driver.decision(state, id, call_id, decision).await;
        }
        panday_sdk::sessions::ClientMessage::Cancel => {
            driver.cancel(state, id).await;
        }
    }
    Ok(())
}

async fn send(socket: &mut WebSocket, envelope: &Envelope) -> Result<(), ()> {
    let text = serde_json::to_string(envelope).map_err(|_| ())?;
    socket
        .send(WsMessage::Text(text.into()))
        .await
        .map_err(|_| ())
}
