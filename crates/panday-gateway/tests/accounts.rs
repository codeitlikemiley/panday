//! Operator console: add Grok sessions and API keys without restarting.

use panday_gateway::adapters::pool::CredHub;
use panday_gateway::console::{self, ConsoleState};
use panday_gateway::{CollectUsage, Gateway};
use panday_router::PolicyRouter;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DEV_POLICY: &str = include_str!("../../panday-router/policy/dev.yaml");

fn state(hub: CredHub) -> ConsoleState {
    let gateway = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("xai", hub.xai.clone())
        .adapter("openai", hub.openai.clone())
        .adapter("anthropic", hub.anthropic.clone())
        .adapter("gemini", hub.gemini.clone())
        .build();
    ConsoleState {
        gateway: Arc::new(gateway),
        usage: Arc::new(CollectUsage::new()),
        listen: "127.0.0.1:0".into(),
        hub,
    }
}

async fn bind(hub: CredHub) -> String {
    let app = console::router(state(hub));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn http(addr: &str, req: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let location = text
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("location:"))
        .map(|l| {
            l.split_once(':')
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    (status, location, text)
}

#[tokio::test]
async fn accounts_page_is_html_without_wasm() {
    let addr = bind(CredHub::new()).await;
    let (_, _, body) = http(
        &addr,
        &format!("GET /accounts HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(body.contains("Import Grok CLI"), "{body}");
    assert!(body.contains("API key"), "{body}");
    assert!(
        body.contains("round_robin") || body.contains("round-robin"),
        "{body}"
    );
}

#[tokio::test]
async fn adding_an_openai_key_lists_last4_not_the_secret() {
    let hub = CredHub::new();
    let addr = bind(hub.clone()).await;
    let body = "provider=openai&label=free&key=sk-test-aaaa";
    let req = format!(
        "POST /console/accounts/api HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, location, _) = http(&addr, &req).await;
    assert!(
        status == 303 || status == 302,
        "expected redirect, got {status}"
    );
    assert_eq!(location, "/accounts");
    let rows = hub.openai.list();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].last4, "aaaa");
    assert_eq!(rows[0].label, "free");
    assert_eq!(rows[0].kind, "api_key");
    assert!(!format!("{rows:?}").contains("sk-test-aaaa"));
}

#[tokio::test]
async fn paste_grok_auth_json_adds_every_xai_session() {
    let hub = CredHub::new();
    let json = r#"{
      "https://auth.x.ai::a": {"key":"sk-test-aaaa","oidc_client_id":"a"},
      "https://auth.x.ai::b": {"key":"sk-test-bbbb","oidc_client_id":"b"}
    }"#;
    for tok in panday_sdk::oauth::grok_cli_all_from_json(json) {
        hub.add_secret("xai", "oauth", "pasted", &tok.access)
            .unwrap();
    }
    assert_eq!(hub.xai.list().len(), 2);
    let rows = hub.accounts();
    assert!(rows.iter().any(|r| r.last4 == "aaaa"));
    assert!(rows.iter().any(|r| r.last4 == "bbbb"));
    assert!(rows.iter().all(|r| r.kind == "oauth"));
}

#[tokio::test]
async fn two_anthropic_keys_and_an_openai_key_coexist() {
    let hub = CredHub::new();
    hub.add_secret("anthropic", "api_key", "a", "sk-ant-aaaa")
        .unwrap();
    hub.add_secret("anthropic", "api_key", "b", "sk-ant-bbbb")
        .unwrap();
    hub.add_secret("openai", "api_key", "o", "sk-test-cccc")
        .unwrap();
    assert_eq!(hub.anthropic.list().len(), 2);
    assert_eq!(hub.openai.list().len(), 1);
    assert!(hub.contains_last4("anthropic", "aaaa"));
    assert!(hub.contains_last4("openai", "cccc"));
}

#[tokio::test]
async fn rotate_form_switches_policy() {
    let hub = CredHub::new();
    let addr = bind(hub.clone()).await;
    let body = "policy=round_robin";
    let req = format!(
        "POST /console/accounts/rotate HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _, _) = http(&addr, &req).await;
    assert!(status == 303 || status == 302, "{status}");
    assert_eq!(
        hub.rotate(),
        panday_gateway::adapters::pool::Rotate::RoundRobin
    );
}

#[tokio::test]
async fn snapshot_json_on_an_empty_hub_has_no_secret_shaped_values() {
    let addr = bind(CredHub::new()).await;
    let (_, _, body) = http(
        &addr,
        &format!("GET /console/snapshot HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(
        body.contains("\"accounts\":[]") || body.contains("accounts"),
        "{body}"
    );
    assert!(!body.contains("sk-"), "{body}");
}

#[test]
fn duplicate_last4_is_rejected() {
    let hub = CredHub::new();
    hub.add_secret("openai", "api_key", "a", "sk-test-aaaa")
        .unwrap();
    let err = hub
        .add_secret("openai", "api_key", "b", "sk-test-aaaa")
        .unwrap_err();
    assert!(err.contains("already"), "{err}");
}
