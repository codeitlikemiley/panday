//! M13.4 — cache-aligned assembly and compaction, measured.
//!
//! docs/13's acceptance is a number: "≥70% cache-read ratio on a 30-turn
//! session replay." So this test simulates prefix caching over a real 30-turn
//! assembly and computes the ratio, rather than asserting the layout "looks
//! right".
//!
//! ## How the simulation works
//!
//! Both major providers cache by **prefix** (ADR-008). On each turn, the
//! tokens that can be served from cache are those in the longest prefix
//! identical to what was sent last turn; everything after is fresh. That is
//! the whole model, and it is what makes ordering load-bearing: change one
//! byte early and every token behind it re-prices at 1x.

use panday_harness::context::{Band, Context, ContextBuilder, ReducerSummarizer, Summarizer};
use panday_types::model::{ContentBlock, Message, Role, ToolDef};

fn tools() -> Vec<ToolDef> {
    [
        "read_file",
        "write_file",
        "edit_file",
        "grep",
        "glob",
        "bash",
    ]
    .iter()
    .map(|n| ToolDef {
        name: (*n).into(),
        description: format!("The {n} tool, which does what its name suggests."),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}, "cmd": {"type": "string"}}
        }),
    })
    .collect()
}

fn msg(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text { text: text.into() }],
        call_id: None,
        provider_call_id: None,
    }
}

fn tokens_of(m: &Message) -> u32 {
    m.content
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::ToolOutput { text, .. } => text.len(),
            ContentBlock::Artifact { summary, .. } => summary.len(),
        })
        .sum::<usize>() as u32
        / 4
}

#[derive(Default, Debug)]
struct CacheMeter {
    cache_read: u64,
    fresh: u64,
    previous: Vec<Message>,
}

impl CacheMeter {
    /// Charge one turn against the previous turn's prefix.
    fn send(&mut self, ctx: &Context) {
        let shared = ctx
            .messages
            .iter()
            .zip(self.previous.iter())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| tokens_of(a) as u64)
            .sum::<u64>();

        let total: u64 = ctx.messages.iter().map(|m| tokens_of(m) as u64).sum();
        self.cache_read += shared;
        self.fresh += total.saturating_sub(shared);
        self.previous = ctx.messages.clone();
    }

    fn ratio(&self) -> f64 {
        let total = self.cache_read + self.fresh;
        if total == 0 {
            return 0.0;
        }
        self.cache_read as f64 / total as f64
    }
}

/// A realistic turn: a user message, an assistant reply, and a chunky tool
/// observation — the shape that actually fills a window.
fn turn_messages(i: usize) -> Vec<Message> {
    vec![
        msg(Role::User, &format!("Turn {i}: please look at module {i}.")),
        msg(
            Role::Assistant,
            &format!("Reading module {i} and checking its tests."),
        ),
        msg(
            Role::Tool,
            &format!(
                "module {i} contents\n{}",
                (0..40)
                    .map(|l| format!("    line {l} of module {i}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        ),
    ]
}

#[test]
fn a_thirty_turn_session_reaches_the_cache_read_target() {
    let builder = ContextBuilder::new(
        "You are a coding agent working in a Rust repository. Be precise.",
        tools(),
    );

    let mut transcript: Vec<Message> = Vec::new();
    let mut meter = CacheMeter::default();

    for i in 0..30 {
        let hot_from = transcript.len();
        transcript.extend(turn_messages(i));
        let ctx = builder.build(&transcript, hot_from);
        meter.send(&ctx);
    }

    let ratio = meter.ratio();
    println!(
        "30-turn replay: {:.1}% cache-read ({} cached / {} fresh tokens)",
        ratio * 100.0,
        meter.cache_read,
        meter.fresh
    );
    assert!(
        ratio >= 0.70,
        "docs/13 M13.4 requires >=70% cache reads; got {:.1}%",
        ratio * 100.0
    );
}

#[test]
fn shuffling_the_layout_destroys_the_cache_ratio() {
    // The control that gives the number above meaning. If a "cache-aligned"
    // layout scores the same as a deliberately churned one, the alignment is
    // doing nothing and the test is decorative.
    let builder = ContextBuilder::new("You are a coding agent.", tools());

    let mut aligned_meter = CacheMeter::default();
    let mut churned_meter = CacheMeter::default();
    let mut transcript: Vec<Message> = Vec::new();

    for i in 0..30 {
        let hot_from = transcript.len();
        transcript.extend(turn_messages(i));

        let ctx = builder.build(&transcript, hot_from);
        aligned_meter.send(&ctx);

        // The banned pattern from ADR-008: injecting something volatile into
        // the stable region each turn (e.g. "dynamically reordering tools").
        let mut churned = ctx.clone();
        churned.messages[0] = msg(
            Role::System,
            &format!(
                "You are a coding agent. [turn {i}, {} tools]",
                tools().len()
            ),
        );
        churned_meter.send(&churned);
    }

    println!(
        "aligned {:.1}% vs churned {:.1}%",
        aligned_meter.ratio() * 100.0,
        churned_meter.ratio() * 100.0
    );
    assert!(
        aligned_meter.ratio() > churned_meter.ratio() + 0.5,
        "churning the stable prefix should collapse the ratio, but aligned={:.2} churned={:.2}",
        aligned_meter.ratio(),
        churned_meter.ratio()
    );
    assert!(
        churned_meter.ratio() < 0.05,
        "a mutated stable prefix must invalidate essentially everything"
    );
}

#[test]
fn compaction_shrinks_the_window_without_losing_the_record() {
    // docs/13: "compaction is a *view* optimization, never data loss."
    let mut builder = ContextBuilder::new("You are a coding agent.", tools());
    builder.model_window_tokens = 4_000;

    let mut transcript: Vec<Message> = Vec::new();
    for i in 0..30 {
        transcript.extend(turn_messages(i));
    }

    let before = builder.build(&transcript, transcript.len());
    assert!(
        builder.should_compact(&before),
        "the fixture should be large enough to trigger compaction"
    );

    // Compact the oldest half of the rolling window.
    let split = transcript.len() / 2;
    let summariser = ReducerSummarizer(panday_reducer::StructuralReducer::new(
        panday_reducer::GenericReducer::default(),
    ));
    let summary = summariser.summarize(&transcript[..split]);
    builder.add_summary(&summary);

    let compacted = builder.build(&transcript[split..], transcript.len() - split);

    assert!(
        compacted.approx_tokens() < before.approx_tokens(),
        "compaction did not reduce the window: {} -> {}",
        before.approx_tokens(),
        compacted.approx_tokens()
    );
    // The summary is in the semi-stable band, where it will itself be cached.
    assert_eq!(compacted.bands[1], Band::SemiStable);
    assert!(!summary.is_empty(), "an empty summary is data loss");
}

#[test]
fn compaction_preserves_the_cache_prefix_it_sits_behind() {
    // Compaction that churned the stable band would cost more than it saved.
    let mut builder = ContextBuilder::new("You are a coding agent.", tools());
    let before_stable = builder.build(&[], 0).messages[0].clone();

    builder.add_summary("turns 0-15 summarised");
    let after = builder.build(&[msg(Role::User, "next")], 0);

    assert_eq!(
        after.messages[0], before_stable,
        "compaction must append, never rewrite the stable prefix"
    );
}

#[test]
fn the_cacheable_prefix_excludes_the_current_turn() {
    // The hot band is by definition not reusable next turn.
    let builder = ContextBuilder::new("You are a coding agent.", tools());
    let transcript = vec![msg(Role::User, "old turn"), msg(Role::User, "current turn")];
    let ctx = builder.build(&transcript, 1);

    assert!(ctx.cacheable_prefix_tokens() < ctx.approx_tokens());
    assert!(ctx.cacheable_prefix_tokens() > 0);
}
