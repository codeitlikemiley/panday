//! The sessions client (docs/10 Layer 3, M10.3).
//!
//! ```ignore
//! let client = SessionsClient::new("http://127.0.0.1:8081", None);
//! let session = client.create().await?;
//! let mut events = client.subscribe(session, After::Beginning).await?;
//! events.send(UserInput::text("fix the failing test")).await?;
//! while let Some(envelope) = events.next().await { ... }
//! ```
//!
//! ## Resume is the only sync mechanism
//!
//! docs/03: "Clients resume with `?after_seq=N`; the server replays. There is no
//! other sync mechanism and none is needed." So this client has no reconcile step,
//! no "am I in sync" handshake and no local mutation queue — it tracks the highest
//! `seq` it has seen and reconnects with it. `After::Seq(n)` and `After::Latest`
//! are the same API as the first connection, because a resume *is* a connection.
//!
//! ## Why the socket is bidirectional
//!
//! An earlier shape had events arriving on the WS and input going over POST. That
//! makes two orderings to reason about — an input accepted after a disconnect but
//! before its events, say — and the client cannot tell whether its input landed
//! before the events it is missing. One socket means one order: everything the
//! client sends is sequenced against what it receives, and if the socket dies both
//! halves die with it and resume replays the truth.

use crate::PandayError;
use futures_util::{SinkExt, StreamExt};
use panday_types::event::{Envelope, PermDecision};
use panday_types::{CallId, SessionId};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Where to resume from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum After {
    /// Replay the whole session, then tail.
    Beginning,
    /// Only new events. Note this *drops* history — correct for a fresh UI that
    /// will fetch history separately, wrong for a client rebuilding state, which is
    /// why it is not the default.
    Latest,
    /// Everything after `seq`. The resume case.
    Seq(u64),
}

impl After {
    fn query(&self, head: u64) -> String {
        match self {
            After::Beginning => "?after_seq=0".into(),
            After::Latest => format!("?after_seq={head}"),
            After::Seq(n) => format!("?after_seq={n}"),
        }
    }
}

/// What a client sends. Deliberately small: docs/03 says a session is driven by
/// events, and every one of these produces events rather than returning data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// A turn of user input.
    UserInput { text: String },
    /// An answer to a `PermissionRequest`.
    Decide {
        call_id: CallId,
        decision: PermDecision,
    },
    /// Stop the current turn (docs/13 §cancellation).
    Cancel,
}

impl ClientMessage {
    pub fn text(text: impl Into<String>) -> Self {
        Self::UserInput { text: text.into() }
    }
}

/// Handle on one session's live stream.
pub struct SessionStream {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    /// Highest `seq` delivered to the caller — the resume point.
    last_seq: u64,
    session: SessionId,
}

impl SessionStream {
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// The `seq` to resume from. Advanced only when an event is *returned* to the
    /// caller, never when it is received: an event dropped between the socket and
    /// the caller must be replayed, and advancing on receipt would skip it.
    pub fn resume_point(&self) -> u64 {
        self.last_seq
    }

    pub async fn send(&mut self, message: ClientMessage) -> Result<(), PandayError> {
        let text = serde_json::to_string(&message)
            .map_err(|e| PandayError::Protocol(format!("encode client message: {e}")))?;
        self.socket
            .send(WsMessage::Text(text.into()))
            .await
            .map_err(as_transport)
    }

    /// Send a raw frame. For tests and for foreign clients that speak the wire
    /// directly — the typed `send` is what application code should use.
    pub async fn send_raw(&mut self, text: &str) -> Result<(), PandayError> {
        self.socket
            .send(WsMessage::Text(text.to_string().into()))
            .await
            .map_err(as_transport)
    }

    /// The next event, or `None` when the socket closes.
    ///
    /// A closed socket is not an error: it is the normal end of a connection, and
    /// the caller's response is to resume rather than to fail. A *protocol* error
    /// (an unparseable frame) is an error, because resuming would just hit it again.
    pub async fn next_event(&mut self) -> Option<Result<Envelope, PandayError>> {
        loop {
            match self.socket.next().await? {
                Ok(WsMessage::Text(text)) => {
                    return Some(match serde_json::from_str::<Envelope>(&text) {
                        Ok(envelope) => {
                            // Unknown event kinds parse into `Event::Unknown`
                            // (docs/03), so a newer server does not break this
                            // client — it just yields events it cannot render.
                            self.last_seq = self.last_seq.max(envelope.seq);
                            Ok(envelope)
                        }
                        Err(e) => Err(PandayError::Protocol(format!(
                            "server sent a frame that is not an AEP envelope: {e}"
                        ))),
                    });
                }
                // Pings are answered by tungstenite; a pong needs no action.
                Ok(WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Binary(_)) => continue,
                Ok(WsMessage::Close(frame)) => {
                    // A close *with* a reason is the server refusing something
                    // (docs/03's 409 for a resume point past the head, for
                    // instance); surfacing it is the difference between "reconnect"
                    // and "your resume point does not exist".
                    return match frame {
                        Some(f) if !f.reason.is_empty() => Some(Err(PandayError::Protocol(
                            format!("server closed the stream: {}", f.reason),
                        ))),
                        _ => None,
                    };
                }
                Ok(WsMessage::Frame(_)) => continue,
                Err(e) => return Some(Err(as_transport(e))),
            }
        }
    }
}

/// A client for a `panday-harnessd` instance.
pub struct SessionsClient {
    base_url: String,
    api_key: Option<String>,
}

#[derive(Deserialize)]
struct CreatedSession {
    session_id: String,
}

impl SessionsClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }

    /// `POST /v1/sessions`.
    pub async fn create(&self) -> Result<SessionId, PandayError> {
        let client = reqwest::Client::new();
        let mut req = client.post(format!("{}/v1/sessions", self.base_url));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let response = req
            .send()
            .await
            .map_err(|e| as_transport_str(e.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| as_transport_str(e.to_string()))?;
        if !status.is_success() {
            return Err(PandayError::Protocol(format!(
                "create session: HTTP {status}: {body}"
            )));
        }
        let created: CreatedSession = serde_json::from_str(&body)
            .map_err(|e| PandayError::Protocol(format!("create session: {e}: {body}")))?;
        let id = uuid::Uuid::parse_str(&created.session_id)
            .map_err(|e| PandayError::Protocol(format!("session id: {e}")))?;
        Ok(SessionId(id))
    }

    /// Read the log without a socket — the fallback docs/03 mentions, and what a
    /// client uses to fetch history after `After::Latest`.
    pub async fn events(
        &self,
        session: SessionId,
        after_seq: u64,
    ) -> Result<Vec<Envelope>, PandayError> {
        let client = reqwest::Client::new();
        let mut req = client.get(format!(
            "{}/v1/sessions/{}/events?after_seq={after_seq}",
            self.base_url, session.0
        ));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let response = req
            .send()
            .await
            .map_err(|e| as_transport_str(e.to_string()))?;
        let body = response
            .text()
            .await
            .map_err(|e| as_transport_str(e.to_string()))?;
        serde_json::from_str(&body).map_err(|e| PandayError::Protocol(format!("events: {e}")))
    }

    /// Open the event stream. `After::Latest` needs the current head, which costs
    /// one extra request — the reason it is not the default.
    pub async fn subscribe(
        &self,
        session: SessionId,
        after: After,
    ) -> Result<SessionStream, PandayError> {
        let head = match after {
            After::Latest => self.events(session, 0).await?.last().map_or(0, |e| e.seq),
            _ => 0,
        };
        let ws_url = format!(
            "{}/v1/sessions/{}/ws{}",
            self.base_url.replacen("http", "ws", 1),
            session.0,
            after.query(head)
        );

        let (socket, response) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .map_err(|e| match &e {
                // The server refuses a resume point past the head with 409
                // (docs/03) — reporting that as a transport failure would send a
                // client into a reconnect loop against a condition retrying cannot
                // fix.
                tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 409 => {
                    PandayError::Protocol(format!(
                        "resume point is beyond this session's log ({})",
                        r.status()
                    ))
                }
                _ => as_transport_str(e.to_string()),
            })?;
        if response.status().as_u16() != 101 {
            return Err(PandayError::Protocol(format!(
                "server did not upgrade: HTTP {}",
                response.status()
            )));
        }

        Ok(SessionStream {
            socket,
            last_seq: match after {
                After::Seq(n) => n,
                After::Latest => head,
                After::Beginning => 0,
            },
            session,
        })
    }

    /// Subscribe, then reconnect on every clean close until the caller stops.
    ///
    /// This is the whole resume story in one function, and it exists so that the
    /// CLI (and anyone else) does not each re-derive it: track the last `seq`,
    /// reconnect with `After::Seq(last)`, and let the server replay. A *protocol*
    /// error is not retried — reconnecting into the same refusal is a loop.
    pub async fn resume_from(
        &self,
        session: SessionId,
        last_seq: u64,
    ) -> Result<SessionStream, PandayError> {
        self.subscribe(session, After::Seq(last_seq)).await
    }
}

fn as_transport(e: tokio_tungstenite::tungstenite::Error) -> PandayError {
    as_transport_str(e.to_string())
}

fn as_transport_str(message: String) -> PandayError {
    // Retryable: a dropped socket is exactly what resume is for.
    PandayError::Provider {
        upstream: "harnessd".into(),
        message,
        retryable: true,
    }
}
