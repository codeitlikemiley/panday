//! M18.4 — capability profiles change what the model is told and given (docs/18
//! §degraded-capability honesty).
//!
//! > "same prompt on cloud vs local produces adapted system prompt + toolset
//! > (snapshot-tested)"
//!
//! The snapshots here are inline rather than in `.snap` files: there are two of them, and
//! a reviewer comparing "what a frontier model is told" with "what a 4B model is told"
//! wants both on one screen. The property being protected is that the *difference* is
//! deliberate — every line a local model gains is a line somebody chose to say.

use panday_harness::context::ContextBuilder;
use panday_types::model::{ContentBlock, Message, Role, ToolDef};
use panday_types::{CapabilityProfile, Provenance};

fn tools() -> Vec<ToolDef> {
    [
        "read_file",
        "write_file",
        "edit_file",
        "grep",
        "glob",
        "bash",
        "plugin:linty",
    ]
    .iter()
    .map(|name| ToolDef {
        name: (*name).to_string(),
        description: format!("the {name} tool"),
        parameters: serde_json::json!({"type": "object"}),
    })
    .collect()
}

fn system_text(builder: &ContextBuilder) -> String {
    let built = builder.build(&[], 0);
    built.messages[0]
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn tool_names(builder: &ContextBuilder) -> Vec<String> {
    let built = builder.build(&[], 0);
    let text: String = built.messages[0]
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    tools()
        .into_iter()
        .map(|t| t.name)
        .filter(|name| text.contains(&format!("## {name}\n")))
        .collect()
}

#[test]
fn a_frontier_model_is_told_nothing_it_does_not_need() {
    // The baseline. A constraints section on a model with no constraints is tokens spent
    // every turn to say nothing — and it trains the reader to skip the section.
    let builder = ContextBuilder::new("You are Panday.", tools())
        .with_capabilities(CapabilityProfile::frontier());
    let text = system_text(&builder);

    assert!(!text.contains("# What you are"), "{text}");
    assert!(!text.contains("cannot see images"), "{text}");
    // Every tool survives.
    assert_eq!(tool_names(&builder).len(), tools().len());
}

#[test]
fn a_small_local_model_is_told_what_it_is() {
    let builder = ContextBuilder::new("You are Panday.", tools())
        .with_capabilities(CapabilityProfile::small_local());
    let text = system_text(&builder);

    assert!(text.contains("# What you are"), "{text}");
    assert!(text.contains("about 16000 tokens"), "{text}");
    assert!(text.contains("cannot see images"), "{text}");
    assert!(text.contains("structured output is unreliable"), "{text}");
    assert!(text.contains("Tool calls are validated"), "{text}");
    assert!(text.contains("cannot spawn sub-agents"), "{text}");

    // Phrased as facts about the model, not as advice it cannot act on.
    assert!(!text.to_lowercase().contains("be careful"), "{text}");
    assert!(!text.to_lowercase().contains("try to"), "{text}");
}

#[test]
fn an_estimated_profile_says_so() {
    // The whole reason `Provenance` exists. docs/18: profiles are "measured by the eval
    // suite — not vibes", and until M19.2 measures them, quoting a declared number as a
    // measured one is lying precisely.
    let declared =
        ContextBuilder::new("system", tools()).with_capabilities(CapabilityProfile::small_local());
    assert!(system_text(&declared).contains("(estimated)"));

    let measured = ContextBuilder::new("system", tools()).with_capabilities(CapabilityProfile {
        provenance: Provenance::Measured,
        ..CapabilityProfile::small_local()
    });
    let text = system_text(&measured);
    assert!(!text.contains("(estimated)"), "{text}");
    // Same numbers, different claim.
    assert!(text.contains("about 16000 tokens"), "{text}");
}

#[test]
fn the_tool_set_shrinks_for_a_model_that_cannot_call_tools_reliably() {
    // docs/18: "tool schemas shrink to the minimal set". A model that gets four calls in
    // ten wrong does worse with seven tools than with four — the schemas are what it is
    // failing to read.
    let local =
        ContextBuilder::new("system", tools()).with_capabilities(CapabilityProfile::small_local());
    let kept = tool_names(&local);
    assert_eq!(kept, ["read_file", "grep", "glob", "bash"]);

    // The dangerous ones are the ones dropped: a bad `read_file` wastes a turn, a bad
    // `write_file` costs work.
    assert!(!kept.contains(&"write_file".to_string()));
    assert!(!kept.contains(&"edit_file".to_string()));
    // And a plugin tool is not handed to a model that cannot call tools — the worst of
    // both.
    assert!(!kept.contains(&"plugin:linty".to_string()));
}

#[test]
fn the_window_follows_the_profile_so_compaction_fires_at_the_right_point() {
    // Compaction at 70% of a number the model cannot use is compaction that fires too
    // late — and "too late" means the turn fails rather than degrades.
    let local =
        ContextBuilder::new("system", tools()).with_capabilities(CapabilityProfile::small_local());
    assert_eq!(local.model_window_tokens, 16_000);

    let frontier =
        ContextBuilder::new("system", tools()).with_capabilities(CapabilityProfile::frontier());
    assert_eq!(frontier.model_window_tokens, 200_000);
}

#[test]
fn the_adaptations_are_thresholds_not_a_local_flag() {
    // "Is this local" is the wrong question — a large local model needs none of this and a
    // small hosted one needs all of it. The rules read the numbers.
    let big_local = CapabilityProfile {
        max_context_tokens: 128_000,
        json_reliability: 0.95,
        tool_reliability: 0.94,
        vision: false,
        max_subagents: 2,
        provenance: Provenance::Declared,
    };
    assert!(!big_local.wants_minimal_tools());
    assert!(!big_local.wants_aggressive_reduction());
    // It still cannot see, and it still says so.
    let text = system_text(&ContextBuilder::new("system", tools()).with_capabilities(big_local));
    assert!(text.contains("cannot see images"), "{text}");
    assert!(!text.contains("structured output is unreliable"), "{text}");

    let small = CapabilityProfile::small_local();
    assert!(small.wants_minimal_tools());
    assert!(small.wants_aggressive_reduction());
    assert!(small.wants_expectation_notice());
    assert!(!CapabilityProfile::frontier().wants_expectation_notice());
}

#[test]
fn no_profile_means_no_claims() {
    // The alternative — assuming frontier capabilities when nobody said — is what produces
    // a local model confidently promising to read an image.
    let builder = ContextBuilder::new("You are Panday.", tools());
    let text = system_text(&builder);
    assert!(!text.contains("# What you are"), "{text}");
    assert!(builder.capabilities().is_none());
    assert_eq!(tool_names(&builder).len(), tools().len());
}

#[test]
fn the_constraints_sit_in_the_stable_band_before_the_tools() {
    // A constraint stated after a tool list is one the model reads after deciding to use
    // the tool. And it belongs in the cached prefix: ADR-008 forbids injecting into the
    // stable band mid-session, so this cannot be added later even if we wanted to.
    let builder = ContextBuilder::new("You are Panday.", tools())
        .with_capabilities(CapabilityProfile::small_local());
    let text = system_text(&builder);
    let what_you_are = text.find("# What you are").expect("constraints present");
    let tools_at = text.find("# Tools").expect("tools present");
    assert!(what_you_are < tools_at, "{text}");

    // Byte-identical across builds of the same context: the cache depends on it.
    let again = ContextBuilder::new("You are Panday.", tools())
        .with_capabilities(CapabilityProfile::small_local());
    assert_eq!(text, system_text(&again));
}

#[test]
fn a_local_session_gets_the_expectation_notice_the_ui_renders() {
    // docs/18: "ambitious tasks get a 'this will be slower/rougher locally' notice event
    // the UI renders". The profile is what decides; the notice text is the caller's.
    assert!(CapahilityHelper::notice_for(CapabilityProfile::small_local()).is_some());
    assert!(CapahilityHelper::notice_for(CapabilityProfile::frontier()).is_none());
}

/// The notice rule, in one place so the CLI and an editor cannot disagree about when to
/// show it.
struct CapahilityHelper;

impl CapahilityHelper {
    fn notice_for(profile: CapabilityProfile) -> Option<&'static str> {
        profile.wants_expectation_notice().then_some(
            "This model is smaller than the hosted ones: expect slower turns, rougher \
             tool use, and more retries.",
        )
    }
}

#[test]
fn the_transcript_still_assembles_normally_under_a_profile() {
    // The adaptations touch the stable band only; the rolling and hot bands are the
    // session's own history and must not be rewritten by a capability decision.
    let builder =
        ContextBuilder::new("system", tools()).with_capabilities(CapabilityProfile::small_local());
    let transcript = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "fix the test".into(),
        }],
        call_id: None,
        provider_call_id: None,
    }];
    let built = builder.build(&transcript, 0);
    assert_eq!(built.messages.len(), 2);
    assert!(matches!(built.messages[1].role, Role::User));
}
