//! Measuring what a model can actually do (docs/19 M19.2, docs/18 §degraded-capability honesty).
//!
//! > "Capability profiles for 3 local catalog models, generated not hand-written."
//!
//! A `CapabilityProfile` decides real behaviour: the harness shrinks tool schemas, drops subagent
//! fan-out and switches the reducer to `aggressive` based on these numbers. Hand-written, they are
//! somebody's impression of a model from an afternoon; generated, they are a measurement with a
//! date on it. The type has carried a `provenance` field since M18.4 precisely so the difference is
//! visible at the point of use.
//!
//! **Every probe is a question with a checkable answer**, because the alternative — asking a model
//! to rate itself — measures its confidence rather than its ability.
//!
//! - *Usable context* is found by bisection, not by reading a spec sheet. A model that advertises
//!   128k and loses the thread at 20k has a usable context of 20k, and docs/18 is explicit that the
//!   number we want is "where a 4B model stops following a long transcript".
//! - *JSON reliability* is `json-bench`'s pass rate (M19.1), reused rather than re-implemented.
//! - *Tool reliability* is whether a requested call arrives with well-formed arguments.
//! - *Vision* is a capability, not a rate: either the adapter reports it or the model is asked and
//!   fails. Bisecting it would be nonsense.
//!
//! What cannot be measured this way is `max_subagents` — it depends on a whole agent loop, not a
//! model call — so it stays declared, and the emitted profile says which fields were measured.

use crate::json_bench;
use panday_sdk::{ModelClient, PandayError};
use panday_types::capability::{CapabilityProfile, Provenance};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StreamItem,
};

/// A needle in a haystack: the standard way to ask "are you still following this?".
///
/// The needle is placed in the *middle* rather than at the end, because a model that only reads the
/// tail passes an end-placed needle while being useless for the thing we care about — a long
/// transcript whose important fact was three tool calls ago.
pub fn haystack(tokens: usize, needle: &str) -> String {
    // ~4 characters per token is the usual rough conversion, and rough is fine: the bisection
    // reports the size it actually sent.
    let filler = "The quick brown fox jumps over the lazy dog. ";
    let target_chars = tokens * 4;
    let half = target_chars / 2;

    let mut out = String::with_capacity(target_chars + needle.len());
    while out.len() < half {
        out.push_str(filler);
    }
    out.push_str(needle);
    while out.len() < target_chars {
        out.push_str(filler);
    }
    out
}

/// What one measurement run found.
#[derive(Debug, Clone, PartialEq)]
pub struct Measured {
    pub profile: CapabilityProfile,
    /// Which fields were actually measured, for the record that goes next to the numbers.
    pub measured_fields: Vec<&'static str>,
    /// Context sizes tried, largest first. Kept because "we tested 32k and it failed at 64k" is a
    /// different claim from "it does 32k", and a scorecard reader deserves the first one.
    pub context_probes: Vec<(u32, bool)>,
}

/// Find the largest context at which the model can still find a fact in the middle.
///
/// Bisection between a floor that is known to work and a ceiling that is known not to. Returns the
/// largest *verified* size — never an interpolation, because a profile that claims a context the
/// model was never asked to hold is worse than one that under-claims.
pub async fn measure_context(
    client: &dyn ModelClient,
    model: &ModelRef,
    floor: u32,
    ceiling: u32,
) -> (u32, Vec<(u32, bool)>) {
    let mut probes = Vec::new();
    let (mut low, mut high) = (floor, ceiling);
    let mut best = 0u32;

    // A handful of steps is enough: each one costs a full-context call, and the difference between
    // 31k and 32k does not change a single downstream decision.
    for _ in 0..6 {
        if low > high {
            break;
        }
        let size = low + (high - low) / 2;
        if size == 0 {
            break;
        }
        let ok = recalls_the_needle(client, model, size).await;
        probes.push((size, ok));
        if ok {
            best = best.max(size);
            low = size + 1;
        } else {
            high = size.saturating_sub(1);
        }
    }
    // Largest first: a reader wants the ceiling that failed before the size that worked.
    probes.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
    (best, probes)
}

async fn recalls_the_needle(client: &dyn ModelClient, model: &ModelRef, tokens: u32) -> bool {
    // A distinctive fact that cannot be guessed from the filler, and that a model cannot produce by
    // continuing the pattern.
    const NEEDLE: &str = "The maintenance code for the gateway is QUARTZ-7741.";
    let prompt = format!(
        "{}\n\nWhat is the maintenance code? Answer with the code only.",
        haystack(tokens as usize, NEEDLE)
    );
    match ask(client, model, &prompt).await {
        Ok(answer) => answer.contains("QUARTZ-7741"),
        // A refusal or an error is a failure at this size — which is the honest reading: whatever
        // the cause, the model did not answer the question at that context.
        Err(_) => false,
    }
}

/// Ask for a tool call and see whether the arguments are usable.
pub async fn measure_tool_reliability(
    client: &dyn ModelClient,
    model: &ModelRef,
    attempts: u32,
) -> f32 {
    if attempts == 0 {
        return 0.0;
    }
    let mut good = 0u32;
    for i in 0..attempts {
        let prompt = format!(
            "Call the `read_file` tool for the file `src/lib.rs`, starting at line {}. \
             Reply with JSON only: {{\"tool\": \"read_file\", \"args\": {{\"path\": …, \"line\": …}}}}",
            i + 1
        );
        let Ok(answer) = ask(client, model, &prompt).await else {
            continue;
        };
        let text = json_bench::unfence(&answer);
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            continue;
        };
        // Well-formed *and* correct: a call with the right shape and the wrong path is a call that
        // reads the wrong file, which is worse than a malformed one that fails loudly.
        let right_tool = value["tool"] == "read_file";
        let right_path = value["args"]["path"] == "src/lib.rs";
        let right_line = value["args"]["line"] == serde_json::json!(i + 1);
        if right_tool && right_path && right_line {
            good += 1;
        }
    }
    good as f32 / attempts as f32
}

/// Everything measurable, in one pass.
pub async fn measure(
    client: &dyn ModelClient,
    model: &ModelRef,
    declared: CapabilityProfile,
    ceiling: u32,
) -> Measured {
    let cases = json_bench::corpus();
    let json_card = json_bench::run(client, model, &model.0, "measured", &cases[..40]).await;
    let tools = measure_tool_reliability(client, model, 10).await;
    let (context, probes) = measure_context(client, model, 1_000, ceiling).await;

    Measured {
        profile: CapabilityProfile {
            // A measurement of zero is a real result and is kept: it means the model failed the
            // smallest probe, and quietly substituting the declared number would hide that.
            max_context_tokens: context,
            json_reliability: json_card.rate() as f32,
            tool_reliability: tools,
            // Not measurable from a text probe alone; carried through from the declaration so the
            // emitted profile is complete.
            vision: declared.vision,
            max_subagents: declared.max_subagents,
            provenance: Provenance::Measured,
        },
        measured_fields: vec!["max_context_tokens", "json_reliability", "tool_reliability"],
        context_probes: probes,
    }
}

async fn ask(
    client: &dyn ModelClient,
    model: &ModelRef,
    prompt: &str,
) -> Result<String, PandayError> {
    use futures_util::StreamExt;

    let request = ChatRequest {
        model: model.clone(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: prompt.to_string(),
            }],
            call_id: None,
            provider_call_id: None,
        }],
        tools: vec![],
        sampling: Sampling {
            // A profile that moved between runs would measure sampling noise as capability.
            temperature: Some(0.0),
            // Needle and tool-JSON answers are short; omitting this lets a
            // reasoning model think until the provider cap.
            max_tokens: Some(256),
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

    let collect = async {
        let mut stream = client.chat(request).await?;
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let StreamItem::Delta { text: delta } = item? {
                text.push_str(&delta);
            }
        }
        Ok(text)
    };
    match tokio::time::timeout(std::time::Duration::from_secs(90), collect).await {
        Ok(result) => result,
        Err(_) => Err(PandayError::Provider {
            upstream: "profile".into(),
            message: "timed out after 90s".into(),
            retryable: true,
        }),
    }
}

/// The catalog entry's YAML fields, for pasting into `catalog/default.yaml`.
///
/// Emitted rather than written in place: a measurement rewriting a checked-in catalog unattended is
/// a model's bad afternoon silently becoming the routing table. A person pastes it, and the diff is
/// the review.
pub fn to_yaml(model: &ModelRef, measured: &Measured, at: &str) -> String {
    let p = &measured.profile;
    format!(
        "  # measured {at} — {} (probes: {})\n  \
         - id: {}\n    \
         context: {}\n    \
         json: {:.2}\n    \
         tools: {:.2}\n    \
         vision: {}\n    \
         max_subagents: {}\n    \
         provenance: measured\n",
        measured.measured_fields.join(", "),
        measured
            .context_probes
            .iter()
            .map(|(size, ok)| format!("{size}{}", if *ok { "✓" } else { "✗" }))
            .collect::<Vec<_>>()
            .join(" "),
        model.0,
        p.max_context_tokens,
        p.json_reliability,
        p.tool_reliability,
        p.vision,
        p.max_subagents,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_needle_sits_in_the_middle_not_at_the_end() {
        // A model that only reads the tail passes an end-placed needle while being useless for a
        // long transcript whose important fact was three tool calls ago.
        let text = haystack(1_000, "NEEDLE-HERE");
        let at = text.find("NEEDLE-HERE").expect("the needle");
        let ratio = at as f64 / text.len() as f64;
        assert!((0.3..0.7).contains(&ratio), "needle at {ratio}");
    }

    #[test]
    fn the_haystack_is_roughly_the_size_asked_for() {
        let text = haystack(2_000, "x");
        let tokens = text.len() / 4;
        assert!((1_800..2_200).contains(&tokens), "{tokens} tokens");
    }

    #[test]
    fn a_measured_profile_says_it_was_measured() {
        let measured = Measured {
            profile: CapabilityProfile {
                max_context_tokens: 16_000,
                json_reliability: 0.71,
                tool_reliability: 0.6,
                vision: false,
                max_subagents: 1,
                provenance: Provenance::Measured,
            },
            measured_fields: vec!["max_context_tokens"],
            context_probes: vec![(32_000, false), (16_000, true)],
        };
        let yaml = to_yaml(
            &ModelRef("local/qwen3.5-4b".into()),
            &measured,
            "2026-08-20",
        );
        assert!(yaml.contains("provenance: measured"));
        assert!(yaml.contains("context: 16000"));
        // The probes are in the comment: "we tested 32k and it failed" is a different claim from
        // "it does 16k", and the reader deserves the first.
        assert!(yaml.contains("32000✗"), "{yaml}");
        assert!(yaml.contains("16000✓"), "{yaml}");
    }
}
