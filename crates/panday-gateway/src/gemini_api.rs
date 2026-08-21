//! Gemini / Antigravity ingress.
//!
//! Antigravity CLI (`agy`) is Gemini-compatible via `GOOGLE_GEMINI_BASE_URL`.
//! It posts to `/v1beta/models/{model}:generateContent` (and
//! `:streamGenerateContent`). Not OpenAI Chat Completions.

use crate::ingress::{authenticate, qualify_model, IngressState};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct GenerateRequest {
    #[serde(default)]
    pub contents: Vec<Content>,
    #[serde(default, rename = "systemInstruction")]
    pub system_instruction: Option<Content>,
    #[serde(default, rename = "generationConfig")]
    pub generation_config: Option<GenerationConfig>,
}

#[derive(Debug, Deserialize)]
pub struct Content {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Deserialize)]
pub struct Part {
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct GenerationConfig {
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default, rename = "maxOutputTokens")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, rename = "topP")]
    pub top_p: Option<f32>,
    #[serde(default, rename = "stopSequences")]
    pub stop_sequences: Vec<String>,
}

impl GenerateRequest {
    fn into_ir(self, account: AccountId, model: String) -> ChatRequest {
        let mut messages = Vec::new();
        if let Some(sys) = self.system_instruction {
            let text = join_parts(&sys.parts);
            if !text.is_empty() {
                messages.push(Message {
                    role: Role::System,
                    content: vec![ContentBlock::Text { text }],
                    call_id: None,
                    provider_call_id: None,
                });
            }
        }
        for c in self.contents {
            let role = match c.role.as_deref() {
                Some("model") | Some("assistant") => Role::Assistant,
                _ => Role::User,
            };
            let text = join_parts(&c.parts);
            messages.push(Message {
                role,
                content: vec![ContentBlock::Text { text }],
                call_id: None,
                provider_call_id: None,
            });
        }
        let cfg = self.generation_config.unwrap_or_default();
        ChatRequest {
            model: ModelRef(model),
            messages,
            tools: Vec::new(),
            sampling: Sampling {
                temperature: cfg.temperature,
                top_p: cfg.top_p,
                max_tokens: cfg.max_output_tokens,
                // agy sends stopSequences; Grok rejects `stop`. Drop them here.
                stop: Vec::new(),
            },
            cache: Default::default(),
            stream: false,
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

fn join_parts(parts: &[Part]) -> String {
    parts
        .iter()
        .map(|p| p.text.as_str())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `gemini-2.5-flash:generateContent` or `xai/grok-4.6:streamGenerateContent`.
fn parse_tail(tail: &str) -> (String, bool) {
    let (model, verb) = tail.rsplit_once(':').unwrap_or((tail, "generateContent"));
    let stream = verb.contains("stream");
    (qualify_model(model, "gemini"), stream)
}

pub async fn list_models(
    State(state): State<IngressState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Err(r) = authenticate(&state, &headers).await {
        return r;
    }
    let models = state.gateway.live_models().await;
    Json(json!({
        "models": models.iter().map(|m| {
            let name = m.id.split_once('/').map(|(_, n)| n).unwrap_or(&m.id);
            json!({
                "name": format!("models/{name}"),
                "displayName": m.display_name.as_deref().unwrap_or(&m.id),
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
            })
        }).collect::<Vec<_>>(),
    }))
    .into_response()
}

pub async fn generate(
    State(state): State<IngressState>,
    headers: axum::http::HeaderMap,
    Path(tail): Path<String>,
    Json(req): Json<GenerateRequest>,
) -> Response {
    let caller = match authenticate(&state, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if let Some(limiter) = &state.rate_limit {
        if let Err(e) = limiter.check(&caller.key_id) {
            return gemini_error(e);
        }
    }
    let (mut model, stream) = parse_tail(&tail);
    // Antigravity names Gemini models. If this process has no Gemini adapter
    // (no GEMINI_API_KEY), route with `auto` so Grok/Claude OAuth still serve.
    if model.starts_with("gemini/") && !state.gateway.providers().contains(&"gemini") {
        model = "auto".into();
    }
    let mut ir = req.into_ir(caller.account, model.clone());
    ir.stream = stream;
    let items = match state.gateway.chat(ir).await {
        Ok(s) => s,
        Err(e) => return gemini_error(e),
    };
    if stream {
        stream_gemini(items).await
    } else {
        buffer_gemini(items).await
    }
}

fn gemini_error(e: PandayError) -> Response {
    let status = match &e {
        PandayError::RateLimited { .. } => axum::http::StatusCode::TOO_MANY_REQUESTS,
        PandayError::Protocol(_) => axum::http::StatusCode::BAD_REQUEST,
        PandayError::ModelUnavailable { .. } => axum::http::StatusCode::NOT_FOUND,
        _ => axum::http::StatusCode::BAD_GATEWAY,
    };
    (
        status,
        Json(json!({
            "error": {
                "code": status.as_u16(),
                "message": e.to_string(),
                "status": status.canonical_reason().unwrap_or("UNKNOWN"),
            }
        })),
    )
        .into_response()
}

fn finish_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::MaxTokens => "MAX_TOKENS",
        StopReason::StopSequence => "STOP",
        _ => "STOP",
    }
}

async fn buffer_gemini(stream: ItemStream) -> Response {
    use futures_util::StreamExt;
    let mut text = String::new();
    let mut prompt = 0u64;
    let mut completion = 0u64;
    let mut reason = StopReason::EndTurn;
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamItem::Delta { text: t }) => text.push_str(&t),
            Ok(StreamItem::Usage { usage }) => {
                prompt = usage.input_tokens;
                completion = usage.output_tokens;
            }
            Ok(StreamItem::Done { reason: r }) => reason = r,
            Ok(_) => {}
            Err(e) => return gemini_error(e),
        }
    }
    Json(json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": text}]},
            "finishReason": finish_reason(reason),
        }],
        "usageMetadata": {
            "promptTokenCount": prompt,
            "candidatesTokenCount": completion,
            "totalTokenCount": prompt + completion,
        }
    }))
    .into_response()
}

async fn stream_gemini(stream: ItemStream) -> Response {
    use futures_util::StreamExt;
    let body = stream.flat_map(|item| {
        let frames: Vec<Result<String, std::convert::Infallible>> = match item {
            Ok(StreamItem::Delta { text }) => vec![Ok(format!(
                "data: {}\n\n",
                json!({
                    "candidates": [{
                        "content": {"role": "model", "parts": [{"text": text}]},
                    }]
                })
            ))],
            Ok(StreamItem::Usage { usage }) => vec![Ok(format!(
                "data: {}\n\n",
                json!({
                    "usageMetadata": {
                        "promptTokenCount": usage.input_tokens,
                        "candidatesTokenCount": usage.output_tokens,
                        "totalTokenCount": usage.input_tokens + usage.output_tokens,
                    }
                })
            ))],
            Ok(StreamItem::Done { reason }) => vec![Ok(format!(
                "data: {}\n\n",
                json!({
                    "candidates": [{
                        "content": {"role": "model", "parts": [{"text": ""}]},
                        "finishReason": finish_reason(reason),
                    }]
                })
            ))],
            Ok(_) => vec![],
            Err(e) => vec![Ok(format!(
                "data: {}\n\n",
                json!({"error": {"message": e.to_string()}})
            ))],
        };
        futures_util::stream::iter(frames)
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from_stream(body))
        .expect("static builder")
}

#[cfg(test)]
mod tests {
    use super::parse_tail;

    #[test]
    fn generate_content_path_is_gemini_prefixed() {
        let (model, stream) = parse_tail("gemini-2.5-flash:generateContent");
        assert_eq!(model, "gemini/gemini-2.5-flash");
        assert!(!stream);
    }

    #[test]
    fn stream_verb_and_already_qualified_id() {
        let (model, stream) = parse_tail("xai/grok-4.6:streamGenerateContent");
        assert_eq!(model, "xai/grok-4.6");
        assert!(stream);
    }
}
