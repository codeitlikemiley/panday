//! M10.3 — the sessions client over a real socket, including resume-after-seq
//! (docs/10 Layer 3, docs/03 §sync).
//!
//! The server is a real `panday-harnessd` on a real port, and the resume case kills
//! a real connection mid-turn: docs/03's acceptance for the endpoint is "resume-
//! after-seq proven by killing the connection mid-turn", and a test that closed the
//! stream politely would not be testing the thing that actually happens.

use panday_harnessd::testing::ScriptedDriver;
use panday_harnessd::{router, AppState, SessionDriver};
use panday_sdk::sessions::{After, ClientMessage, SessionsClient};
use panday_types::event::{Envelope, Event, PermDecision};
use std::sync::Arc;

async fn serve(driver: Arc<ScriptedDriver>) -> (String, AppState, Arc<ScriptedDriver>) {
    let state = AppState::new().with_driver(driver.clone() as Arc<dyn SessionDriver>);
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), state, driver)
}

/// Read events until `f` is satisfied, or the stream ends.
async fn collect_until(
    stream: &mut panday_sdk::sessions::SessionStream,
    mut f: impl FnMut(&Envelope) -> bool,
) -> Vec<Envelope> {
    let mut out = Vec::new();
    while let Some(item) = stream.next_event().await {
        let envelope = item.expect("a well-formed envelope");
        let done = f(&envelope);
        out.push(envelope);
        if done {
            break;
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_creates_a_session_sends_input_and_receives_the_events() {
    let (base, _, _driver) = serve(Arc::new(ScriptedDriver::default())).await;
    let client = SessionsClient::new(&base, None);

    let session = client.create().await.expect("create");
    let mut stream = client
        .subscribe(session, After::Beginning)
        .await
        .expect("subscribe");
    stream
        .send(ClientMessage::text("fix the failing test"))
        .await
        .expect("send");

    let events = collect_until(&mut stream, |e| {
        matches!(e.event, Event::TurnFinished { .. })
    })
    .await;

    let kinds: Vec<&str> = events.iter().filter_map(|e| e.event.kind()).collect();
    assert_eq!(
        kinds,
        [
            "user_message",
            "turn_started",
            "assistant_message",
            "turn_finished"
        ]
    );
    // Gapless, which is what makes `after_seq` a complete sync mechanism.
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(stream.resume_point(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_the_connection_mid_turn_loses_nothing() {
    // docs/03's acceptance, through the client: drop the socket after the second
    // event, reconnect with the last seq seen, and receive exactly the rest.
    let (base, _, _driver) = serve(Arc::new(ScriptedDriver::default())).await;
    let client = SessionsClient::new(&base, None);
    let session = client.create().await.unwrap();

    let mut stream = client.subscribe(session, After::Beginning).await.unwrap();
    stream.send(ClientMessage::text("go")).await.unwrap();
    let first = collect_until(&mut stream, |e| e.seq >= 2).await;
    assert_eq!(first.len(), 2);
    let resume_from = stream.resume_point();

    // The connection dies. Not closed politely — dropped, which is what a laptop
    // lid, a load balancer or a lost Wi-Fi does.
    drop(stream);

    let mut resumed = client.resume_from(session, resume_from).await.unwrap();
    let rest = collect_until(&mut resumed, |e| {
        matches!(e.event, Event::TurnFinished { .. })
    })
    .await;

    assert_eq!(
        rest.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![3, 4],
        "resume must deliver exactly what was missed, with no repeats and no gap"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resume_point_beyond_the_log_is_refused_rather_than_hanging() {
    // The bug this guards was real and silent: with `last_sent` initialised from the
    // requested `after_seq`, a client past the head had every future event filtered
    // as "already sent" and sat there receiving nothing, forever, with no error.
    let (base, _, _driver) = serve(Arc::new(ScriptedDriver::default())).await;
    let client = SessionsClient::new(&base, None);
    let session = client.create().await.unwrap();

    let err = match client.resume_from(session, 99).await {
        Err(e) => e,
        Ok(_) => panic!("a resume point past the head cannot be honoured"),
    };
    let text = err.to_string();
    assert!(
        text.contains("beyond") || text.contains("409"),
        "the client should surface the refusal, not a transport error: {text}"
    );
    // And it must not be reported as retryable: reconnecting into the same refusal
    // is a loop.
    assert!(!err.is_retryable(), "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permission_round_trip_happens_over_the_same_socket() {
    // One socket, one order: the client sees the request, answers on the same
    // connection, and receives the decision it caused.
    let (base, _, _driver) = serve(Arc::new(ScriptedDriver::parking())).await;
    let client = SessionsClient::new(&base, None);
    let session = client.create().await.unwrap();
    let mut stream = client.subscribe(session, After::Beginning).await.unwrap();

    stream
        .send(ClientMessage::text("run the tests"))
        .await
        .unwrap();
    let parked = collect_until(&mut stream, |e| {
        matches!(e.event, Event::PermissionRequest { .. })
    })
    .await;
    let call_id = parked
        .iter()
        .find_map(|e| match &e.event {
            Event::PermissionRequest { call_id, .. } => Some(*call_id),
            _ => None,
        })
        .expect("a permission request");

    stream
        .send(ClientMessage::Decide {
            call_id,
            decision: PermDecision::Allow,
        })
        .await
        .unwrap();

    let after = collect_until(&mut stream, |e| {
        matches!(e.event, Event::TurnFinished { .. })
    })
    .await;
    assert!(after.iter().any(
        |e| matches!(&e.event, Event::PermissionDecision { decision, .. }
            if *decision == PermDecision::Allow)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn after_latest_skips_history_and_after_beginning_replays_it() {
    let (base, state, driver) = serve(Arc::new(ScriptedDriver::default())).await;
    let client = SessionsClient::new(&base, None);
    let session = client.create().await.unwrap();

    // Produce some history without a client attached — through the *server's* driver,
    // not a second one: a fresh `ScriptedDriver` has its own seq counter, and using
    // one here collided at seq 1 and tripped the single-writer invariant. Which is
    // the invariant working: two writers is exactly what it exists to catch.
    driver.user_input(&state, session, "earlier".into()).await;

    let replayed = client.events(session, 0).await.unwrap();
    assert_eq!(replayed.len(), 4, "the log has the earlier turn");

    let mut latest = client.subscribe(session, After::Latest).await.unwrap();
    assert_eq!(latest.resume_point(), 4);
    // Nothing replayed; the next event is a new one.
    latest.send(ClientMessage::text("now")).await.unwrap();
    let fresh = collect_until(&mut latest, |e| e.seq >= 5).await;
    assert_eq!(fresh.first().map(|e| e.seq), Some(5));

    let mut from_start = client.subscribe(session, After::Beginning).await.unwrap();
    let history = collect_until(&mut from_start, |e| e.seq >= 4).await;
    assert_eq!(history.first().map(|e| e.seq), Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_accepts_no_input_says_so_instead_of_dropping_it() {
    // A read-only deployment (a dashboard, a replay viewer) has no driver. Input
    // must be refused audibly: a client whose message vanished would wait forever
    // for events that were never coming.
    let state = AppState::new();
    assert!(!state.accepts_input());
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = SessionsClient::new(format!("http://{addr}"), None);
    let session = client.create().await.unwrap();
    let mut stream = client.subscribe(session, After::Beginning).await.unwrap();
    stream.send(ClientMessage::text("hello")).await.unwrap();

    let err = stream
        .next_event()
        .await
        .expect("a close with a reason, not silence")
        .expect_err("the server refuses input");
    assert!(
        err.to_string().contains("does not accept session input"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_client_message_closes_the_socket_with_a_reason() {
    // Not ignored: a client sending the wrong shape needs to hear about it, and
    // continuing would leave it convinced the message was accepted.
    let (base, _, _driver) = serve(Arc::new(ScriptedDriver::default())).await;
    let client = SessionsClient::new(&base, None);
    let session = client.create().await.unwrap();
    let mut stream = client.subscribe(session, After::Beginning).await.unwrap();

    // Reach past the typed API on purpose — this is what a foreign client does.
    stream.send_raw("{\"type\":\"nonsense\"}").await.unwrap();
    let err = stream
        .next_event()
        .await
        .expect("a close frame")
        .expect_err("a reason");
    assert!(err.to_string().contains("not a client message"), "{err}");
}
