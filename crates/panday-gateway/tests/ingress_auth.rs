//! M17.3 — the ingress refuses unauthenticated callers, and bounds authenticated ones.
//!
//! The wedge in M11.5 was "redirect a client by changing a base URL". A hosted deployment of that
//! wedge is an open proxy to somebody else's paid inference unless a key gates it, so these tests
//! speak raw HTTP for the same reason the M11.5 ones do: a real client sends an `Authorization`
//! header and nothing else, and the failure has to be readable by a parser that only knows OpenAI's
//! error envelope.

use panday_gateway::ingress::{AuthError, Authenticator, Caller, IngressState, RateLimiter};
use panday_gateway::{AdapterCaps, Gateway, ProviderAdapter};
use panday_router::PolicyRouter;
use panday_sdk::{ItemStream, PandayError};
use panday_types::id::AccountId;
use panday_types::model::{ChatRequest, StopReason, StreamItem, Usage};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DEV_POLICY: &str = include_str!("../../panday-router/policy/dev.yaml");

struct Echo;

#[async_trait::async_trait]
impl ProviderAdapter for Echo {
    fn name(&self) -> &'static str {
        "local"
    }
    fn capabilities(&self, _m: &str) -> AdapterCaps {
        AdapterCaps::default()
    }
    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        let items: Vec<Result<StreamItem, PandayError>> = vec![
            Ok(StreamItem::Delta { text: "ok".into() }),
            Ok(StreamItem::Usage {
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 4,
                    ..Default::default()
                },
            }),
            Ok(StreamItem::Done {
                reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

/// One key, one account, one scope set — the smallest thing that is not `NoAuth`.
struct OneKey {
    token: &'static str,
    account: AccountId,
    scopes: Vec<String>,
}

#[async_trait::async_trait]
impl Authenticator for OneKey {
    async fn authenticate(&self, bearer: &str) -> Result<Caller, AuthError> {
        if bearer.is_empty() {
            return Err(AuthError::Missing);
        }
        if bearer != self.token {
            return Err(AuthError::Invalid);
        }
        Ok(Caller {
            account: self.account,
            key_id: "key_1".into(),
            scopes: self.scopes.clone(),
        })
    }
}

fn gateway() -> Arc<Gateway> {
    Arc::new(
        Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
            .adapter("local", Arc::new(Echo))
            .build(),
    )
}

async fn serve(state: IngressState) -> String {
    let app = panday_gateway::ingress::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

const BODY: &str = r#"{"model":"local/echo","messages":[{"role":"user","content":"hi"}]}"#;

/// `auth` is the raw header value, or `None` to send no `Authorization` header at all — which is
/// what a client that forgot the key does, and is a different code path from an empty one.
async fn post(addr: &str, auth: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let header = match auth {
        Some(v) => format!("Authorization: {v}\r\n"),
        None => String::new(),
    };
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Content-Type: application/json\r\n\
         {header}\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{BODY}",
        BODY.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = match text.find("\r\n\r\n") {
        Some(p) => text[p + 4..].to_string(),
        None => String::new(),
    };
    (status, body)
}

fn one_key(scopes: &[&str]) -> Arc<OneKey> {
    Arc::new(OneKey {
        token: "pnd_live_good",
        account: AccountId::new(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
    })
}

#[tokio::test]
async fn a_good_key_is_let_through() {
    let addr =
        serve(IngressState::open(gateway(), AccountId::new()).with_auth(one_key(&["models"])))
            .await;
    let (status, body) = post(&addr, Some("Bearer pnd_live_good")).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn no_key_is_401_and_not_a_500() {
    let addr =
        serve(IngressState::open(gateway(), AccountId::new()).with_auth(one_key(&["models"])))
            .await;

    for auth in [
        None,
        Some("Bearer "),
        Some("Bearer pnd_live_wrong"),
        Some("pnd_live_good"),
    ] {
        let (status, body) = post(&addr, auth).await;
        assert_eq!(status, 401, "{auth:?} -> {body}");
        // Readable by a client that only knows the OpenAI envelope.
        let json: serde_json::Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(json["error"]["type"], "authentication_error");
        // Every refusal says the same thing. "Revoked" or "unknown key" would tell somebody holding
        // a token they found in a log whether it was ever real.
        assert_eq!(json["error"]["message"], "invalid API key");
    }
}

#[tokio::test]
async fn a_key_without_the_models_scope_cannot_call_the_model_plane() {
    // A sessions-only key is a real thing to hand out — a dashboard that reads transcripts should
    // not be able to spend inference money with the same secret.
    let addr =
        serve(IngressState::open(gateway(), AccountId::new()).with_auth(one_key(&["sessions"])))
            .await;
    let (status, body) = post(&addr, Some("Bearer pnd_live_good")).await;
    assert_eq!(status, 401, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["error"]["message"], "this key does not carry the `models` scope",
        "a scope failure is worth naming: the holder already proved they have the key"
    );
}

#[tokio::test]
async fn the_default_ingress_still_needs_no_key() {
    // `panday local` and every M11.5 test wire this. An ingress that demanded a key before accounts
    // exist would make the offline tier impossible (ADR-011).
    let addr = serve(IngressState::open(gateway(), AccountId::new())).await;
    let (status, body) = post(&addr, None).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn a_key_over_its_limit_gets_429_not_a_dropped_connection() {
    let addr = serve(
        IngressState::open(gateway(), AccountId::new())
            .with_auth(one_key(&["models"]))
            .with_rate_limit(Arc::new(RateLimiter::per_minute(2))),
    )
    .await;

    for i in 0..2 {
        let (status, body) = post(&addr, Some("Bearer pnd_live_good")).await;
        assert_eq!(status, 200, "request {i}: {body}");
    }
    let (status, body) = post(&addr, Some("Bearer pnd_live_good")).await;
    assert_eq!(status, 429, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn the_limit_is_checked_after_authentication() {
    // Otherwise an unauthenticated flood consumes the limit of whatever key it guesses at, which
    // turns the rate limiter into the denial-of-service it exists to prevent.
    let addr = serve(
        IngressState::open(gateway(), AccountId::new())
            .with_auth(one_key(&["models"]))
            .with_rate_limit(Arc::new(RateLimiter::per_minute(1))),
    )
    .await;

    for _ in 0..5 {
        assert_eq!(post(&addr, Some("Bearer pnd_live_wrong")).await.0, 401);
    }
    assert_eq!(post(&addr, Some("Bearer pnd_live_good")).await.0, 200);
}
