//! Session replay (docs/21 §the replay tool, M21.3).
//!
//! > "`panday replay <session_id>` — renders any session's event log as the CLI
//! > would have shown it, with `--at seq` time travel, `--diff` between two
//! > replays, and `--costs` per-turn ledger overlay. ... This tool is why
//! > state-must-fold-from-log is an invariant and not a preference (ADR-002)."
//!
//! Every function here reads *only* the event log. That is the point: if a
//! rendering needs something the log does not contain, the missing event is the
//! bug (docs/03), and this module is where that shows up first.

use panday_types::event::{Envelope, Event};
use panday_types::model::{ContentBlock, StopReason, Usage};

/// What to include in a rendering.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReplayOptions {
    /// Stop after this `seq` — `--at`, the time-travel flag.
    pub at_seq: Option<u64>,
    /// Overlay per-turn usage and cost — `--costs`.
    pub costs: bool,
    /// Include tool arguments and observations. Off by default: a replay is
    /// often read by someone debugging a *shape*, and full observations bury it.
    pub verbose: bool,
}

fn text_of(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::ToolOutput { text, .. } => text.clone(),
            ContentBlock::Artifact { summary, .. } => format!("[artifact] {summary}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn first_line(s: &str, limit: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() > limit {
        format!("{}…", line.chars().take(limit).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Indent a body block under its `[seq]` line so every line of a replay is
/// either anchored to a seq or visibly a continuation of one. Multi-line tool
/// output would otherwise produce lines that cannot be cited.
fn indented(body: &str) -> String {
    body.lines()
        .map(|l| format!("     {l}\n"))
        .collect::<String>()
}

/// Render a log the way the CLI would have shown it.
pub fn render(events: &[Envelope], opts: ReplayOptions) -> String {
    let mut renderer = Renderer::new(opts);
    events.iter().map(|e| renderer.push(e)).collect()
}

/// Incremental renderer.
///
/// The same formatter a replay uses, driven one event at a time — because a live
/// session and a replay must look identical (docs/21: a replay "renders any
/// session's event log as the CLI would have shown it"). Two formatters would drift
/// the moment one got a fix, and the whole value of the replay is that it is not a
/// separate rendering of the same facts.
pub struct Renderer {
    opts: ReplayOptions,
    turn: u32,
    turn_usage: Usage,
}

impl Renderer {
    pub fn new(opts: ReplayOptions) -> Self {
        Self {
            opts,
            turn: 0,
            turn_usage: Usage::default(),
        }
    }

    /// Render one event, advancing the turn counter and usage fold.
    pub fn push(&mut self, env: &Envelope) -> String {
        let opts = self.opts;
        let turn = &mut self.turn;
        let turn_usage = &mut self.turn_usage;
        let mut out = String::new();
        // Time travel: everything after the cut is simply not part of this
        // rendering. No interpolation — the log at seq N is a real state the
        // session actually passed through.
        if opts.at_seq.is_some_and(|at| env.seq > at) {
            return String::new();
        }

        match &env.event {
            Event::UserMessage { content, .. } => {
                out.push_str(&format!(
                    "\n[{}] user\n{}",
                    env.seq,
                    indented(&text_of(content))
                ));
            }
            Event::TurnStarted { model, .. } => {
                *turn += 1;
                *turn_usage = Usage::default();
                out.push_str(&format!(
                    "\n[{}] ── turn {turn} · {} ──\n",
                    env.seq, model.0
                ));
            }
            Event::AssistantMessage { content, usage } => {
                turn_usage.add(*usage);
                out.push_str(&format!(
                    "[{}] assistant\n{}",
                    env.seq,
                    indented(&text_of(content))
                ));
            }
            Event::ToolCall { tool, args, .. } => {
                let detail = if opts.verbose {
                    args.to_string()
                } else {
                    first_line(&args.to_string(), 72)
                };
                out.push_str(&format!("[{}] → {tool}({detail})\n", env.seq));
            }
            Event::ToolResult {
                output, is_error, ..
            } => {
                let marker = if *is_error { "✗" } else { "✓" };
                let body = if opts.verbose {
                    output.text.clone()
                } else {
                    first_line(&output.text, 72)
                };
                let (head, rest) = match body.split_once('\n') {
                    Some((h, r)) => (h.to_string(), Some(r.to_string())),
                    None => (body.clone(), None),
                };
                out.push_str(&format!(
                    "[{}] {marker} {head}   [{} → {} tok, {}]\n",
                    env.seq, output.tokens_raw, output.tokens_kept, output.strategy
                ));
                if let Some(rest) = rest {
                    out.push_str(&indented(&rest));
                }
            }
            Event::PermissionRequest { tool, action, .. } => {
                out.push_str(&format!("[{}] ? permission: {tool} — {action}\n", env.seq));
            }
            Event::PermissionDecision { decision, by, .. } => {
                out.push_str(&format!("[{}] ! {decision:?} by {by:?}\n", env.seq));
            }
            Event::Compaction {
                tokens_before,
                tokens_after,
                ..
            } => {
                out.push_str(&format!(
                    "[{}] ⇲ compacted {tokens_before} → {tokens_after} tok\n",
                    env.seq
                ));
            }
            Event::SubagentSpawned { brief, .. } => {
                out.push_str(&format!(
                    "[{}] ⇢ subagent: {}\n",
                    env.seq,
                    first_line(brief, 72)
                ));
            }
            Event::SubagentFinished { .. } => {
                out.push_str(&format!("[{}] ⇠ subagent finished\n", env.seq));
            }
            Event::SessionForked { from_seq } => {
                out.push_str(&format!("[{}] ⑂ forked from seq {from_seq}\n", env.seq));
            }
            Event::TurnFinished {
                reason,
                cost_micros,
                ..
            } => {
                out.push_str(&format!("[{}] ── {reason:?} ──\n", env.seq));
                if opts.costs {
                    out.push_str(&format!(
                        "     usage: in {} (cache read {}) · out {} · ${:.4}\n",
                        turn_usage.input_tokens,
                        turn_usage.cache_read_tokens,
                        turn_usage.output_tokens,
                        *cost_micros as f64 / 1_000_000.0
                    ));
                }
            }
            Event::Error { code, message, .. } => {
                out.push_str(&format!("[{}] ⚠ {code}: {message}\n", env.seq));
            }
            // Never persisted, but a log from elsewhere might contain one.
            Event::AssistantDelta { .. } => {}
            // docs/03 requires unknown kinds be ignored-and-preserved. A replay
            // tool that crashed on a newer server's log would be useless at
            // exactly the moment it was needed.
            Event::Unknown { payload } => {
                out.push_str(&format!(
                    "[{}] · (unknown event `{}` from a newer version)\n",
                    env.seq,
                    payload.get("event").and_then(|v| v.as_str()).unwrap_or("?")
                ));
            }
        }
        out
    }
}

/// A per-turn cost summary, folded from the log alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCost {
    pub turn: u32,
    pub usage: Usage,
    pub cost_micros: u64,
    pub stop: Option<StopReason>,
}

/// Fold per-turn costs — the `--costs` overlay.
///
/// Usage comes from `AssistantMessage` events only. docs/03: `TurnFinished.usage`
/// is "a redundant turn summary ... folds count the former only". Adding both
/// would double-bill every turn in the report.
pub fn turn_costs(events: &[Envelope]) -> Vec<TurnCost> {
    let mut out: Vec<TurnCost> = Vec::new();
    let mut current: Option<TurnCost> = None;

    for env in events {
        match &env.event {
            Event::TurnStarted { .. } => {
                if let Some(t) = current.take() {
                    out.push(t);
                }
                current = Some(TurnCost {
                    turn: out.len() as u32 + 1,
                    usage: Usage::default(),
                    cost_micros: 0,
                    stop: None,
                });
            }
            Event::AssistantMessage { usage, .. } => {
                if let Some(t) = current.as_mut() {
                    t.usage.add(*usage);
                }
            }
            Event::TurnFinished {
                reason,
                cost_micros,
                ..
            } => {
                if let Some(mut t) = current.take() {
                    t.cost_micros += cost_micros;
                    t.stop = Some(*reason);
                    out.push(t);
                }
            }
            _ => {}
        }
    }
    if let Some(t) = current.take() {
        out.push(t);
    }
    out
}

/// Line-level difference between two renderings — the `--diff` flag.
///
/// docs/21's stated use is "before/after a reducer change", so the comparison is
/// on the rendered text: that is what a human is judging, and a structural diff
/// of events would report changes that make no visible difference.
pub fn diff(before: &str, after: &str) -> String {
    let old: Vec<String> = before.lines().map(str::to_string).collect();
    let new: Vec<String> = after.lines().map(str::to_string).collect();
    panday_reducer::hunk_diff(&old, &new)
}

/// Summarise a session in one line, for a listing.
pub fn summarize(events: &[Envelope]) -> String {
    let state = crate::fold(events);
    let costs = turn_costs(events);
    let total: u64 = costs.iter().map(|c| c.cost_micros).sum();

    format!(
        "{} events · {} turns · in {} tok (cache read {}) · out {} tok · ${:.4} · last stop {:?}",
        events.len(),
        state.finished_turns,
        state.usage_total.input_tokens,
        state.usage_total.cache_read_tokens,
        state.usage_total.output_tokens,
        total as f64 / 1_000_000.0,
        state.last_stop,
    )
}
