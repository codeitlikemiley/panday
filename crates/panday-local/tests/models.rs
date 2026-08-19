//! M18.2 — pulling, verifying and supervising, without needing a model or a model server.
//!
//! The two things worth testing here are the ones that protect a user from a bad download and from
//! a crash loop; neither needs 4GB of weights to exercise.

use panday_local::catalog::{Index, Mirror, ModelArtifact};
use panday_local::models::{ModelError, Store};
use panday_local::supervisor::{Health, ServerConfig, State, Supervisor, SupervisorError};
use panday_types::capability::{CapabilityProfile, Provenance};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

const BODY: &[u8] = b"not really a gguf, but it hashes the same way";

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn artifact(id: &str, url: String, sha256: String) -> ModelArtifact {
    ModelArtifact {
        id: id.into(),
        url,
        sha256,
        license: "Apache-2.0".into(),
        size_bytes: BODY.len() as u64,
        ram_estimate_mb: 6_000,
        profile: CapabilityProfile {
            max_context_tokens: 16_000,
            json_reliability: 0.7,
            tool_reliability: 0.6,
            vision: false,
            max_subagents: 1,
            provenance: Provenance::Declared,
        },
    }
}

/// A one-file HTTP server, so the download path is exercised over a real socket rather than mocked
/// out — the bug this catches is in the streaming and hashing, which a mock would skip.
async fn serve(body: &'static [u8]) -> String {
    use axum::routing::get;
    let app = axum::Router::new().route("/{*path}", get(move || async move { body }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("panday-models-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn a_pull_verifies_while_it_streams() {
    let base = serve(BODY).await;
    let dir = scratch("pull");
    let store = Store::new(&dir);
    let a = artifact("qwen3.5-4b-q4", format!("{base}/qwen.gguf"), digest(BODY));

    let installed = store.pull(&a, None).await.expect("pull");
    assert_eq!(installed.size_bytes, BODY.len() as u64);
    assert_eq!(std::fs::read(&installed.path).unwrap(), BODY);
    assert!(store.verify(&a).is_ok());
    assert_eq!(store.list().len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_download_that_is_not_the_signed_file_leaves_nothing_behind() {
    // The failure this prevents: a mirror serving a different file at the right name, and a
    // half-verified artifact sitting on disk waiting for someone to rename it.
    let base = serve(BODY).await;
    let dir = scratch("mismatch");
    let store = Store::new(&dir);
    let a = artifact("qwen3.5-4b-q4", format!("{base}/qwen.gguf"), "b".repeat(64));

    let err = store.pull(&a, None).await.unwrap_err();
    assert!(matches!(err, ModelError::DigestMismatch { .. }), "{err}");
    assert!(store.list().is_empty(), "nothing is installed");
    // Not even a `.partial` — a rejected file that stays on disk is one somebody eventually keeps.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn pulling_twice_verifies_rather_than_downloads_again() {
    let base = serve(BODY).await;
    let dir = scratch("twice");
    let store = Store::new(&dir);
    let a = artifact("qwen3.5-4b-q4", format!("{base}/qwen.gguf"), digest(BODY));
    store.pull(&a, None).await.unwrap();

    // Point the artifact at a URL that does not resolve. A second pull that tried to download
    // would fail; one that verifies what is already there does not.
    let unreachable = artifact(
        "qwen3.5-4b-q4",
        "http://127.0.0.1:1/qwen.gguf".into(),
        digest(BODY),
    );
    assert!(store.pull(&unreachable, None).await.is_ok());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_rotted_file_is_caught_by_verify_and_by_the_next_pull() {
    let base = serve(BODY).await;
    let dir = scratch("rot");
    let store = Store::new(&dir);
    let a = artifact("qwen3.5-4b-q4", format!("{base}/qwen.gguf"), digest(BODY));
    let installed = store.pull(&a, None).await.unwrap();

    std::fs::write(&installed.path, b"corrupted").unwrap();
    assert!(matches!(
        store.verify(&a),
        Err(ModelError::DigestMismatch { .. })
    ));
    // `pull` re-verifies what it finds, so "just pull it again" surfaces the problem too.
    assert!(store.pull(&a, None).await.is_err());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_mirror_serves_the_same_artifact_under_the_same_hash() {
    // What an enterprise mirror is for (docs/18): a different host, the same signed hash. The hash
    // is what makes the mirror a convenience rather than a trust boundary.
    let base = serve(BODY).await;
    let dir = scratch("mirror");
    let store = Store::new(&dir);
    let a = artifact(
        "qwen3.5-4b-q4",
        "https://models.panday.dev/gguf/qwen.gguf".into(),
        digest(BODY),
    );

    let mirror = Mirror::new(&base);
    assert!(store.pull(&a, Some(&mirror)).await.is_ok());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn remove_and_list_agree() {
    let dir = scratch("rm");
    let store = Store::new(&dir);
    std::fs::write(store.path_for("a"), b"x").unwrap();
    std::fs::write(store.path_for("b"), b"y").unwrap();
    // A stray file that is not a model is not a model.
    std::fs::write(dir.join("README.txt"), b"z").unwrap();

    assert_eq!(
        store
            .list()
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    store.remove("a").unwrap();
    assert_eq!(store.list().len(), 1);
    assert!(matches!(
        store.remove("a"),
        Err(ModelError::NotInstalled(_))
    ));

    std::fs::remove_dir_all(&dir).ok();
}

// ── supervision ────────────────────────────────────────────────────────────────

/// Ready as soon as it is asked, or never — enough to drive the state machine without an
/// inference server.
struct Always(bool);

#[async_trait::async_trait]
impl Health for Always {
    async fn ready(&self) -> bool {
        self.0
    }
}

/// Counts how many times the supervisor asked, so a restart is observable.
#[derive(Default)]
struct Counting {
    asked: AtomicU32,
    ready: AtomicBool,
}

#[async_trait::async_trait]
impl Health for Counting {
    async fn ready(&self) -> bool {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.ready.load(Ordering::SeqCst)
    }
}

fn config(args: &[&str]) -> ServerConfig {
    ServerConfig {
        binary: "/bin/sh".into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        base_url: "http://127.0.0.1:0".into(),
        startup_timeout: Duration::from_secs(3),
        max_restarts: 2,
        window: Duration::from_secs(60),
    }
}

#[tokio::test]
async fn a_server_that_comes_up_is_healthy() {
    let s = Supervisor::start(config(&["-c", "sleep 30"]), Arc::new(Always(true)))
        .await
        .expect("start");
    assert_eq!(s.state(), State::Healthy);
    s.stop();
}

#[tokio::test]
async fn a_server_that_exits_immediately_is_reported_now_not_after_the_timeout() {
    // Waiting out a two-minute startup timeout to say "it died in 30ms" wastes the one piece of
    // attention the user was going to give this.
    let started = std::time::Instant::now();
    let err = Supervisor::start(config(&["-c", "exit 3"]), Arc::new(Always(false)))
        .await
        .unwrap_err();
    assert!(matches!(err, SupervisorError::ExitedEarly(_)), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "waited too long"
    );
}

#[tokio::test]
async fn a_server_that_never_answers_the_health_check_is_killed() {
    let mut c = config(&["-c", "sleep 30"]);
    c.startup_timeout = Duration::from_millis(500);
    let err = Supervisor::start(c, Arc::new(Always(false)))
        .await
        .unwrap_err();
    assert!(matches!(err, SupervisorError::Unhealthy { .. }), "{err}");
}

#[tokio::test]
async fn a_crash_is_restarted() {
    let health = Arc::new(Counting::default());
    health.ready.store(true, Ordering::SeqCst);

    // Lives ~200ms, then dies. The supervisor should bring it back.
    let s = Supervisor::start(
        config(&["-c", "sleep 0.2; exit 1"]),
        health.clone() as Arc<dyn Health>,
    )
    .await
    .expect("start");
    assert_eq!(s.state(), State::Healthy);

    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        health.asked.load(Ordering::SeqCst) > 1,
        "the supervisor never re-checked, so it never restarted"
    );
    s.stop();
}

#[tokio::test]
async fn a_crash_loop_gives_up_and_keeps_the_reason() {
    // A model that crashes on load crashes on every load. Restarting forever burns a battery and
    // buries the one error message that explains why.
    let s = Supervisor::start(
        config(&["-c", "sleep 0.1; exit 9"]),
        Arc::new(Always(true)) as Arc<dyn Health>,
    )
    .await
    .expect("start");

    for _ in 0..40 {
        if matches!(s.state(), State::Failed { .. }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    match s.state() {
        State::Failed { detail } => {
            assert!(detail.contains("restarts"), "{detail}");
            // The exit status a human needs, not just "it failed".
            assert!(detail.contains('9'), "{detail}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn stopping_is_not_a_crash() {
    // A supervisor that fights its own shutdown is a process nobody can stop.
    let health = Arc::new(Counting::default());
    health.ready.store(true, Ordering::SeqCst);
    let s = Supervisor::start(
        config(&["-c", "sleep 30"]),
        health.clone() as Arc<dyn Health>,
    )
    .await
    .unwrap();
    s.stop();
    assert_eq!(s.state(), State::Stopped);

    let asked = health.asked.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        health.asked.load(Ordering::SeqCst),
        asked,
        "still supervising after stop"
    );
}

#[tokio::test]
async fn the_http_health_check_reads_a_real_response() {
    use panday_local::supervisor::HttpHealth;

    let app = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    assert!(HttpHealth::new(&format!("http://{addr}")).ready().await);
    // A port with nothing on it is not healthy, and does not hang waiting to find out.
    assert!(!HttpHealth::new("http://127.0.0.1:1").ready().await);
}

#[test]
fn the_llama_server_defaults_bind_to_loopback() {
    // The offline tier's promise is that nothing leaves the machine; a server bound to 0.0.0.0
    // breaks it silently on a café network (ADR-011).
    let c = ServerConfig::llama_server(std::path::Path::new("/models/qwen.gguf"), 8081);
    assert!(c.args.windows(2).any(|w| w == ["--host", "127.0.0.1"]));
    assert_eq!(c.base_url, "http://127.0.0.1:8081");
}

#[test]
fn the_example_catalog_satisfies_our_own_rules() {
    // The example is the format's documentation; one that does not parse would teach the wrong
    // shape. The licence allowlist is the check most likely to be quietly wrong, and it runs here.
    let raw = include_bytes!("../catalog/models.example.json");
    let index = Index::parse_unverified(raw).expect("the example catalog must parse");
    assert_eq!(index.models.len(), 2);
    // Deliberately unsigned and digest-free: it is an example, and a catalog with invented digests
    // would fail on first use while looking authoritative.
    assert!(index.models.iter().all(|m| m.sha256 == "0".repeat(64)));
}

// ── M18.5: the alternate runner ───────────────────────────────────────────────

#[test]
fn both_runners_bind_to_loopback_and_serve_the_port_they_were_given() {
    // The only contract that matters: an OpenAI-compatible server, on this port, reachable only
    // from this machine. Everything else about the two command lines is their own business.
    use panday_local::supervisor::Runner;

    for runner in [Runner::LlamaServer, Runner::MistralRs] {
        let c = runner.config(std::path::Path::new("/models/qwen3.5-4b.gguf"), 8099);
        assert_eq!(c.base_url, "http://127.0.0.1:8099");
        assert!(
            c.args.iter().any(|a| a == "127.0.0.1"),
            "{runner:?} does not bind to loopback: {:?}",
            c.args
        );
        assert!(
            c.args.iter().any(|a| a == "8099"),
            "{runner:?}: {:?}",
            c.args
        );
        assert!(
            c.args.iter().any(|a| a.contains("qwen3.5-4b")),
            "{runner:?}: {:?}",
            c.args
        );
    }
}

#[test]
fn mistralrs_takes_the_model_as_a_subcommand_which_is_the_whole_difference() {
    use panday_local::supervisor::Runner;
    let c = Runner::MistralRs.config(std::path::Path::new("/models/qwen3.5-4b.gguf"), 8081);
    assert_eq!(c.binary, "mistralrs-server");
    // `gguf -m <dir> -f <file>`, not `-m <path>`.
    let gguf = c
        .args
        .iter()
        .position(|a| a == "gguf")
        .expect("the subcommand");
    assert_eq!(c.args[gguf + 1], "-m");
    assert_eq!(c.args[gguf + 2], "/models");
    assert_eq!(c.args[gguf + 4], "qwen3.5-4b.gguf");
}

#[test]
fn a_runner_name_is_forgiving_but_not_a_guess() {
    use panday_local::supervisor::Runner;
    assert_eq!(Runner::parse("llama.cpp"), Some(Runner::LlamaServer));
    assert_eq!(Runner::parse("mistral.rs"), Some(Runner::MistralRs));
    // An unknown name is an error rather than a silent default: starting the wrong server and
    // failing a health check tells the user nothing about what went wrong.
    assert_eq!(Runner::parse("vllm"), None);
}

#[test]
fn the_argument_list_can_be_replaced_when_a_runners_flags_move() {
    // Neither binary is present here, so these command lines are the one thing in the crate that
    // cannot be verified against reality. The override is what keeps a version bump from being an
    // outage for somebody whose model will not start.
    use panday_local::supervisor::Runner;
    let c = Runner::LlamaServer
        .config(std::path::Path::new("/m.gguf"), 8081)
        .with_args(vec!["--brand-new-flag".into()]);
    assert_eq!(c.args, ["--brand-new-flag"]);
    assert_eq!(
        c.base_url, "http://127.0.0.1:8081",
        "the URL is not part of the override"
    );
}
