//! M19.2 — the profile generator, driven by models that fail in specific ways.
//!
//! A generator is only worth trusting if it reports a *bad* model as bad. So every fake here is bad
//! in one particular way, and the assertion is that the number moves accordingly.

use panday_harness::profiling::{measure, measure_context, measure_tool_reliability};
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::capability::{CapabilityProfile, Provenance};
use panday_types::model::{ChatRequest, ContentBlock, ModelRef, Role, StopReason, StreamItem};

fn declared() -> CapabilityProfile {
    CapabilityProfile {
        max_context_tokens: 128_000,
        json_reliability: 0.9,
        tool_reliability: 0.9,
        vision: false,
        max_subagents: 1,
        provenance: Provenance::Declared,
    }
}

fn reply(text: String) -> ItemStream {
    let items: Vec<Result<StreamItem, PandayError>> = vec![
        Ok(StreamItem::Delta { text }),
        Ok(StreamItem::Done {
            reason: StopReason::EndTurn,
        }),
    ];
    Box::pin(futures_util::stream::iter(items))
}

fn prompt_of(req: &ChatRequest) -> String {
    req.messages
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Finds the needle only when the prompt is under `limit` characters — a model with a real usable
/// context, whatever its spec sheet claims.
struct ShortContext {
    limit: usize,
}

#[async_trait::async_trait]
impl ModelClient for ShortContext {
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        let prompt = prompt_of(&req);
        if prompt.len() <= self.limit && prompt.contains("QUARTZ-7741") {
            return Ok(reply("QUARTZ-7741".into()));
        }
        Ok(reply("I'm not sure I can see that.".into()))
    }
}

#[tokio::test]
async fn the_measured_context_is_the_one_that_worked_not_the_one_advertised() {
    // The number docs/18 actually wants: "where a 4B model stops following a long transcript".
    let client = ShortContext { limit: 40_000 }; // ~10k tokens
    let (context, probes) =
        measure_context(&client, &ModelRef("local/fake".into()), 1_000, 128_000).await;

    assert!(context > 0, "found nothing");
    assert!(
        (5_000..=12_000).contains(&context),
        "measured {context}, expected around 10k"
    );
    // Never an interpolation: every reported size was actually tried and passed.
    assert!(probes.iter().any(|(size, ok)| *size == context && *ok));
    // And the failures are kept, because "we tried 64k and it failed" is its own claim.
    assert!(probes.iter().any(|(_, ok)| !*ok));
}

#[tokio::test]
async fn a_model_that_never_recalls_anything_measures_zero_rather_than_the_declared_number() {
    // Substituting the declaration here would hide the only interesting result.
    struct Useless;
    #[async_trait::async_trait]
    impl ModelClient for Useless {
        async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
            Ok(reply("Sorry!".into()))
        }
    }

    let (context, _) =
        measure_context(&Useless, &ModelRef("local/fake".into()), 1_000, 32_000).await;
    assert_eq!(context, 0);
}

#[tokio::test]
async fn tool_reliability_counts_correct_calls_not_merely_parseable_ones() {
    // A call with the right shape and the wrong path reads the wrong file, which is worse than a
    // malformed one that fails loudly.
    struct WrongPath;
    #[async_trait::async_trait]
    impl ModelClient for WrongPath {
        async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
            Ok(reply(
                r#"{"tool":"read_file","args":{"path":"src/main.rs","line":1}}"#.into(),
            ))
        }
    }

    let score = measure_tool_reliability(&WrongPath, &ModelRef("local/fake".into()), 5).await;
    assert_eq!(score, 0.0, "well-formed but wrong must not count");
}

#[tokio::test]
async fn a_model_that_gets_tool_calls_right_scores_one() {
    /// Echoes back exactly what was asked for, which is what a good model does.
    struct Obedient;
    #[async_trait::async_trait]
    impl ModelClient for Obedient {
        async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
            let prompt = prompt_of(&req);
            // Pull the requested line number out of the prompt, as a model reading it would.
            let line: u32 = prompt
                .split("starting at line ")
                .nth(1)
                .and_then(|rest| rest.split('.').next())
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(0);
            Ok(reply(format!(
                r#"```json
{{"tool":"read_file","args":{{"path":"src/lib.rs","line":{line}}}}}
```"#
            )))
        }
    }

    let score = measure_tool_reliability(&Obedient, &ModelRef("local/fake".into()), 6).await;
    assert_eq!(score, 1.0, "a fenced but correct answer must count");
}

#[tokio::test]
async fn a_full_measurement_marks_itself_measured_and_lists_what_it_measured() {
    let client = ShortContext { limit: 30_000 };
    let measured = measure(&client, &ModelRef("local/fake".into()), declared(), 32_000).await;

    assert_eq!(measured.profile.provenance, Provenance::Measured);
    // json-bench's corpus expects JSON; this fake answers prose, so the rate should be at the floor
    // rather than inheriting the declared 0.9.
    assert!(measured.profile.json_reliability < 0.5);
    assert!(measured.measured_fields.contains(&"max_context_tokens"));
    // What cannot be measured from a text probe is carried through rather than invented.
    assert_eq!(measured.profile.max_subagents, declared().max_subagents);
    assert!(!measured.measured_fields.contains(&"max_subagents"));
}
