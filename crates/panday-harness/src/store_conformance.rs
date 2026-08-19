//! One conformance suite, every `EventStore` (M18.3, docs/18).
//!
//! > "SQLite event store passes the same harness suite as PG (one test matrix, two
//! > stores)."
//!
//! The matrix is this function. A store implementation calls it and inherits every
//! invariant the harness depends on; adding Postgres later is one more call site, not
//! another copy of these assertions. That matters because the invariants are not
//! obvious-and-local — "gapless `seq`" is what makes `after_seq` a complete sync
//! mechanism (docs/03), and a store that got it subtly wrong would fail somewhere far
//! away, like a client that silently stops receiving events.
//!
//! Public rather than `#[cfg(test)]` for the same reason `testing` is: the
//! implementations live in other crates.

use crate::{EventStore, StoreError};
use panday_types::event::{ClientKind, Envelope, Event};
use panday_types::model::{ContentBlock, ModelRef, StopReason, Usage};
use panday_types::SessionId;
use std::future::Future;
use std::sync::Arc;

/// Builds a fresh, empty store. Called several times; each call must produce a store that
/// shares nothing with the previous one.
pub type Factory = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn Future<Output = Arc<dyn EventStore>> + Send>> + Send + Sync,
>;

fn envelope(session: SessionId, seq: u64, event: Event) -> Envelope {
    Envelope {
        v: 1,
        session_id: session,
        seq,
        at: time::OffsetDateTime::UNIX_EPOCH,
        turn_id: None,
        event,
    }
}

fn user(text: &str) -> Event {
    Event::UserMessage {
        content: vec![ContentBlock::Text { text: text.into() }],
        source: ClientKind::Cli,
    }
}

/// Run every invariant against one store implementation.
///
/// Panics with a message naming the store on the first violation — a conformance failure
/// should read like "SqliteStore lost an event", not like an assertion in a shared file.
pub async fn run(name: &str, make: Factory) {
    appends_and_reads_back(name, &make).await;
    seq_must_be_gapless(name, &make).await;
    a_repeated_seq_is_refused(name, &make).await;
    next_seq_starts_at_one(name, &make).await;
    read_after_is_exclusive(name, &make).await;
    sessions_do_not_see_each_other(name, &make).await;
    every_event_kind_round_trips(name, &make).await;
}

async fn appends_and_reads_back(name: &str, make: &Factory) {
    let store = make().await;
    let session = SessionId::new();
    for seq in 1..=3 {
        store
            .append(envelope(session, seq, user(&format!("message {seq}"))))
            .await
            .unwrap_or_else(|e| panic!("{name}: append {seq}: {e}"));
    }
    let read = store.read_after(session, 0).await.unwrap();
    assert_eq!(read.len(), 3, "{name}: read back {} of 3", read.len());
    assert_eq!(
        read.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "{name}: events must come back in seq order"
    );
    assert_eq!(
        read[0].session_id, session,
        "{name}: the session id must survive the round trip"
    );
}

async fn seq_must_be_gapless(name: &str, make: &Factory) {
    // The invariant `after_seq` rests on. A store that accepted a gap would let a client
    // wait forever for an event that is never coming, with no error anywhere.
    let store = make().await;
    let session = SessionId::new();
    store
        .append(envelope(session, 1, user("one")))
        .await
        .unwrap();
    let err = store
        .append(envelope(session, 3, user("three")))
        .await
        .expect_err(&format!("{name}: a gap must be refused"));
    assert!(
        matches!(err, StoreError::SeqConflict(3)),
        "{name}: a gap should be a SeqConflict, got {err:?}"
    );
}

async fn a_repeated_seq_is_refused(name: &str, make: &Factory) {
    // Two writers on one session is what this catches, and it is the reason a session has
    // exactly one actor (ADR-002).
    let store = make().await;
    let session = SessionId::new();
    store
        .append(envelope(session, 1, user("one")))
        .await
        .unwrap();
    let err = store
        .append(envelope(session, 1, user("one again")))
        .await
        .expect_err(&format!("{name}: a repeated seq must be refused"));
    assert!(
        matches!(err, StoreError::SeqConflict(1)),
        "{name}: got {err:?}"
    );
}

async fn next_seq_starts_at_one(name: &str, make: &Factory) {
    let store = make().await;
    let session = SessionId::new();
    assert_eq!(
        store.next_seq(session).await.unwrap(),
        1,
        "{name}: an empty session starts at 1"
    );
    store
        .append(envelope(session, 1, user("one")))
        .await
        .unwrap();
    assert_eq!(store.next_seq(session).await.unwrap(), 2, "{name}");
}

async fn read_after_is_exclusive(name: &str, make: &Factory) {
    // `after_seq=N` means "everything I have not seen", so N itself must not come back —
    // an off-by-one here duplicates an event on every resume.
    let store = make().await;
    let session = SessionId::new();
    for seq in 1..=3 {
        store
            .append(envelope(session, seq, user("x")))
            .await
            .unwrap();
    }
    let read = store.read_after(session, 2).await.unwrap();
    assert_eq!(
        read.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![3],
        "{name}: read_after must be exclusive"
    );
    assert!(
        store.read_after(session, 3).await.unwrap().is_empty(),
        "{name}: nothing after the head"
    );
}

async fn sessions_do_not_see_each_other(name: &str, make: &Factory) {
    let store = make().await;
    let (a, b) = (SessionId::new(), SessionId::new());
    store.append(envelope(a, 1, user("mine"))).await.unwrap();
    // A second session in the same store starts its own seq at 1 — sessions are
    // independent single-writer logs, not one shared stream.
    match store.append(envelope(b, 1, user("also mine"))).await {
        Ok(()) => {
            let read = store.read_after(a, 0).await.unwrap();
            assert_eq!(read.len(), 1, "{name}: sessions leaked into each other");
            assert_eq!(read[0].session_id, a, "{name}");
            assert_eq!(store.next_seq(b).await.unwrap(), 2, "{name}");
        }
        // A single-session store (one file per session) is a legitimate shape; it must
        // *refuse* the second session rather than mix it in.
        Err(e) => assert!(
            matches!(e, StoreError::SeqConflict(_)),
            "{name}: a single-session store should refuse a second session, got {e:?}"
        ),
    }
}

async fn every_event_kind_round_trips(name: &str, make: &Factory) {
    // A store that serialized events lossily would corrupt the fold, and the fold is the
    // state (ADR-002). Two representative shapes: a nested content block and a numeric
    // usage record.
    let store = make().await;
    let session = SessionId::new();
    store
        .append(envelope(
            session,
            1,
            Event::TurnStarted {
                model: ModelRef("local/qwen3.5-4b".into()),
                parent: None,
            },
        ))
        .await
        .unwrap();
    store
        .append(envelope(
            session,
            2,
            Event::AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "answer".into(),
                }],
                usage: Usage {
                    input_tokens: 1_234,
                    output_tokens: 56,
                    cache_read_tokens: 1_000,
                    cache_write_tokens: 7,
                    cache_write_1h_tokens: 0,
                },
            },
        ))
        .await
        .unwrap();
    store
        .append(envelope(
            session,
            3,
            Event::TurnFinished {
                reason: StopReason::EndTurn,
                usage: Usage::default(),
                cost_micros: 42,
            },
        ))
        .await
        .unwrap();

    let read = store.read_after(session, 0).await.unwrap();
    assert_eq!(read.len(), 3, "{name}");
    match &read[1].event {
        Event::AssistantMessage { content, usage } => {
            assert_eq!(usage.input_tokens, 1_234, "{name}: usage did not survive");
            assert_eq!(usage.cache_read_tokens, 1_000, "{name}");
            assert!(
                matches!(&content[0], ContentBlock::Text { text } if text == "answer"),
                "{name}: content did not survive"
            );
        }
        other => panic!("{name}: event 2 came back as {other:?}"),
    }
    match &read[2].event {
        Event::TurnFinished { cost_micros, .. } => {
            assert_eq!(*cost_micros, 42, "{name}: cost did not survive")
        }
        other => panic!("{name}: event 3 came back as {other:?}"),
    }
}
