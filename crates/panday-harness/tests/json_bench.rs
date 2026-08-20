//! M19.1 — `json-bench` measures what it claims to, without needing a model.
//!
//! The suite's own correctness is testable offline: a fake client returning known-bad answers must
//! produce the scores those answers deserve. What needs a model is the *number*, not the method.

use panday_harness::json_bench::{self, Case, Outcome};
use panday_harness::json_schema::SUPPORTED_KEYWORDS;
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::model::{ChatRequest, ModelRef, StopReason, StreamItem};
use serde_json::Value;
use std::sync::Arc;

fn case(schema: Value) -> Case {
    Case {
        name: "t".into(),
        shape: "t".into(),
        schema,
        prompt: "describe it".into(),
    }
}

#[test]
fn a_fenced_answer_is_unwrapped_because_a_real_harness_unwraps_one() {
    assert_eq!(json_bench::unfence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
    assert_eq!(json_bench::unfence("```\n{\"a\":1}\n```"), "{\"a\":1}");
    assert_eq!(json_bench::unfence("  {\"a\":1}  "), "{\"a\":1}");
}

#[test]
fn prose_around_json_is_not_salvaged() {
    // Hunting for an object inside prose would measure our salvage code rather than the model —
    // and would quietly raise every score in a way no product behaviour matches.
    let c = case(serde_json::json!({"type": "object"}));
    assert_eq!(
        json_bench::judge(&c, "Sure! Here you go: {\"a\": 1}"),
        Outcome::NotJson
    );
}

#[test]
fn the_three_failure_modes_are_scored_apart() {
    // "The model chats at you" and "the model gets the shape wrong" need different fixes, so they
    // must not collapse into one number.
    let c = case(serde_json::json!({
        "type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}
    }));
    assert_eq!(
        json_bench::judge(&c, "I'd be happy to help!"),
        Outcome::NotJson
    );
    assert!(matches!(
        json_bench::judge(&c, "{\"other\": 1}"),
        Outcome::WrongShape(_)
    ));
    assert_eq!(
        json_bench::judge(&c, "{\"name\": \"panday\"}"),
        Outcome::Valid
    );
}

#[test]
fn the_corpus_reaches_its_target_and_says_how() {
    // docs/19 asks for 200 fixtures. Ten shapes × twenty phrasings, which is the product of two
    // dimensions that both matter — not two hundred near-duplicates.
    let corpus = json_bench::corpus();
    assert_eq!(corpus.len(), 200);

    let shapes: std::collections::BTreeSet<&str> =
        corpus.iter().map(|c| c.shape.as_str()).collect();
    assert_eq!(shapes.len(), 10);
    for shape in &shapes {
        assert_eq!(corpus.iter().filter(|c| &c.shape == shape).count(), 20);
    }
    // Every case is uniquely named, or a scorecard cannot say which one failed.
    let names: std::collections::BTreeSet<&str> = corpus.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names.len(), corpus.len());
}

#[test]
fn every_corpus_schema_uses_only_keywords_the_validator_checks() {
    // An ignored constraint is a case scored as passing. This is the assertion that keeps the
    // corpus inside what the validator actually enforces.
    fn keywords(schema: &Value, out: &mut Vec<String>) {
        if let Some(map) = schema.as_object() {
            for (key, child) in map {
                match key.as_str() {
                    "properties" => {
                        if let Some(props) = child.as_object() {
                            for sub in props.values() {
                                keywords(sub, out);
                            }
                        }
                    }
                    "items" => keywords(child, out),
                    other => out.push(other.to_string()),
                }
            }
        }
    }

    for case in json_bench::corpus() {
        let mut found = Vec::new();
        keywords(&case.schema, &mut found);
        for keyword in found {
            assert!(
                SUPPORTED_KEYWORDS.contains(&keyword.as_str()),
                "`{}` uses `{keyword}`, which the validator ignores — the case would pass for free",
                case.name
            );
        }
    }
}

#[test]
fn the_prompt_asks_for_the_schema_it_will_be_judged_against() {
    // A bench that judged against a schema the model was never shown would measure telepathy.
    for case in json_bench::corpus().iter().take(5) {
        assert!(!case.prompt.contains("{subject}"), "unsubstituted template");
        assert!(!case.prompt.is_empty());
    }
}

/// Replies with a fixed string, whatever it is asked.
struct Fixed(&'static str);

/// Records the request so we can assert sampling, then replies with JSON.
struct Records(std::sync::Mutex<Option<ChatRequest>>);

#[async_trait::async_trait]
impl ModelClient for Records {
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        *self.0.lock().unwrap() = Some(req);
        let items: Vec<Result<StreamItem, PandayError>> = vec![
            Ok(StreamItem::Delta {
                text: r#"{"name":"panday-gateway","language":"Rust"}"#.into(),
            }),
            Ok(StreamItem::Done {
                reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

#[async_trait::async_trait]
impl ModelClient for Fixed {
    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        let items: Vec<Result<StreamItem, PandayError>> = vec![
            Ok(StreamItem::Delta {
                text: self.0.into(),
            }),
            Ok(StreamItem::Done {
                reason: StopReason::EndTurn,
            }),
        ];
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

/// Fails every call.
struct Broken;

#[async_trait::async_trait]
impl ModelClient for Broken {
    async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
        Err(PandayError::ModelUnavailable { tried: vec![] })
    }
}

#[tokio::test]
async fn a_model_that_always_apologises_scores_zero() {
    let client = Arc::new(Fixed("I'm sorry, I can't do that."));
    let card = json_bench::run(
        client.as_ref(),
        &ModelRef("local/qwen3.5-4b".into()),
        "local/qwen3.5-4b",
        "2026-08-19T00:00:00Z",
        &json_bench::corpus()[..10],
    )
    .await;

    assert_eq!(card.passed, 0);
    assert_eq!(card.cases, 10);
    assert_eq!(card.metrics["not_json_rate"], 1.0);
    assert_eq!(card.metrics["wrong_shape_rate"], 0.0);
    assert!(!card.meets(0.5));
}

#[tokio::test]
async fn a_call_failure_is_a_failed_case_not_a_skipped_one() {
    // A suite that drops its errors reports a rate for the cases that happened to work, which is
    // the most flattering possible lie.
    let card = json_bench::run(
        &Broken,
        &ModelRef("local/qwen3.5-4b".into()),
        "local/qwen3.5-4b",
        "2026-08-19T00:00:00Z",
        &json_bench::corpus()[..5],
    )
    .await;

    assert_eq!(card.cases, 5);
    assert_eq!(card.passed, 0);
    assert_eq!(card.metrics["call_failure_rate"], 1.0);
    assert!(card.failures[0].detail.contains("call failed"));
}

#[tokio::test]
async fn a_perfect_answer_for_one_shape_scores_that_shape() {
    // Answers the `flat-strings` shape correctly and nothing else — so the scorecard should show
    // exactly the twenty cases of that shape passing.
    let client = Fixed(r#"{"name": "panday-gateway", "language": "Rust"}"#);
    let corpus: Vec<Case> = json_bench::corpus()
        .into_iter()
        .filter(|c| c.shape == "flat-strings" || c.shape == "numbers")
        .collect();

    let card = json_bench::run(
        &client,
        &ModelRef("local/qwen3.5-4b".into()),
        "local/qwen3.5-4b",
        "2026-08-19T00:00:00Z",
        &corpus,
    )
    .await;

    assert_eq!(card.cases, 40);
    assert_eq!(card.passed, 20, "one shape right, one wrong");
    assert_eq!(card.metrics["schema_validity"], 0.5);
    // And the artifact says which shape failed, not just how many.
    assert!(card.failures.iter().all(|f| f.case.starts_with("numbers/")));
}

#[tokio::test]
async fn each_case_caps_output_tokens() {
    // Reasoning models think until the provider cap when this is unset; the
    // first live grok-4.6 run sat on one stream for minutes.
    let client = Records(std::sync::Mutex::new(None));
    let _ = json_bench::run(
        &client,
        &ModelRef("xai/grok-4.6".into()),
        "xai/grok-4.6",
        "2026-08-20T00:00:00Z",
        &json_bench::corpus()[..1],
    )
    .await;
    let req = client.0.lock().unwrap().clone().expect("issued a request");
    assert_eq!(req.sampling.max_tokens, Some(512));
    assert_eq!(req.sampling.temperature, Some(0.0));
}
