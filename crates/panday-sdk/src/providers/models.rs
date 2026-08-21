//! What a provider reports it can serve (`GET /v1/models`).
//!
//! The shipped catalog is prices, measured context, and pool preference — not
//! the inventory. Inventory comes from the signed-in account.

use crate::PandayError;
use serde_json::Value;

/// One model the authenticated account can call, as the provider named it
/// (no `provider/` prefix — the adapter that listed it adds that).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteModel {
    pub id: String,
    pub context: Option<u32>,
    pub display_name: Option<String>,
}

/// Anthropic `GET /v1/models` page. `next_after` is `last_id` when `has_more`.
pub fn parse_anthropic_models(
    bytes: &[u8],
) -> Result<(Vec<RemoteModel>, Option<String>), PandayError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|e| PandayError::Protocol(format!("anthropic models: {e}")))?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
        for item in arr {
            let Some(id) = item.get("id").and_then(|x| x.as_str()) else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            out.push(RemoteModel {
                id: id.to_string(),
                context: positive_u32(item.get("max_input_tokens")),
                display_name: item
                    .get("display_name")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            });
        }
    }
    let next = match (
        v.get("has_more").and_then(|x| x.as_bool()).unwrap_or(false),
        v.get("last_id").and_then(|x| x.as_str()),
    ) {
        (true, Some(id)) if !id.is_empty() => Some(id.to_string()),
        _ => None,
    };
    Ok((out, next))
}

/// OpenAI-compatible `GET /v1/models` (`data` or llama-server `models`).
pub fn parse_openai_models(bytes: &[u8]) -> Result<Vec<RemoteModel>, PandayError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|e| PandayError::Protocol(format!("openai_compat models: {e}")))?;
    let arr = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for item in arr {
        let id = item
            .get("id")
            .or_else(|| item.get("name"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        if id.is_empty() || !looks_like_chat(id) {
            continue;
        }
        let context = positive_u32(item.get("context_window"))
            .or_else(|| positive_u32(item.get("max_model_len")))
            .or_else(|| positive_u32(item.get("max_input_tokens")));
        out.push(RemoteModel {
            id: id.to_string(),
            context,
            display_name: None,
        });
    }
    Ok(out)
}

/// Drop embeddings, TTS, image, and similar — `/v1/models` on OpenAI-shaped
/// APIs lists the whole product surface, not just chat.
pub fn looks_like_chat(id: &str) -> bool {
    let l = id.to_ascii_lowercase();
    const SKIP: &[&str] = &[
        "embed",
        "whisper",
        "tts",
        "dall-e",
        "dall_e",
        "moderation",
        "transcribe",
        "realtime",
        "babbage",
        "davinci",
        "sora-",
        "rerank",
        "-image",
        "image-",
        "audio",
        "video",
        "imagine",
    ];
    !SKIP.iter().any(|s| l.contains(s))
}

fn positive_u32(v: Option<&Value>) -> Option<u32> {
    let n = v.and_then(|x| x.as_u64()).or_else(|| {
        v.and_then(|x| x.as_f64())
            .filter(|f| *f > 0.0 && f.is_finite())
            .map(|f| f as u64)
    })?;
    if n == 0 || n > u32::MAX as u64 {
        None
    } else {
        Some(n as u32)
    }
}

/// `{base}/v1/models`, or `{base}/models` when `base` already ends in `/v1`.
pub fn models_url(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_page_extracts_ids_and_cursor() {
        let (models, next) = parse_anthropic_models(
            br#"{
              "data": [
                {"id":"claude-fable-5","display_name":"Claude Fable 5","max_input_tokens":1000000,"type":"model"},
                {"id":"claude-opus-5","display_name":"Claude Opus 5","max_input_tokens":0,"type":"model"}
              ],
              "has_more": true,
              "last_id": "claude-opus-5"
            }"#,
        )
        .unwrap();
        assert_eq!(models[0].id, "claude-fable-5");
        assert_eq!(models[0].context, Some(1_000_000));
        assert_eq!(models[0].display_name.as_deref(), Some("Claude Fable 5"));
        assert_eq!(models[1].context, None, "0 is not a real window");
        assert_eq!(next.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn openai_list_drops_whisper_and_keeps_gpt() {
        let models = parse_openai_models(
            br#"{
              "object":"list",
              "data":[
                {"id":"gpt-5.6-sol","object":"model"},
                {"id":"whisper-1","object":"model"},
                {"id":"text-embedding-3-large","object":"model"},
                {"id":"grok-imagine-video","object":"model"},
                {"id":"grok-4.6","object":"model"}
              ]
            }"#,
        )
        .unwrap();
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-5.6-sol", "grok-4.6"]);
        assert!(!ids
            .iter()
            .any(|id| id.contains("imagine") || id.contains("video")));
    }

    #[test]
    fn llama_server_models_wrapper_is_accepted() {
        let models = parse_openai_models(br#"{"models":[{"name":"qwen3.5-4b"}]}"#).unwrap();
        assert_eq!(models[0].id, "qwen3.5-4b");
    }

    #[test]
    fn models_url_does_not_double_v1() {
        assert_eq!(
            models_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(models_url("https://api.x.ai"), "https://api.x.ai/v1/models");
        assert_eq!(
            models_url("http://127.0.0.1:8081/"),
            "http://127.0.0.1:8081/v1/models"
        );
    }
}
