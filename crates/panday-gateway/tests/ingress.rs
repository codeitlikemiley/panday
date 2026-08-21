//! M11.5 — the OpenAI-compatible ingress.
//!
//! docs/11 calls this "the platform's cheapest adoption wedge and its best A/B
//! harness (compare us vs direct)". The A/B property is why the surface must be
//! *exactly* the standard dialect: a client has to be redirectable by changing
//! a base URL and nothing else, or the comparison is not like-for-like.
//!
//! So these tests speak raw HTTP rather than using a typed client — that is
//! what aider, continue.dev and a curl script actually do, and it is the only
//! way to catch a response that our own types would happily round-trip but a
//! real parser would reject.

use panday_gateway::{AdapterCaps, Gateway, IngressState, ProviderAdapter};
use panday_router::PolicyRouter;
use panday_sdk::providers::RemoteModel;
use panday_sdk::{ItemStream, PandayError};
use panday_types::id::AccountId;
use panday_types::model::{ChatRequest, StopReason, StreamItem, Usage};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DEV_POLICY: &str = include_str!("../../panday-router/policy/dev.yaml");

struct Echo {
    reply: &'static str,
    fail: Option<fn() -> PandayError>,
}

#[async_trait::async_trait]
impl ProviderAdapter for Echo {
    fn name(&self) -> &'static str {
        "openai_compat"
    }
    fn capabilities(&self, _m: &str) -> AdapterCaps {
        AdapterCaps::default()
    }
    async fn list_models(&self) -> Result<Vec<RemoteModel>, PandayError> {
        Ok(vec![RemoteModel {
            id: "qwen3.5-4b".into(),
            context: Some(16_000),
            display_name: None,
        }])
    }

    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        if let Some(f) = self.fail {
            return Err(f());
        }
        let items: Vec<Result<StreamItem, PandayError>> = vec![
            Ok(StreamItem::Delta {
                text: self.reply.into(),
            }),
            Ok(StreamItem::Usage {
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 4,
                    cache_read_tokens: 8,
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

async fn serve(adapter: Arc<dyn ProviderAdapter>) -> String {
    let gateway = Gateway::builder(Arc::new(PolicyRouter::from_yaml(DEV_POLICY).unwrap()))
        .adapter("local", adapter)
        .build();

    let app =
        panday_gateway::ingress::router(IngressState::open(Arc::new(gateway), AccountId::new()));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// Raw HTTP POST — no client library, because a client library would paper over
/// exactly the wire mistakes this test exists to catch.
async fn post(addr: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Content-Type: application/json\r\n\
         Authorization: Bearer pnd_live_test\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
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
    let (head, raw_body) = match text.find("\r\n\r\n") {
        Some(p) => (&text[..p], text[p + 4..].to_string()),
        None => (text.as_str(), String::new()),
    };

    // Streaming responses arrive chunked; a real client decodes them, and a
    // test that does not would assert against chunk-size lines.
    let body = if head.to_lowercase().contains("transfer-encoding: chunked") {
        dechunk(&raw_body)
    } else {
        raw_body
    };
    (status, body)
}

/// Minimal `Transfer-Encoding: chunked` decoder.
fn dechunk(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(nl) = rest.find("\r\n") {
        let size = usize::from_str_radix(rest[..nl].trim(), 16).unwrap_or(0);
        rest = &rest[nl + 2..];
        if size == 0 || rest.len() < size {
            out.push_str(&rest[..rest.len().min(size)]);
            break;
        }
        out.push_str(&rest[..size]);
        rest = &rest[size..];
        if rest.starts_with("\r\n") {
            rest = &rest[2..];
        }
    }
    out
}

fn echo(reply: &'static str) -> Arc<dyn ProviderAdapter> {
    Arc::new(Echo { reply, fail: None })
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_non_streaming_request_returns_the_standard_envelope() {
    let addr = serve(echo("hello from panday")).await;
    let (status, body) = post(
        &addr,
        r#"{"model":"local/qwen3.5-4b","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;

    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

    // Every field a standard client reads.
    assert_eq!(v["object"], "chat.completion");
    assert!(v["id"].as_str().unwrap().starts_with("chatcmpl-"));
    assert_eq!(v["choices"][0]["message"]["role"], "assistant");
    assert_eq!(v["choices"][0]["message"]["content"], "hello from panday");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["prompt_tokens"], 12);
    assert_eq!(v["usage"]["completion_tokens"], 4);
    assert_eq!(v["usage"]["total_tokens"], 16);
}

#[tokio::test]
async fn a_streaming_request_emits_chunks_and_the_done_sentinel() {
    let addr = serve(echo("streamed")).await;
    let (status, body) = post(
        &addr,
        r#"{"model":"local/qwen3.5-4b","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;

    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("\"object\":\"chat.completion.chunk\""),
        "{body}"
    );
    assert!(body.contains("\"content\":\"streamed\""), "{body}");
    assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
    assert!(
        body.trim_end().ends_with("data: [DONE]"),
        "every client in this family waits for the sentinel: {body}"
    );
}

#[tokio::test]
async fn streamed_usage_is_reported_in_the_dialects_own_shape() {
    // Our cache splits ride under `prompt_tokens_details`, which is where a
    // standard client already looks — inventing a top-level key would break
    // strict parsers.
    let addr = serve(echo("x")).await;
    let (_s, body) = post(
        &addr,
        r#"{"model":"local/m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;

    assert!(body.contains("\"prompt_tokens\":12"), "{body}");
    assert!(body.contains("\"cached_tokens\":8"), "{body}");
}

#[tokio::test]
async fn every_sse_frame_is_valid_json_a_strict_parser_would_accept() {
    // The failure mode this catches: a frame that our own serializer emits
    // happily and a real client rejects.
    let addr = serve(echo("abc")).await;
    let (_s, body) = post(
        &addr,
        r#"{"model":"local/m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;

    let mut frames = 0;
    for line in body.lines().filter(|l| l.starts_with("data: ")) {
        let payload = line.trim_start_matches("data: ");
        if payload == "[DONE]" {
            continue;
        }
        serde_json::from_str::<serde_json::Value>(payload)
            .unwrap_or_else(|e| panic!("frame is not valid JSON: {payload} ({e})"));
        frames += 1;
    }
    assert!(frames >= 2, "expected content and terminator frames");
}

#[tokio::test]
async fn a_system_message_is_carried_through() {
    let addr = serve(echo("ok")).await;
    let (status, _b) = post(
        &addr,
        r#"{"model":"local/m","messages":[
             {"role":"system","content":"be terse"},
             {"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn stop_accepts_both_a_string_and_an_array() {
    // Real clients send both shapes; accepting one would reject working tools.
    let addr = serve(echo("ok")).await;

    let (a, _) = post(
        &addr,
        r##"{"model":"local/m","stop":"END","messages":[{"role":"user","content":"hi"}]}"##,
    )
    .await;
    assert_eq!(a, 200);

    let (b, _) = post(
        &addr,
        r##"{"model":"local/m","stop":["END","STOP"],"messages":[{"role":"user","content":"hi"}]}"##,
    )
    .await;
    assert_eq!(b, 200);
}

#[tokio::test]
async fn an_assistant_message_without_content_is_accepted() {
    // Assistant turns that only carried tool calls have no `content`. Requiring
    // it would reject any transcript that used tools.
    let addr = serve(echo("ok")).await;
    let (status, body) = post(
        &addr,
        r#"{"model":"local/m","messages":[
             {"role":"user","content":"hi"},
             {"role":"assistant"},
             {"role":"user","content":"again"}]}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn errors_use_the_standard_error_envelope_and_a_sensible_status() {
    // A rate limit must survive an exhausted chain: collapsing it into
    // "model unavailable" would strip the one signal a client can act on.
    let addr = serve(Arc::new(Echo {
        reply: "",
        fail: Some(|| PandayError::RateLimited {
            retry_after_ms: 100,
        }),
    }))
    .await;

    let (status, body) = post(
        &addr,
        r#"{"model":"local/m","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;

    assert_eq!(status, 429, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    // A client that only understands OpenAI's shape must be able to read our
    // failures too.
    assert!(v["error"]["message"].is_string());
    assert_eq!(v["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn an_unroutable_model_is_service_unavailable_not_a_500() {
    let addr = serve(echo("x")).await;
    let (status, body) = post(
        &addr,
        r#"{"model":"nonexistent/model","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, 503, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v["error"]["message"].is_string());
}

#[tokio::test]
async fn malformed_json_is_rejected_before_reaching_a_provider() {
    let addr = serve(echo("x")).await;
    let (status, _body) = post(&addr, r#"{"model": "local/m", "messages": [ }"#).await;
    assert!(
        (400..500).contains(&status),
        "expected a 4xx for malformed input, got {status}"
    );
}

#[tokio::test]
async fn auto_lets_the_router_choose_which_is_the_point_of_the_wedge() {
    // A client sending `auto` inherits routing without knowing it exists.
    let addr = serve(echo("routed")).await;
    let (status, body) = post(
        &addr,
        r#"{"model":"auto","messages":[{"role":"user","content":"summarize this"}]}"#,
    )
    .await;

    // dev.yaml routes `summarize` under 8k to the cheap pool, whose first
    // reachable leg here is `local`.
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "routed");
}

#[tokio::test]
async fn get_v1_models_lists_what_the_adapter_reported() {
    let addr = serve(echo("x")).await;
    let mut stream = TcpStream::connect(&addr).await.unwrap();
    let request = format!(
        "GET /v1/models HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Authorization: Bearer pnd_live_test\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    assert_eq!(status, 200, "{text}");
    let v: serde_json::Value = serde_json::from_str(body.trim()).expect("valid JSON");
    assert_eq!(v["object"], "list");
    assert_eq!(v["data"][0]["id"], "local/qwen3.5-4b");
    assert_eq!(v["data"][0]["owned_by"], "local");
}
