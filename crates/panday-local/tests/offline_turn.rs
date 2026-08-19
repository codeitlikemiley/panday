//! M18.1 — `panday local` boots the whole composition and completes a turn with tools,
//! with nothing leaving the machine (docs/18).
//!
//! The model server here is a fake OpenAI-compatible endpoint on loopback rather than a
//! real llama-server. That is the honest boundary of what CI can prove: the composition,
//! the wire dialect, the jail, the log and the rendering are all real, and "an actual
//! llama-server answers" is a one-line `#[ignore]`d test for whoever has one running.
//! docs/18's own claim is that the gateway's local adapter "doesn't care which" server it
//! is, so a compatible fake exercises the same path.

use panday_harness::Profile;
use panday_local::{is_loopback, Local, LocalConfig, LocalError};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("panday-local-{tag}-{nanos}"));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A minimal OpenAI-compatible `/v1/chat/completions` that streams SSE.
///
/// Two turns: read a file, then answer. Exactly what a small local model would do, and
/// enough to prove the loop, the jail and the reducer are all wired.
async fn fake_llama() -> String {
    use axum::response::sse::{Event, Sse};
    use axum::routing::post;
    use axum::Router;

    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let calls = calls.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let frames: Vec<String> = if n == 0 {
                    vec![
                        serde_json::json!({
                            "choices": [{"index": 0, "delta": {"content": "Let me look at the file."}}]
                        })
                        .to_string(),
                        serde_json::json!({
                            "choices": [{"index": 0, "delta": {"tool_calls": [{
                                "index": 0,
                                "id": "call_local_0",
                                "type": "function",
                                "function": {"name": "read_file", "arguments": "{\"path\":\"src/lib.rs\"}"}
                            }]}}]
                        })
                        .to_string(),
                        serde_json::json!({
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                            "usage": {"prompt_tokens": 800, "completion_tokens": 20,
                                      "prompt_tokens_details": {"cached_tokens": 0}}
                        })
                        .to_string(),
                    ]
                } else {
                    vec![
                        serde_json::json!({
                            "choices": [{"index": 0, "delta": {"content": "The subtraction should be an addition."}}]
                        })
                        .to_string(),
                        serde_json::json!({
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                            "usage": {"prompt_tokens": 900, "completion_tokens": 30,
                                      "prompt_tokens_details": {"cached_tokens": 700}}
                        })
                        .to_string(),
                    ]
                };
                let stream = futures_util::stream::iter(
                    frames
                        .into_iter()
                        .map(|f| Ok::<_, std::convert::Infallible>(Event::default().data(f)))
                        .chain(std::iter::once(Ok(Event::default().data("[DONE]")))),
                );
                Sse::new(stream)
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{}", addr.port())
}

// ── Zero egress ──────────────────────────────────────────────────────────────

#[test]
fn only_loopback_counts_as_local() {
    for url in [
        "http://127.0.0.1:8080",
        "http://localhost:11434",
        "http://[::1]:8080",
        "http://0.0.0.0:8080",
    ] {
        assert!(is_loopback(url), "{url}");
    }
    for url in [
        "https://api.anthropic.com",
        "http://192.168.1.10:8080",
        "http://model.internal:8080",
        // The trick this exists to refuse: a name that resolves to loopback today and
        // somewhere else tomorrow.
        "http://localhost.evil.example:8080",
    ] {
        assert!(!is_loopback(url), "{url}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_base_url_is_refused_at_boot() {
    // Everything else about the offline tier is configuration — one adapter, one pool, a
    // policy file with nowhere to go — and configuration is what gets changed in a hurry.
    // This is the part that cannot be reconfigured into egress.
    let dir = TempDir::new("egress");
    let err = match Local::boot(LocalConfig::new(dir.path()).base_url("https://api.anthropic.com"))
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("a remote base URL must be refused"),
    };
    assert!(matches!(err, LocalError::NotLoopback(_)), "{err:?}");
    assert!(err.to_string().contains("loopback-only"), "{err}");
}

// ── The end-to-end turn ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_whole_turn_runs_offline_with_real_tools() {
    let dir = TempDir::new("turn");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a - b }\n",
    )
    .unwrap();

    let base = fake_llama().await;
    let mut local = Local::boot(
        LocalConfig::new(dir.path())
            .base_url(&base)
            .profile(Profile::Dev),
    )
    .await
    .expect("boot");

    let rendered = local.turn("why does the test fail?").await.expect("turn");

    // The rendering is the replay format — one renderer for the CLI, the hosted session
    // and the offline tier (docs/18: "the client cannot tell").
    assert!(rendered.contains("why does the test fail?"), "{rendered}");
    assert!(rendered.contains("→ read_file"), "{rendered}");
    assert!(
        rendered.contains("The subtraction should be an addition."),
        "{rendered}"
    );
    assert!(rendered.contains("EndTurn"), "{rendered}");
    // The tool really ran in the jail and really read the file.
    assert!(
        rendered.contains("pub fn add") || rendered.contains("a - b"),
        "{rendered}"
    );

    // Metered, at zero money: docs/18 keeps local usage measured so it can sync later.
    let usage = local.usage();
    assert!(!usage.is_empty(), "a local turn is still metered");
    assert!(usage.iter().any(|u| u.usage.input_tokens > 0));
    assert!(usage.iter().all(|u| u.model.0.starts_with("local/")));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_log_is_a_single_file_a_replay_can_read() {
    // docs/18 wants a single-file store; `JsonlStore` is that shape, and the point of
    // choosing it is that `panday replay` already reads it (SQLite parity is M18.3).
    let dir = TempDir::new("log");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn add() {}\n").unwrap();

    let base = fake_llama().await;
    let mut local = Local::boot(LocalConfig::new(dir.path()).base_url(&base))
        .await
        .unwrap();
    let log = local.log_path().to_path_buf();
    local.turn("look at it").await.unwrap();

    assert!(log.exists(), "{}", log.display());
    let events = panday_harness::read_log(&log).expect("a replayable log");
    assert!(events.len() > 3, "{}", events.len());
    // Gapless, because the store enforces it — a laptop that dies mid-turn resumes from
    // this file and nothing else.
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>()
    );
    let replayed = panday_harness::render(&events, Default::default());
    assert!(replayed.contains("look at it"), "{replayed}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_boot_on_the_same_log_continues_it() {
    // The offline recovery story: reopen the file, keep the seqs, no reconciliation.
    let dir = TempDir::new("resume");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn add() {}\n").unwrap();
    let base = fake_llama().await;

    let log = dir.path().join("session.jsonl");
    {
        let mut first = Local::boot(LocalConfig::new(dir.path()).base_url(&base).log(&log))
            .await
            .unwrap();
        first.turn("first question").await.unwrap();
    }
    let after_first = panday_harness::read_log(&log).unwrap().len();

    let base2 = fake_llama().await;
    let mut second = Local::boot(LocalConfig::new(dir.path()).base_url(&base2).log(&log))
        .await
        .unwrap();
    let rendered = second.turn("second question").await.unwrap();

    let events = panday_harness::read_log(&log).unwrap();
    assert!(events.len() > after_first, "the log grew");
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>(),
        "seqs stay gapless across a reboot"
    );
    // The second boot renders only what it did — not the first session again.
    assert!(!rendered.contains("first question"), "{rendered}");
    assert!(rendered.contains("second question"), "{rendered}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a real llama-server on 127.0.0.1:8080 (PANDAY_LOCAL_BASE_URL to override)"]
async fn a_real_llama_server_answers() {
    // docs/18 M18.1's literal wording is "against an already-running llama-server". This
    // is that test; it is `#[ignore]`d because CI has no GGUF and no GPU, and a suite that
    // silently skipped it would let the composition rot.
    let dir = TempDir::new("live");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn add() {}\n").unwrap();

    let base =
        std::env::var("PANDAY_LOCAL_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let model = std::env::var("PANDAY_LOCAL_MODEL").unwrap_or_else(|_| "local/qwen3.5-4b".into());

    let mut local = Local::boot(LocalConfig::new(dir.path()).base_url(base).model(model))
        .await
        .expect("boot against a running server");
    let rendered = local
        .turn("read src/lib.rs and tell me what the function does")
        .await
        .expect("a real turn");
    println!("{rendered}");
    assert!(
        rendered.contains("EndTurn") || rendered.contains("MaxSteps"),
        "{rendered}"
    );
}
