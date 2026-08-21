//! Anthropic Messages ingress (`POST /v1/messages`).
//!
//! Claude Code speaks this dialect (`ANTHROPIC_BASE_URL` + `/v1/messages`).
//! Mapped onto IR, then the same gateway path as Chat Completions.

use crate::ingress::{authenticate, qualify_model, IngressState};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    ToolDef,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<IncomingMessage>,
    #[serde(default)]
    pub system: Option<SystemField>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    #[serde(default)]
    pub tools: Vec<IncomingTool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SystemField {
    Text(String),
    Blocks(Vec<IncomingBlock>),
}

#[derive(Debug, Deserialize)]
pub struct IncomingMessage {
    pub role: String,
    pub content: ContentField,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ContentField {
    Text(String),
    Blocks(Vec<IncomingBlock>),
}

#[derive(Debug, Deserialize)]
pub struct IncomingBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub content: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct IncomingTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

impl MessagesRequest {
    pub fn into_ir(self, account: AccountId) -> ChatRequest {
        let mut messages = Vec::new();
        if let Some(system) = self.system {
            let text = match system {
                SystemField::Text(t) => t,
                SystemField::Blocks(blocks) => flatten_blocks(&blocks),
            };
            if !text.is_empty() {
                messages.push(Message {
                    role: Role::System,
                    content: vec![ContentBlock::Text { text }],
                    call_id: None,
                    provider_call_id: None,
                });
            }
        }
        for m in self.messages {
            messages.extend(ir_messages(m));
        }
        ChatRequest {
            model: ModelRef(qualify_model(&self.model, "anthropic")),
            messages,
            tools: self
                .tools
                .into_iter()
                .map(|t| ToolDef {
                    name: t.name,
                    description: t.description,
                    parameters: t.input_schema,
                })
                .collect(),
            sampling: Sampling {
                temperature: self.temperature,
                top_p: self.top_p,
                max_tokens: Some(self.max_tokens),
                stop: self.stop_sequences,
            },
            cache: Default::default(),
            stream: self.stream,
            metadata: CallMeta {
                account,
                request: RequestId::new(),
                session: None,
                turn: None,
                task: None,
            },
        }
    }
}

fn flatten_blocks(blocks: &[IncomingBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        let piece = if b.kind == "text" {
            b.text.as_str()
        } else if let Some(Value::String(s)) = &b.content {
            s.as_str()
        } else {
            ""
        };
        if piece.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(piece);
    }
    out
}

fn ir_messages(m: IncomingMessage) -> Vec<Message> {
    match m.content {
        ContentField::Text(text) => vec![Message {
            role: if m.role == "assistant" {
                Role::Assistant
            } else {
                Role::User
            },
            content: vec![ContentBlock::Text { text }],
            call_id: None,
            provider_call_id: None,
        }],
        ContentField::Blocks(blocks) => {
            let mut out = Vec::new();
            let mut text_parts = Vec::new();
            let role = if m.role == "assistant" {
                Role::Assistant
            } else {
                Role::User
            };
            for b in blocks {
                match b.kind.as_str() {
                    "tool_result" => {
                        if !text_parts.is_empty() {
                            out.push(Message {
                                role,
                                content: vec![ContentBlock::Text {
                                    text: text_parts.join("\n"),
                                }],
                                call_id: None,
                                provider_call_id: None,
                            });
                            text_parts.clear();
                        }
                        let text = match &b.content {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => b.text.clone(),
                        };
                        out.push(Message {
                            role: Role::Tool,
                            content: vec![ContentBlock::Text { text }],
                            call_id: None,
                            provider_call_id: b.tool_use_id,
                        });
                    }
                    "tool_use" => {
                        let summary = format!(
                            "[tool_use {} {}]",
                            b.name.as_deref().unwrap_or("tool"),
                            b.id.as_deref().unwrap_or("")
                        );
                        text_parts.push(summary);
                    }
                    _ => {
                        if !b.text.is_empty() {
                            text_parts.push(b.text);
                        }
                    }
                }
            }
            if !text_parts.is_empty() {
                out.push(Message {
                    role,
                    content: vec![ContentBlock::Text {
                        text: text_parts.join("\n"),
                    }],
                    call_id: None,
                    provider_call_id: None,
                });
            }
            out
        }
    }
}

pub async fn create(
    State(state): State<IngressState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<MessagesRequest>,
) -> Response {
    let caller = match authenticate(&state, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Some(limiter) = &state.rate_limit {
        if let Err(e) = limiter.check(&caller.key_id) {
            return anthropic_error(e);
        }
    }
    let streaming = req.stream;
    let model_label = qualify_model(&req.model, "anthropic");
    let ir = req.into_ir(caller.account);
    let stream = match state.gateway.chat(ir).await {
        Ok(s) => s,
        Err(e) => return anthropic_error(e),
    };
    if streaming {
        stream_anthropic(stream, model_label).await
    } else {
        buffer_anthropic(stream, model_label).await
    }
}

pub(crate) fn anthropic_error(e: PandayError) -> Response {
    // Status comes from the shared table (docs/11 M11.8) — it is protocol, not
    // dialect. Only the envelope and the `type` vocabulary below are Anthropic's.
    let status = crate::ingress::status_for(&e);
    let ty = match &e {
        PandayError::RateLimited { .. } => "rate_limit_error",
        PandayError::Protocol(_) => "invalid_request_error",
        // Anthropic's own vocabulary, so a client written against the real API
        // branches the same way here. `overloaded_error` is what they send when
        // capacity is the problem, which is what an exhausted chain is.
        PandayError::PermissionDenied(_) => "permission_error",
        PandayError::ModelUnavailable { .. } => "overloaded_error",
        _ => "api_error",
    };
    (
        status,
        // Anthropic's own API sends `retry-after` on a 429, and this is the
        // route Claude Code talks to — dropping it here is the one place it
        // would be felt most (docs/11 M11.7).
        crate::ingress::retry_after_headers(&e),
        Json(json!({
            "type": "error",
            "error": { "type": ty, "message": e.to_string() }
        })),
    )
        .into_response()
}

fn stop_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::ToolUse => "tool_use",
        StopReason::StopSequence => "stop_sequence",
        _ => "end_turn",
    }
}

async fn buffer_anthropic(stream: ItemStream, model: String) -> Response {
    use futures_util::StreamExt;
    let mut text = String::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut reason = StopReason::EndTurn;
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamItem::Delta { text: t }) => text.push_str(&t),
            Ok(StreamItem::Usage { usage }) => {
                input_tokens = usage.input_tokens;
                output_tokens = usage.output_tokens;
            }
            Ok(StreamItem::Done { reason: r }) => reason = r,
            Ok(_) => {}
            Err(e) => return anthropic_error(e),
        }
    }
    let model_name = model
        .split_once('/')
        .map(|(_, n)| n)
        .unwrap_or(model.as_str());
    Json(json!({
        "id": format!("msg_{}", RequestId::new().0.simple()),
        "type": "message",
        "role": "assistant",
        "model": model_name,
        "content": [{"type": "text", "text": text}],
        "stop_reason": stop_reason(reason),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        }
    }))
    .into_response()
}

async fn stream_anthropic(stream: ItemStream, model: String) -> Response {
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let id = format!("msg_{}", RequestId::new().0.simple());
    let model_name = model
        .split_once('/')
        .map(|(_, n)| n.to_string())
        .unwrap_or(model);
    let text_open = Arc::new(AtomicBool::new(false));
    let start = event(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": model_name,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        }),
    );
    let rest = stream.flat_map(move |item| {
        let text_open = Arc::clone(&text_open);
        let mut frames: Vec<Result<String, std::convert::Infallible>> = Vec::new();
        match item {
            Ok(StreamItem::Delta { text }) => {
                if !text_open.swap(true, Ordering::SeqCst) {
                    frames.push(Ok(event(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": 0,
                            "content_block": {"type": "text", "text": ""}
                        }),
                    )));
                }
                frames.push(Ok(event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                )));
            }
            Ok(StreamItem::Done { reason }) => {
                if text_open.load(Ordering::SeqCst) {
                    frames.push(Ok(event(
                        "content_block_stop",
                        &json!({"type": "content_block_stop", "index": 0}),
                    )));
                }
                frames.push(Ok(event(
                    "message_delta",
                    &json!({
                        "type": "message_delta",
                        "delta": {
                            "stop_reason": stop_reason(reason),
                            "stop_sequence": Value::Null
                        },
                        "usage": {"output_tokens": 0}
                    }),
                )));
                frames.push(Ok(event("message_stop", &json!({"type": "message_stop"}))));
            }
            Ok(_) => {}
            Err(e) => frames.push(Ok(event(
                "error",
                &json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": e.to_string()}
                }),
            ))),
        }
        futures_util::stream::iter(frames)
    });
    let body = futures_util::stream::once(async move { Ok::<_, std::convert::Infallible>(start) })
        .chain(rest);
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from_stream(body))
        .expect("static builder")
}

fn event(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

#[cfg(test)]
mod tests {
    use super::anthropic_error;
    use panday_sdk::PandayError;

    #[test]
    fn a_rate_limit_tells_claude_code_when_to_come_back() {
        // This is the route Claude Code talks to (ANTHROPIC_BASE_URL), and
        // Anthropic's own API sends `retry-after` on a 429 — a client that
        // handles the real API must get the same signal here (docs/11 M11.7).
        let resp = anthropic_error(PandayError::RateLimited {
            retry_after_ms: 45_000,
        });
        assert_eq!(resp.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::RETRY_AFTER)
                .map(|v| v.to_str().unwrap()),
            Some("45")
        );
    }

    #[test]
    fn an_unknown_wait_sends_no_header() {
        let resp = anthropic_error(PandayError::RateLimited { retry_after_ms: 0 });
        assert_eq!(resp.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert!(resp
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .is_none());
    }
}
