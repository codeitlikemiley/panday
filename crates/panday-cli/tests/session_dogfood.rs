//! M10.3's acceptance clause: "used by panday-cli (dogfood — the CLI has no private
//! APIs)".
//!
//! The test drives `panday session` against a real `panday-harnessd` over a real
//! socket. What it is really asserting is an architectural property: the CLI reaches
//! the platform only through `panday_sdk::sessions`, so anything it can do, a
//! customer's own client can do with the published SDK.

use panday_cli::{parse_args, run_session, Output};
use panday_harnessd::testing::ScriptedDriver;
use panday_harnessd::{router, AppState, SessionDriver};
use std::sync::Arc;

#[derive(Default)]
struct Captured {
    text: String,
}

impl Output for Captured {
    fn text(&mut self, s: &str) {
        self.text.push_str(s);
    }
    fn line(&mut self, s: &str) {
        self.text.push_str(s);
        self.text.push('\n');
    }
}

async fn serve() -> (String, Arc<ScriptedDriver>) {
    let driver = Arc::new(ScriptedDriver::new());
    let state = AppState::new().with_driver(driver.clone() as Arc<dyn SessionDriver>);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), driver)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_runs_a_whole_turn_through_the_public_sdk() {
    let (base, _driver) = serve().await;
    let cmd = parse_args(["session", "--url", &base, "fix", "the", "failing", "test"]).unwrap();
    let mut out = Captured::default();

    let resume_point = run_session(&cmd, &mut out).await.expect("the turn ran");

    // Rendered by the same renderer `panday replay` uses, so a live session and its
    // replay are the same text.
    assert!(out.text.contains("fix the failing test"), "{}", out.text);
    assert!(
        out.text.contains("── turn 1 · local/test ──"),
        "{}",
        out.text
    );
    assert!(out.text.contains("assistant"), "{}", out.text);
    assert!(out.text.contains("EndTurn"), "{}", out.text);
    // `--costs` is on for a live session: the number a user most wants during a turn
    // is what it is costing.
    assert!(out.text.contains("usage:"), "{}", out.text);
    assert!(out.text.contains("cache read 900"), "{}", out.text);

    assert_eq!(resume_point, 4);
    // And it tells the user how to come back, which is what makes `--after-seq`
    // usable from a shell rather than only from a library.
    assert!(out.text.contains("--after-seq 4"), "{}", out.text);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_can_resume_a_session_it_already_ran() {
    let (base, _driver) = serve().await;

    let first = parse_args(["session", "--url", &base, "start the work"]).unwrap();
    let mut out = Captured::default();
    let resume_point = run_session(&first, &mut out).await.unwrap();
    let session_id = out
        .text
        .lines()
        .find_map(|l| l.strip_prefix("session "))
        .expect("the id is printed")
        .to_string();

    // Second invocation: same session, resuming past what the first already showed.
    let second = parse_args([
        "session",
        "--url",
        &base,
        "--session",
        &session_id,
        "--after-seq",
        &resume_point.to_string(),
        "and now the next thing",
    ])
    .unwrap();
    let mut out2 = Captured::default();
    let next_point = run_session(&second, &mut out2).await.unwrap();

    assert!(next_point > resume_point);
    // Nothing from the first turn is repeated — resume replays what was missed, not
    // what was seen.
    assert!(
        !out2.text.contains("start the work"),
        "the first turn was replayed:\n{}",
        out2.text
    );
    assert!(
        out2.text.contains("and now the next thing"),
        "{}",
        out2.text
    );
    // The turn counter restarts because this invocation is a new rendering, not a
    // continuation of the previous process — the seqs are what carry continuity.
    assert!(out2.text.contains("── turn 1 ·"), "{}", out2.text);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_resume_point_is_reported_rather_than_hanging() {
    let (base, _driver) = serve().await;
    let create = parse_args(["session", "--url", &base, "hello"]).unwrap();
    let mut out = Captured::default();
    run_session(&create, &mut out).await.unwrap();
    let session_id = out
        .text
        .lines()
        .find_map(|l| l.strip_prefix("session "))
        .unwrap()
        .to_string();

    let bad = parse_args([
        "session",
        "--url",
        &base,
        "--session",
        &session_id,
        "--after-seq",
        "999",
        "hi",
    ])
    .unwrap();
    let err = run_session(&bad, &mut Captured::default())
        .await
        .expect_err("a resume point past the head cannot be honoured");
    assert!(err.contains("beyond") || err.contains("409"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_is_not_there_fails_with_a_usable_message() {
    // The most common failure in practice, and the one where a raw transport error
    // wastes the most time.
    let cmd = parse_args(["session", "--url", "http://127.0.0.1:1", "hi"]).unwrap();
    let err = run_session(&cmd, &mut Captured::default())
        .await
        .expect_err("nothing is listening");
    assert!(
        err.to_lowercase().contains("connection")
            || err.to_lowercase().contains("refused")
            || err.to_lowercase().contains("harnessd"),
        "{err}"
    );
}
