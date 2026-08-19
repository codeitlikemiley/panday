//! `json-bench` — schema-validity rates (docs/19 §19.1, M19.1).
//!
//! The question this suite answers is narrow and the most operationally useful one about a local
//! model: **when you ask for JSON matching a schema, how often do you get it?** docs/18's
//! `CapabilityProfile.json_reliability` is a number that has to come from somewhere, and until it
//! comes from here it is a guess (M19.2 is the milestone that closes that loop).
//!
//! Three failure modes, scored separately, because the fixes differ:
//!
//! - **Not JSON at all** — prose, or JSON wrapped in a markdown fence. A harness can strip a fence;
//!   it cannot rescue an apology. Counted apart so "the model chats at you" and "the model gets the
//!   shape wrong" are not one number.
//! - **JSON, wrong shape** — the interesting one: a missing required field, a string where a number
//!   belongs, an invented enum member.
//! - **Valid** — parses, and satisfies the schema.
//!
//! The validator is a subset, hand-written, and the subset is exactly what the corpus uses. A full
//! JSON Schema implementation is a dependency and a spec-compliance project; what a bench needs is
//! a checker whose failure messages a person can act on.

use crate::json_schema::{validate, SchemaError};
use panday_sdk::{ModelClient, PandayError};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StreamItem,
};
use panday_types::scorecard::Scorecard;
use serde_json::Value;

/// One ask: a schema, and a prompt that should produce something matching it.
#[derive(Debug, Clone)]
pub struct Case {
    pub name: String,
    /// Named separately from the case so a scorecard can group by shape as well as by phrasing.
    pub shape: String,
    pub schema: Value,
    pub prompt: String,
}

/// What one response was.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Valid,
    /// Parsed as JSON, but does not satisfy the schema.
    WrongShape(String),
    /// Not JSON, even after unwrapping a markdown fence.
    NotJson,
    /// The call itself failed. Not the model's fault, and not scored as a model failure.
    CallFailed(String),
}

/// Strip a markdown fence, if the model wrapped its answer in one.
///
/// Not leniency for its own sake: a real harness does exactly this before parsing, so a bench that
/// refused would measure a strictness the product does not have. What it does *not* do is hunt for
/// a JSON object inside prose — that would be measuring our salvage code rather than the model.
pub fn unfence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    // ```json\n{...}\n```  → drop the language tag and the closing fence.
    let after_tag = rest.split_once('\n').map(|(_, rest)| rest).unwrap_or(rest);
    after_tag
        .rsplit_once("```")
        .map(|(body, _)| body.trim())
        .unwrap_or(after_tag.trim())
}

/// Score one response against one case.
pub fn judge(case: &Case, response: &str) -> Outcome {
    let text = unfence(response);
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Outcome::NotJson;
    };
    match validate(&case.schema, &value) {
        Ok(()) => Outcome::Valid,
        Err(SchemaError(detail)) => Outcome::WrongShape(detail),
    }
}

/// Run the suite against any model client — the gateway, in production, so the eval exercises
/// routing and adapters too (docs/19: "three birds").
pub async fn run(
    client: &dyn ModelClient,
    model: &ModelRef,
    subject: &str,
    at: &str,
    cases: &[Case],
) -> Scorecard {
    let mut card = Scorecard::new("json-bench", subject, at);
    let mut not_json = 0u32;
    let mut wrong_shape = 0u32;
    let mut call_failed = 0u32;

    for case in cases {
        let outcome = match ask(client, model, case).await {
            Ok(text) => judge(case, &text),
            Err(e) => Outcome::CallFailed(e.to_string()),
        };
        match outcome {
            Outcome::Valid => card.record(&case.name, true, ""),
            Outcome::WrongShape(detail) => {
                wrong_shape += 1;
                card.record(&case.name, false, format!("wrong shape: {detail}"));
            }
            Outcome::NotJson => {
                not_json += 1;
                card.record(&case.name, false, "not JSON");
            }
            Outcome::CallFailed(detail) => {
                // Counted, and scored as a failure of the *run* rather than silently dropped: a
                // suite that skips its errors reports a rate for the cases that happened to work.
                call_failed += 1;
                card.record(&case.name, false, format!("call failed: {detail}"));
            }
        }
    }

    let total = cases.len().max(1) as f64;
    card.metric("schema_validity", card.rate());
    card.metric("not_json_rate", not_json as f64 / total);
    card.metric("wrong_shape_rate", wrong_shape as f64 / total);
    card.metric("call_failure_rate", call_failed as f64 / total);
    card
}

async fn ask(
    client: &dyn ModelClient,
    model: &ModelRef,
    case: &Case,
) -> Result<String, PandayError> {
    use futures_util::StreamExt;

    // The schema goes in the prompt, because that is what a caller without provider-side structured
    // output has to do — and the local tier is exactly that caller (docs/18).
    let instruction = format!(
        "{}\n\nReply with JSON only, matching this schema:\n{}",
        case.prompt,
        serde_json::to_string(&case.schema).unwrap_or_default()
    );

    let request = ChatRequest {
        model: model.clone(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: instruction }],
            call_id: None,
            provider_call_id: None,
        }],
        tools: vec![],
        // Deterministic as the provider allows: a bench whose score moves on re-run measures
        // sampling noise as well as the model.
        sampling: Sampling {
            temperature: Some(0.0),
            ..Default::default()
        },
        cache: Default::default(),
        stream: true,
        metadata: CallMeta {
            account: panday_types::id::AccountId::new(),
            request: panday_types::id::RequestId::new(),
            session: None,
            turn: None,
            task: None,
        },
    };

    let mut stream = client.chat(request).await?;
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let StreamItem::Delta { text: delta } = item? {
            text.push_str(&delta);
        }
    }
    Ok(text)
}

/// The corpus.
///
/// **How the count is reached, said out loud:** ten shapes × twenty phrasings. The shapes are the
/// JSON structures an agent actually asks for — a tool call, a diff plan, a triage verdict — and
/// the phrasings are the ways a caller actually asks. Both dimensions matter and neither alone is
/// the test: a model that handles nested objects but only when asked politely is not usable, and
/// docs/19's target of 200 fixtures is met by the product rather than by writing two hundred
/// near-duplicates by hand. Mined transcripts (M19.4) will replace the phrasing dimension with
/// real ones.
pub fn corpus() -> Vec<Case> {
    let mut cases = Vec::new();
    for (shape, schema, subject) in shapes() {
        for (i, phrasing) in phrasings().iter().enumerate() {
            cases.push(Case {
                name: format!("{shape}/{i:02}"),
                shape: shape.to_string(),
                schema: schema.clone(),
                prompt: phrasing.replace("{subject}", subject),
            });
        }
    }
    cases
}

/// The structures, from easy to genuinely hard for a 4B model.
fn shapes() -> Vec<(&'static str, Value, &'static str)> {
    use serde_json::json;
    vec![
        (
            "flat-strings",
            json!({"type": "object", "required": ["name", "language"], "properties": {
                "name": {"type": "string"}, "language": {"type": "string"}
            }}),
            "the crate `panday-gateway` and the language it is written in",
        ),
        (
            "numbers",
            json!({"type": "object", "required": ["files", "lines"], "properties": {
                "files": {"type": "integer"}, "lines": {"type": "integer"}
            }}),
            "a count of 3 files and 128 lines",
        ),
        (
            "booleans",
            json!({"type": "object", "required": ["passes", "flaky"], "properties": {
                "passes": {"type": "boolean"}, "flaky": {"type": "boolean"}
            }}),
            "a test that passes and is not flaky",
        ),
        (
            "enum",
            json!({"type": "object", "required": ["severity"], "properties": {
                "severity": {"enum": ["low", "medium", "high"]}
            }}),
            "a bug of medium severity",
        ),
        (
            "array-of-strings",
            json!({"type": "object", "required": ["files"], "properties": {
                "files": {"type": "array", "items": {"type": "string"}}
            }}),
            "the files src/lib.rs and src/main.rs",
        ),
        (
            "nested-object",
            json!({"type": "object", "required": ["tool", "args"], "properties": {
                "tool": {"type": "string"},
                "args": {"type": "object", "required": ["path"], "properties": {
                    "path": {"type": "string"}, "limit": {"type": "integer"}
                }}
            }}),
            "a call to the `read` tool for src/lib.rs limited to 50 lines",
        ),
        (
            "array-of-objects",
            json!({"type": "object", "required": ["edits"], "properties": {
                "edits": {"type": "array", "items": {
                    "type": "object", "required": ["path", "line"], "properties": {
                        "path": {"type": "string"}, "line": {"type": "integer"}
                    }
                }}
            }}),
            "two edits: src/lib.rs line 12 and src/main.rs line 40",
        ),
        (
            "optional-fields",
            json!({"type": "object", "required": ["verdict"], "properties": {
                "verdict": {"enum": ["fix", "wontfix", "duplicate"]},
                "duplicate_of": {"type": "string"}
            }}),
            "a verdict of wontfix, with no duplicate",
        ),
        (
            "mixed-types",
            json!({"type": "object", "required": ["model", "cost_micros", "cached"], "properties": {
                "model": {"type": "string"},
                "cost_micros": {"type": "integer"},
                "cached": {"type": "boolean"}
            }}),
            "a call to local/qwen3.5-4b costing 0 micros, served from cache",
        ),
        (
            "deep-nesting",
            json!({"type": "object", "required": ["session"], "properties": {
                "session": {"type": "object", "required": ["turn"], "properties": {
                    "turn": {"type": "object", "required": ["index", "tools"], "properties": {
                        "index": {"type": "integer"},
                        "tools": {"type": "array", "items": {"type": "string"}}
                    }}
                }}
            }}),
            "turn 2 of a session that used the `read` and `bash` tools",
        ),
    ]
}

/// Twenty ways a caller asks. Terse, polite, adversarial ("explain your reasoning" is the one that
/// most reliably breaks JSON discipline), and with distractors.
fn phrasings() -> Vec<&'static str> {
    vec![
        "Describe {subject}.",
        "Give me {subject}.",
        "Please describe {subject}.",
        "{subject} — as JSON.",
        "I need {subject}.",
        "Return {subject}.",
        "Can you describe {subject}?",
        "Describe {subject}. Be brief.",
        "Describe {subject}. Be thorough.",
        "Describe {subject} and explain your reasoning.",
        "First think, then describe {subject}.",
        "Describe {subject}. No prose.",
        "Describe {subject}. Do not wrap the answer in a code fence.",
        "Describe {subject}. Output must be machine-readable.",
        "As a JSON API would: {subject}.",
        "Hey! Quick one — {subject}?",
        "URGENT: {subject}.",
        "Describe {subject}. If you are unsure, guess.",
        "Describe {subject}. Include nothing else.",
        "describe {subject} (lowercase, no formatting)",
    ]
}
