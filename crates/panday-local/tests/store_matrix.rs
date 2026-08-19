//! M18.3 — one conformance suite, three stores (docs/18).
//!
//! > "SQLite event store passes the same harness suite as PG (one test matrix, two
//! > stores)."
//!
//! The matrix is `panday_harness::store_conformance::run`, and this file is the list of
//! stores it runs against. Postgres joins by adding one entry when M3.5 brings it; that is
//! the point of writing the invariants once rather than per store.

use panday_harness::store_conformance::{run, Factory};
use panday_harness::{EventStore, JsonlStore, MemoryStore};
use panday_local::sqlite::SqliteStore;
use std::sync::Arc;

fn scratch(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("panday-store-matrix-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_store_conforms() {
    // The reference implementation. If this ever fails, the suite is wrong, not the store.
    let factory: Factory =
        Arc::new(|| Box::pin(async { Arc::new(MemoryStore::new()) as Arc<dyn EventStore> }));
    run("MemoryStore", factory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_store_conforms() {
    let factory: Factory = Arc::new(|| {
        Box::pin(async {
            Arc::new(SqliteStore::in_memory().await.expect("open")) as Arc<dyn EventStore>
        })
    });
    run("SqliteStore", factory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn jsonl_store_conforms() {
    // The debugging artifact store (M21.3) is held to the same invariants. It is
    // single-session by design — one file, one session — which the suite allows as long as
    // a second session is *refused* rather than mixed in.
    let dir = scratch("jsonl");
    let counter = std::sync::atomic::AtomicUsize::new(0);
    let factory: Factory = Arc::new(move || {
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = dir.join(format!("session-{n}.jsonl"));
        Box::pin(
            async move { Arc::new(JsonlStore::open(path).expect("open")) as Arc<dyn EventStore> },
        )
    });
    run("JsonlStore", factory).await;
}

// ── SQLite specifics ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_sqlite_session_survives_a_reopen() {
    // The reason SQLite is the offline store: a laptop closes, and the events are still
    // there — including the durability that `synchronous = FULL` buys (docs/13
    // §persist-before-proceed).
    let dir = scratch("reopen");
    let db = dir.join("panday.db");
    let session = panday_types::SessionId::new();

    {
        let store = SqliteStore::open(&db).await.unwrap();
        for seq in 1..=3 {
            store
                .append(envelope(session, seq))
                .await
                .unwrap_or_else(|e| panic!("append {seq}: {e}"));
        }
    }

    let reopened = SqliteStore::open(&db).await.unwrap();
    assert_eq!(reopened.read_after(session, 0).await.unwrap().len(), 3);
    assert_eq!(reopened.next_seq(session).await.unwrap(), 4);
    assert_eq!(reopened.sessions().await.unwrap(), vec![session]);
}

#[tokio::test(flavor = "multi_thread")]
async fn many_sessions_live_in_one_database() {
    // The difference from `JsonlStore` that matters on a laptop: months of sessions in one
    // file, each an independent single-writer log.
    let store = SqliteStore::in_memory().await.unwrap();
    let sessions: Vec<_> = (0..5).map(|_| panday_types::SessionId::new()).collect();
    for session in &sessions {
        for seq in 1..=2 {
            store.append(envelope(*session, seq)).await.unwrap();
        }
    }
    assert_eq!(store.sessions().await.unwrap().len(), 5);
    for session in &sessions {
        assert_eq!(store.read_after(*session, 0).await.unwrap().len(), 2);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_event_kind_survives_the_database() {
    // docs/03: unknown kinds are ignored-and-preserved. Events are stored as JSON text
    // precisely so a newer version's event comes back byte-for-byte — a column layout
    // would drop the fields it had no column for, and the fold would be wrong forever.
    let store = SqliteStore::in_memory().await.unwrap();
    let raw = serde_json::json!({
        "v": 1,
        "session_id": "01930000-0000-7000-8000-000000000009",
        "seq": 1,
        "at": "2026-01-15T12:00:00Z",
        "event": "cache_warmed",
        "tokens_primed": 512
    });
    let envelope: panday_types::event::Envelope = serde_json::from_value(raw).unwrap();
    let session = envelope.session_id;
    store.append(envelope).await.unwrap();

    let read = store.read_after(session, 0).await.unwrap();
    assert!(read[0].event.is_unknown());
    // And the payload is intact, not just the tag.
    let json = serde_json::to_value(&read[0]).unwrap();
    assert_eq!(json["event"], "cache_warmed");
    assert_eq!(json["tokens_primed"], 512);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_exports_to_a_file_panday_replay_can_read() {
    // The replay tool takes a log file (M21.3); a session in a database is not one. This is
    // the bridge, and it matters because the person debugging is usually not the person
    // whose laptop it happened on.
    let dir = scratch("export");
    let store = SqliteStore::in_memory().await.unwrap();
    let session = panday_types::SessionId::new();
    store
        .append(user_message(session, 1, "why does it fail?"))
        .await
        .unwrap();
    store.append(envelope(session, 2)).await.unwrap();

    let out = dir.join("session.jsonl");
    let count = store.export_jsonl(session, &out).await.unwrap();
    assert_eq!(count, 2);

    let events = panday_harness::read_log(&out).expect("a replayable log");
    assert_eq!(events.len(), 2);
    let rendered = panday_harness::render(&events, Default::default());
    assert!(rendered.contains("why does it fail?"), "{rendered}");
}

fn envelope(session: panday_types::SessionId, seq: u64) -> panday_types::event::Envelope {
    panday_types::event::Envelope {
        v: 1,
        session_id: session,
        seq,
        at: time::OffsetDateTime::UNIX_EPOCH,
        turn_id: None,
        event: panday_types::event::Event::TurnStarted {
            model: panday_types::model::ModelRef("local/qwen3.5-4b".into()),
            parent: None,
        },
    }
}

fn user_message(
    session: panday_types::SessionId,
    seq: u64,
    text: &str,
) -> panday_types::event::Envelope {
    panday_types::event::Envelope {
        v: 1,
        session_id: session,
        seq,
        at: time::OffsetDateTime::UNIX_EPOCH,
        turn_id: None,
        event: panday_types::event::Event::UserMessage {
            content: vec![panday_types::model::ContentBlock::Text {
                text: text.to_string(),
            }],
            source: panday_types::event::ClientKind::Cli,
        },
    }
}
