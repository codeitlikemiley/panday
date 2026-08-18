//! M3.3 — the AEP WebSocket endpoint and resume-after-seq.
//!
//! docs/03's acceptance: "create session, stream events, resume-after-seq
//! proven by killing the connection mid-turn."
//!
//! The proof has to involve a real socket that really dies, so this drives the
//! server over TCP with a minimal WebSocket client written inline. Hand-rolled
//! rather than pulling in `tokio-tungstenite`: it is not in the docs/02
//! dependency table, and reading unmasked text frames is a small, well-defined
//! piece of the RFC.

use panday_harnessd::{router, AppState};
use panday_types::event::{ClientKind, Envelope, Event};
use panday_types::model::ContentBlock;
use panday_types::SessionId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug)]
struct WsClient {
    stream: TcpStream,
    buf: Vec<u8>,
}

enum Frame {
    Text(String),
    Close,
    Other,
}

impl WsClient {
    async fn connect(addr: &str, path: &str) -> Result<Self, String> {
        let mut stream = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| e.to_string())?;

        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("server closed during handshake".into());
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = find(&buf, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                if !head.starts_with("HTTP/1.1 101") {
                    return Err(format!(
                        "upgrade refused: {}",
                        head.lines().next().unwrap_or("")
                    ));
                }
                buf.drain(..pos + 4);
                break;
            }
        }
        Ok(Self { stream, buf })
    }

    async fn next_text(&mut self) -> Option<String> {
        loop {
            if let Some(frame) = self.take_frame() {
                match frame {
                    Frame::Text(t) => return Some(t),
                    Frame::Close => return None,
                    Frame::Other => continue,
                }
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// Server-to-client frames are never masked, which keeps this short.
    fn take_frame(&mut self) -> Option<Frame> {
        if self.buf.len() < 2 {
            return None;
        }
        let opcode = self.buf[0] & 0x0f;
        let len_byte = self.buf[1] & 0x7f;

        let (len, header) = match len_byte {
            126 if self.buf.len() >= 4 => {
                (u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize, 4)
            }
            127 if self.buf.len() >= 10 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&self.buf[2..10]);
                (u64::from_be_bytes(b) as usize, 10)
            }
            n if n < 126 => (n as usize, 2),
            _ => return None,
        };

        if self.buf.len() < header + len {
            return None;
        }
        let payload = self.buf[header..header + len].to_vec();
        self.buf.drain(..header + len);

        Some(match opcode {
            0x1 => Frame::Text(String::from_utf8_lossy(&payload).to_string()),
            0x8 => Frame::Close,
            _ => Frame::Other,
        })
    }

    /// Drop the connection without a close frame — a process being killed,
    /// not a polite goodbye.
    fn kill(self) {
        drop(self.stream);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn serve() -> (String, AppState) {
    let state = AppState::new();
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, state)
}

fn user_event(session: SessionId, seq: u64, text: &str) -> Envelope {
    Envelope {
        v: panday_types::PROTOCOL_VERSION,
        session_id: session,
        seq,
        turn_id: None,
        at: time::OffsetDateTime::now_utc(),
        event: Event::UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            source: ClientKind::Cli,
        },
    }
}

fn seq_of(json: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(json).unwrap()["seq"]
        .as_u64()
        .unwrap()
}

#[tokio::test]
async fn a_client_receives_events_as_they_are_appended() {
    let (addr, state) = serve().await;
    let id = state.create();
    let mut ws = WsClient::connect(&addr, &format!("/v1/sessions/{}/ws", id.0))
        .await
        .unwrap();

    state.publish(id, user_event(id, 1, "first")).await.unwrap();
    state
        .publish(id, user_event(id, 2, "second"))
        .await
        .unwrap();

    assert_eq!(seq_of(&ws.next_text().await.unwrap()), 1);
    assert_eq!(seq_of(&ws.next_text().await.unwrap()), 2);
}

#[tokio::test]
async fn a_late_client_is_replayed_the_whole_log_first() {
    let (addr, state) = serve().await;
    let id = state.create();
    for seq in 1..=3 {
        state.publish(id, user_event(id, seq, "x")).await.unwrap();
    }

    let mut ws = WsClient::connect(&addr, &format!("/v1/sessions/{}/ws", id.0))
        .await
        .unwrap();
    for expected in 1..=3 {
        assert_eq!(seq_of(&ws.next_text().await.unwrap()), expected);
    }
}

#[tokio::test]
async fn killing_the_connection_mid_turn_loses_nothing_on_resume() {
    // The acceptance criterion, literally.
    let (addr, state) = serve().await;
    let id = state.create();
    let mut ws = WsClient::connect(&addr, &format!("/v1/sessions/{}/ws", id.0))
        .await
        .unwrap();

    state.publish(id, user_event(id, 1, "seen")).await.unwrap();
    state.publish(id, user_event(id, 2, "seen")).await.unwrap();
    assert_eq!(seq_of(&ws.next_text().await.unwrap()), 1);
    assert_eq!(seq_of(&ws.next_text().await.unwrap()), 2);

    // --- the connection dies mid-turn ---
    ws.kill();

    // Work continues while nobody is listening.
    for seq in 3..=6 {
        state
            .publish(id, user_event(id, seq, "missed"))
            .await
            .unwrap();
    }

    let mut resumed = WsClient::connect(&addr, &format!("/v1/sessions/{}/ws?after_seq=2", id.0))
        .await
        .unwrap();

    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(seq_of(&resumed.next_text().await.unwrap()));
    }
    assert_eq!(
        got,
        vec![3, 4, 5, 6],
        "resume must deliver exactly what was missed, in order"
    );
}

#[tokio::test]
async fn resuming_from_beyond_the_log_is_refused_rather_than_hanging() {
    // Regression: `last_sent` used to be initialised from the REQUESTED
    // after_seq, so a client resuming past the head filtered out every future
    // event as "already sent" and waited forever, receiving nothing and
    // seeing no error. Refusing surfaces an impossible client state instead
    // of hiding it.
    let (addr, state) = serve().await;
    let id = state.create();
    state.publish(id, user_event(id, 1, "old")).await.unwrap();

    let err = WsClient::connect(&addr, &format!("/v1/sessions/{}/ws?after_seq=99", id.0))
        .await
        .expect_err("a resume point beyond the log must be refused");
    assert!(err.contains("409"), "{err}");
}

#[tokio::test]
async fn resuming_from_exactly_the_head_is_allowed_and_tails_live() {
    // The normal steady-state case: the client has seen everything.
    let (addr, state) = serve().await;
    let id = state.create();
    state.publish(id, user_event(id, 1, "old")).await.unwrap();

    let mut ws = WsClient::connect(&addr, &format!("/v1/sessions/{}/ws?after_seq=1", id.0))
        .await
        .unwrap();

    state.publish(id, user_event(id, 2, "new")).await.unwrap();
    assert_eq!(seq_of(&ws.next_text().await.unwrap()), 2);
}

#[tokio::test]
async fn no_event_falls_between_the_replay_and_the_live_tail() {
    // The subscription is taken before the log is read precisely so an event
    // appended in between is not lost. A duplicate is filtered by seq; a gap
    // could never be recovered.
    let (addr, state) = serve().await;
    let id = state.create();
    state
        .publish(id, user_event(id, 1, "before"))
        .await
        .unwrap();

    let mut ws = WsClient::connect(&addr, &format!("/v1/sessions/{}/ws", id.0))
        .await
        .unwrap();

    for seq in 2..=20 {
        state
            .publish(id, user_event(id, seq, "during"))
            .await
            .unwrap();
    }

    let mut seen = Vec::new();
    for _ in 0..20 {
        match ws.next_text().await {
            Some(t) => seen.push(seq_of(&t)),
            None => break,
        }
    }
    assert_eq!(
        seen,
        (1..=20).collect::<Vec<u64>>(),
        "the stream must be gapless and in order"
    );
}

#[tokio::test]
async fn two_clients_both_see_the_stream() {
    let (addr, state) = serve().await;
    let id = state.create();
    let mut a = WsClient::connect(&addr, &format!("/v1/sessions/{}/ws", id.0))
        .await
        .unwrap();
    let mut b = WsClient::connect(&addr, &format!("/v1/sessions/{}/ws", id.0))
        .await
        .unwrap();

    state
        .publish(id, user_event(id, 1, "broadcast"))
        .await
        .unwrap();
    assert_eq!(seq_of(&a.next_text().await.unwrap()), 1);
    assert_eq!(seq_of(&b.next_text().await.unwrap()), 1);
}

#[tokio::test]
async fn an_unknown_session_is_refused_rather_than_upgraded() {
    let (addr, _state) = serve().await;
    let ghost = SessionId::new();
    let err = WsClient::connect(&addr, &format!("/v1/sessions/{}/ws", ghost.0))
        .await
        .expect_err("a session that does not exist must not upgrade");
    assert!(err.contains("404"), "{err}");
}

#[tokio::test]
async fn a_malformed_session_id_is_rejected() {
    let (addr, _state) = serve().await;
    let err = WsClient::connect(&addr, "/v1/sessions/not-a-uuid/ws")
        .await
        .expect_err("a malformed id must not upgrade");
    assert!(err.contains("400"), "{err}");
}

#[tokio::test]
async fn the_replay_endpoint_agrees_with_the_socket() {
    // Same log, same order — the read-only fallback docs/03 mentions must not
    // become a second source of truth.
    let (addr, state) = serve().await;
    let id = state.create();
    for seq in 1..=4 {
        state.publish(id, user_event(id, seq, "x")).await.unwrap();
    }

    let body = http_get(&format!(
        "http://{addr}/v1/sessions/{}/events?after_seq=2",
        id.0
    ))
    .await;
    let events: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, vec![3, 4]);
}

/// Minimal HTTP GET, so this crate needs no HTTP client dependency.
async fn http_get(url: &str) -> String {
    let without_scheme = url.trim_start_matches("http://");
    let (addr, path) = without_scheme.split_once('/').unwrap();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET /{path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let pos = text.find("\r\n\r\n").unwrap();
    text[pos + 4..].to_string()
}
