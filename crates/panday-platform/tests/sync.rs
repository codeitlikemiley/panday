//! M18.6 — an offline session lands in a cloud account, and the ledger reconciles.
//!
//! The properties worth testing are the ones a flaky connection exercises: pushing twice, pushing
//! half, pushing somebody else's log.

use panday_platform::{pg, sync};
use panday_types::event::{Envelope, Event};
use panday_types::model::{ModelRef, StopReason, Usage};
use panday_types::pricing::{PriceTable, Pricing};
use panday_types::{SessionId, TurnId};

async fn database() -> sqlx::PgPool {
    let url = pg::test_database_url()
        .expect("PANDAY_TEST_DATABASE_URL is unset — start deploy/integration-compose.yml first");
    let pool = pg::connect(&url).await.expect("connect");
    pg::migrate(&pool, &pg::migrations_dir())
        .await
        .expect("migrate");
    pool
}

/// A local session: one turn, real tokens, no money.
fn offline_log(session: SessionId) -> Vec<Envelope> {
    let turn = TurnId::new();
    let mut seq = 0u64;
    let mut next = |event: Event| {
        let envelope = Envelope {
            v: 1,
            session_id: session,
            seq,
            turn_id: Some(turn),
            at: time::OffsetDateTime::now_utc(),
            event,
        };
        seq += 1;
        envelope
    };

    vec![
        next(Event::TurnStarted {
            model: ModelRef("local/qwen3.5-4b".into()),
            parent: None,
        }),
        next(Event::UserMessage {
            content: vec![],
            source: panday_types::event::ClientKind::Cli,
        }),
        next(Event::AssistantMessage {
            content: vec![],
            usage: Usage {
                input_tokens: 1_200,
                output_tokens: 340,
                ..Default::default()
            },
        }),
        next(Event::TurnFinished {
            reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 1_200,
                output_tokens: 340,
                ..Default::default()
            },
            // Zero, and that is the honest number for a local model (ADR-007).
            cost_micros: 0,
        }),
    ]
}

/// Local models are free, and the price table says so explicitly — which is a different statement
/// from having no price at all (ADR-007).
fn prices() -> PriceTable {
    PriceTable::new().with("local/qwen3.5-4b", Pricing::local())
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_offline_session_appears_in_the_account() {
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let session = SessionId::new();
    let log = offline_log(session);

    let report = sync::push(&pool, account, &log, &prices()).await.unwrap();
    assert_eq!(report.stored, 4);
    assert_eq!(report.already_present, 0);
    assert_eq!(report.input_tokens, 1_200);
    assert_eq!(report.output_tokens, 340);

    // Readable back as the same events, in order — which is what makes `panday replay` work against
    // a synced session as well as a local file.
    let back = sync::session(&pool, account, session.0).await.unwrap();
    assert_eq!(back.len(), 4);
    assert_eq!(back[0].seq, 0);
    assert!(matches!(back[3].event, Event::TurnFinished { .. }));

    let listed = sync::sessions(&pool, account, 10).await.unwrap();
    assert!(listed
        .iter()
        .any(|(id, count)| *id == session.0 && *count == 4));
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn the_ledger_records_usage_and_no_money() {
    // docs/18: offline entries "still count against fair-use metering for free tiers". Zero cost,
    // real tokens — a free tier that recorded nothing could not enforce fair use, and one that
    // invented a cost would bill for electricity it did not buy.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let before = pg::balance_micros(&pool, account).await.unwrap();

    let report = sync::push(&pool, account, &offline_log(SessionId::new()), &prices())
        .await
        .unwrap();
    assert_eq!(report.ledger_entries, 1);
    assert_eq!(
        pg::balance_micros(&pool, account).await.unwrap(),
        before,
        "local inference must not move the balance"
    );
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn pushing_the_same_log_twice_changes_nothing() {
    // Reconnect, crash, reconnect. The whole design has to survive this without double billing or
    // duplicate events.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let log = offline_log(SessionId::new());

    let first = sync::push(&pool, account, &log, &prices()).await.unwrap();
    let second = sync::push(&pool, account, &log, &prices()).await.unwrap();

    assert_eq!(first.stored, 4);
    assert_eq!(second.stored, 0);
    assert_eq!(second.already_present, 4, "idempotency, seen from outside");
    assert_eq!(second.ledger_entries, 0, "the entry was already written");
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_partial_push_can_be_completed_later() {
    // A connection that drops mid-sync leaves a prefix. The next push carries the whole log again
    // and only the missing tail is stored.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let log = offline_log(SessionId::new());

    sync::push(&pool, account, &log[..2], &prices())
        .await
        .unwrap();
    let rest = sync::push(&pool, account, &log, &prices()).await.unwrap();
    assert_eq!(rest.stored, 2);
    assert_eq!(rest.already_present, 2);
    assert_eq!(rest.ledger_entries, 0, "the first push already reconciled");
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_log_with_a_gap_is_refused() {
    // Gaplessness is what makes `after_seq` a complete sync mechanism (docs/03). A log with a hole
    // in it is a log whose fold is wrong, and storing it would make the hole permanent.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let mut log = offline_log(SessionId::new());
    log.remove(1);

    assert!(matches!(
        sync::push(&pool, account, &log, &prices()).await,
        Err(sync::SyncError::Gap { .. })
    ));
    // And nothing was stored — the events go in one transaction, so a rejected push leaves no
    // prefix that the next push would read as a gap.
    assert!(sync::sessions(&pool, account, 10).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn two_sessions_in_one_push_are_refused() {
    // One push is one session: sessions are single-writer (ADR-002), and interleaving two logs
    // would make the seq check meaningless.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let mut log = offline_log(SessionId::new());
    log.extend(offline_log(SessionId::new()));

    assert!(matches!(
        sync::push(&pool, account, &log, &prices()).await,
        Err(sync::SyncError::MixedSessions(2))
    ));
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn one_account_never_reads_another_accounts_synced_session() {
    let pool = database().await;
    let mine = pg::create_account(&pool, "mine").await.unwrap();
    let theirs = pg::create_account(&pool, "theirs").await.unwrap();
    let session = SessionId::new();
    sync::push(&pool, theirs, &offline_log(session), &prices())
        .await
        .unwrap();

    assert!(sync::session(&pool, mine, session.0)
        .await
        .unwrap()
        .is_empty());
    assert!(sync::sessions(&pool, mine, 10).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_unreadable_event_is_stored_counted_and_not_hidden() {
    // docs/03: unknown kinds are ignored-and-preserved, so a laptop on a newer build does not lose
    // events by syncing to an older server. The risk that creates is a log that syncs
    // "successfully" and reconciles to nothing, which is why the count is reported.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let mut log = offline_log(SessionId::new());
    log.push(Envelope {
        v: 1,
        session_id: log[0].session_id,
        seq: 4,
        turn_id: log[0].turn_id,
        at: time::OffsetDateTime::now_utc(),
        event: Event::Unknown {
            payload: serde_json::json!({"event": "telemetry_v9", "from": "a newer build"})
                .as_object()
                .unwrap()
                .clone(),
        },
    });

    let report = sync::push(&pool, account, &log, &prices()).await.unwrap();
    assert_eq!(report.stored, 5, "the unknown event is stored, not dropped");
    assert_eq!(report.unknown_events, 1);

    // And it comes back out unchanged.
    let back = sync::session(&pool, account, log[0].session_id.0)
        .await
        .unwrap();
    assert!(matches!(back[4].event, Event::Unknown { .. }));
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn the_reconciled_entry_is_recomputed_not_taken_from_the_client() {
    // The laptop is the customer's machine. What it says about its own bill is a claim; the log is
    // the evidence, and the entry is rebuilt from it (M3.5's primitive, reused).
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let log = offline_log(SessionId::new());

    let report = sync::push(&pool, account, &log, &prices()).await.unwrap();
    let rebuilt = panday_platform::rebuild::from_log(&log, &prices());
    assert_eq!(report.input_tokens, rebuilt.usage.input_tokens);
    assert_eq!(report.output_tokens, rebuilt.usage.output_tokens);
    assert_eq!(rebuilt.total_micros, 0, "local is free, explicitly");
}
