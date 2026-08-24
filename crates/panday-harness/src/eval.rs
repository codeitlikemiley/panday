//! Reduce-then-solve: the eval that decides whether a reducer may ship (M15.5).
//!
//! docs/15 §retention tests:
//!
//! > "The nightly 'reduce-then-solve' eval (19) replays recorded sessions with
//! > reduction on/off and diffs task success — regression there blocks release,
//! > because a reducer that loses the plot is negative value at any compression
//! > ratio."
//!
//! ## Why this is not a compression benchmark
//!
//! A compression benchmark always looks good. The JetBrains measurement of `rtk`
//! (docs/15 §what the benchmark taught) reported 60–90% reduction and ~0% net cost
//! change, because the channels it compressed were not the ones being paid for.
//! ADR-007 draws the conclusion: the number is dollars, never percent.
//!
//! This harness adds the other half. Ratio and dollars say what a reduction
//! *saved*; only task success says what it *cost*. So each scenario names the facts
//! a solver needs — the failing test's name, the compiler error's file:line, the
//! conflicted path — and the solver in the loop genuinely fails when they are gone.
//! A reducer that eats the failing test's name shows up here as a task that stopped
//! being solvable, not as an impressive ratio.
//!
//! ## Why the model is a fact-checker and not an LLM
//!
//! A real model would make this eval non-deterministic and unrunnable in CI, and
//! its judgement would be the thing under test rather than the reducer's. The
//! solver here is mechanical: it reads the observation the loop actually assembled
//! and succeeds only if every required fact survived. That is a *lower bound* on a
//! real model's needs — a fact a person can see is not automatically a fact a model
//! uses — and it is the part that can be gated in CI.

use crate::testing::EchoTool;
use crate::tools::ToolRegistry;
use crate::{MemoryStore, PermissionEngine, Profile, SessionActor, TurnBudget};
use panday_reducer::Reducer;
use panday_sdk::{ItemStream, ModelClient, PandayError};
use panday_types::model::{ChatRequest, ContentBlock, ModelRef, StopReason, StreamItem, Usage};
use panday_types::{AccountId, CallId, SessionId};
use std::sync::Arc;
use std::sync::Mutex;

/// One recorded situation: a tool output, and what a solver must be able to see
/// in it.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: String,
    /// The tool whose output this is — the reducer dispatches on it.
    pub tool: String,
    /// The raw observation, as recorded.
    pub observation: String,
    /// Facts a solver needs. Substrings, because a fact is a fact whatever the
    /// surrounding formatting.
    pub required_facts: Vec<String>,
    pub is_error: bool,
}

impl Scenario {
    pub fn new(name: &str, tool: &str, observation: impl Into<String>, facts: &[&str]) -> Self {
        Self {
            name: name.into(),
            tool: tool.into(),
            observation: observation.into(),
            required_facts: facts.iter().map(|f| f.to_string()).collect(),
            // Error output is where generic elision does the most damage
            // (docs/15), so the corpus marks it and the accounting charges for it.
            is_error: true,
        }
    }

    pub fn clean(mut self) -> Self {
        self.is_error = false;
        self
    }
}

/// What one run produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub solved: bool,
    pub tokens_raw: u32,
    pub tokens_kept: u32,
    /// Facts the solver could not find. Named, because "it failed" is not
    /// actionable and "it lost the failing test's name" is.
    pub missing_facts: Vec<String>,
    pub strategy: String,
}

impl Outcome {
    pub fn ratio(&self) -> f64 {
        if self.tokens_raw == 0 {
            return 0.0;
        }
        1.0 - (self.tokens_kept as f64 / self.tokens_raw as f64)
    }
}

/// One scenario, run twice.
#[derive(Debug, Clone)]
pub struct Row {
    pub scenario: String,
    pub without_reduction: Outcome,
    pub with_reduction: Outcome,
}

impl Row {
    /// The regression: solvable before, unsolvable after.
    ///
    /// Deliberately asymmetric. A scenario that fails *both* ways is a bad
    /// scenario, not a reducer regression, and blocking a release on it would
    /// teach everyone to ignore this gate.
    pub fn is_regression(&self) -> bool {
        self.without_reduction.solved && !self.with_reduction.solved
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub rows: Vec<Row>,
}

impl Report {
    pub fn regressions(&self) -> Vec<&Row> {
        self.rows.iter().filter(|r| r.is_regression()).collect()
    }

    /// Task success with reduction on, over scenarios that are solvable at all.
    pub fn success_rate(&self) -> f64 {
        let solvable: Vec<&Row> = self
            .rows
            .iter()
            .filter(|r| r.without_reduction.solved)
            .collect();
        if solvable.is_empty() {
            return 0.0;
        }
        solvable.iter().filter(|r| r.with_reduction.solved).count() as f64 / solvable.len() as f64
    }

    /// **M19.5's gate**: "≥25% cheaper semantic tier than the provider cheap-pool it replaces".
    ///
    /// Until now this suite had **no cost dimension at all** — it measured retention and token
    /// ratio, which are not money. So the milestone could not be judged even with a model in hand,
    /// and that is the failure docs/19 M19.1 exists to prevent: a gate stated in prose is a gate
    /// nobody can fail.
    ///
    /// Cost, not tokens, because the two move independently and the milestone is about the
    /// cheaper one *winning*. A reducer that keeps 40% of the tokens but hands them to a model at
    /// three times the price is more expensive, and a ratio-only scorecard would call it a 60%
    /// improvement.
    ///
    /// Prices are **per token**, in the same unit for both sides, and supplied by the caller —
    /// this crate has no price table and must not grow one, or the gate starts depending on a
    /// catalog that drifts. `panday_router`'s catalog is where a binary gets them.
    ///
    /// Returns `None` when the incumbent would spend nothing: a saving against zero is not a
    /// percentage, and reporting one would be inventing a number.
    pub fn cost_saving(&self, incumbent_price: f64, replacement_price: f64) -> Option<f64> {
        let incumbent: f64 = self
            .rows
            .iter()
            .map(|r| r.without_reduction.tokens_raw as f64 * incumbent_price)
            .sum();
        let replacement: f64 = self
            .rows
            .iter()
            .map(|r| r.with_reduction.tokens_kept as f64 * replacement_price)
            .sum();
        if incumbent <= 0.0 {
            return None;
        }
        Some(1.0 - replacement / incumbent)
    }

    /// M19.5, whole: zero regressions **and** at least `fraction` cheaper.
    ///
    /// Both halves, because either alone is a trap. Cheapness with regressions is a reducer that
    /// saves money by losing the answer; zero regressions with no saving is a semantic tier with
    /// no reason to exist. The milestone names both and so does this.
    ///
    /// `fraction` is a ratio — M19.5 says 25%, so `cheaper_than(0.25, ...)`. Unlike route-bench's
    /// margin, which is stated in points, this one is stated as a percentage *of* the incumbent,
    /// so a ratio is the honest shape. `the_two_gates_do_not_share_a_unit` pins the difference.
    pub fn cheaper_than(
        &self,
        fraction: f64,
        incumbent_price: f64,
        replacement_price: f64,
    ) -> bool {
        self.regressions().is_empty()
            && self
                .cost_saving(incumbent_price, replacement_price)
                .is_some_and(|saved| saved >= fraction)
    }

    /// Mean reduction across the corpus. Reported *after* success, and never
    /// instead of it (ADR-007).
    pub fn mean_ratio(&self) -> f64 {
        if self.rows.is_empty() {
            return 0.0;
        }
        self.rows
            .iter()
            .map(|r| r.with_reduction.ratio())
            .sum::<f64>()
            / self.rows.len() as f64
    }

    /// A human-readable scorecard. Printed by the nightly job.
    pub fn scorecard(&self) -> String {
        let mut out = String::from("reduce-then-solve (M15.5)\n\n");
        out.push_str("  scenario                     solved  ratio   strategy\n");
        for row in &self.rows {
            out.push_str(&format!(
                "  {:28} {:>6}  {:>5.1}%  {}{}\n",
                row.scenario,
                if row.with_reduction.solved {
                    "yes"
                } else {
                    "NO"
                },
                row.with_reduction.ratio() * 100.0,
                row.with_reduction.strategy,
                if row.is_regression() {
                    format!(
                        "   ← REGRESSION, lost: {}",
                        row.with_reduction.missing_facts.join(", ")
                    )
                } else {
                    String::new()
                }
            ));
        }
        out.push_str(&format!(
            "\n  success {:.0}% of solvable · mean reduction {:.1}% · {} regression(s)\n",
            self.success_rate() * 100.0,
            self.mean_ratio() * 100.0,
            self.regressions().len()
        ));
        out
    }
}

/// The mechanical solver: succeeds iff every required fact is in the context the
/// loop assembled.
struct FactChecker {
    required: Vec<String>,
    /// What the last request actually contained, for reporting which fact went
    /// missing rather than just that one did.
    missing: Arc<Mutex<Vec<String>>>,
    turn: Mutex<u32>,
    tool: String,
}

#[async_trait::async_trait]
impl ModelClient for FactChecker {
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        let mut turn = self.turn.lock().unwrap();
        *turn += 1;
        let first = *turn == 1;
        drop(turn);

        let items: Vec<Result<StreamItem, PandayError>> = if first {
            // Turn one: run the tool that produces the observation.
            let id = CallId::new();
            vec![
                Ok(StreamItem::ToolCallStart {
                    id,
                    name: self.tool.clone(),
                    provider_id: None,
                }),
                Ok(StreamItem::ToolCallDelta {
                    id,
                    args_fragment: "{}".into(),
                }),
                Ok(StreamItem::Usage {
                    usage: Usage {
                        input_tokens: 100,
                        output_tokens: 10,
                        ..Default::default()
                    },
                }),
                Ok(StreamItem::Done {
                    reason: StopReason::ToolUse,
                }),
            ]
        } else {
            // Turn two: solve, if the facts are there. This reads the *assembled
            // context*, not the tool's return value — which is the whole point:
            // compaction and spilling happen between those two, and a fact lost
            // there is lost to a real model too.
            let seen = transcript_text(&req);
            let missing: Vec<String> = self
                .required
                .iter()
                .filter(|f| !seen.contains(f.as_str()))
                .cloned()
                .collect();
            *self.missing.lock().unwrap() = missing.clone();

            let answer = if missing.is_empty() {
                "SOLVED"
            } else {
                "I cannot tell from this output what went wrong."
            };
            vec![
                Ok(StreamItem::Delta {
                    text: answer.into(),
                }),
                Ok(StreamItem::Usage {
                    usage: Usage {
                        input_tokens: 100,
                        output_tokens: 10,
                        ..Default::default()
                    },
                }),
                Ok(StreamItem::Done {
                    reason: StopReason::EndTurn,
                }),
            ]
        };
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

fn transcript_text(req: &ChatRequest) -> String {
    req.messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::ToolOutput { text, .. } => text.clone(),
            // The summary only. An artifact's body is out of context on purpose
            // (docs/15) — counting it here would credit the reducer for facts the
            // model cannot see without spending a tool call to expand it.
            ContentBlock::Artifact { summary, .. } => summary.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run one scenario through the loop with the given reducer.
pub async fn run(scenario: &Scenario, reducer: Box<dyn Reducer>) -> Outcome {
    let store = Arc::new(MemoryStore::new());
    let missing = Arc::new(Mutex::new(Vec::new()));

    let mut tools = ToolRegistry::default();
    let tool: Box<dyn crate::tools::Tool> = if scenario.is_error {
        EchoTool::failing(&scenario.tool, &scenario.observation)
    } else {
        EchoTool::ok(&scenario.tool, &scenario.observation)
    };
    tools.register(tool);

    let mut actor = SessionActor::new(
        SessionId::new(),
        AccountId::new(),
        ModelRef("local/eval".into()),
        store.clone(),
        Arc::new(FactChecker {
            required: scenario.required_facts.clone(),
            missing: missing.clone(),
            turn: Mutex::new(0),
            tool: scenario.tool.clone(),
        }),
        tools,
        PermissionEngine::new(Profile::Dev),
        reducer,
        TurnBudget::default(),
    );

    let _ = actor.handle_user_input("why did this fail?").await;

    let log = store.all();
    let solved = log.iter().any(|e| match &e.event {
        panday_types::event::Event::AssistantMessage { content, .. } => {
            transcript_of(content).contains("SOLVED")
        }
        _ => false,
    });
    let reduced = log.iter().find_map(|e| match &e.event {
        panday_types::event::Event::ToolResult { output, .. } => Some(output.clone()),
        _ => None,
    });

    let missing_facts = missing.lock().unwrap().clone();
    Outcome {
        solved,
        tokens_raw: reduced.as_ref().map_or(0, |r| r.tokens_raw),
        tokens_kept: reduced.as_ref().map_or(0, |r| r.tokens_kept),
        missing_facts,
        strategy: reduced.map_or_else(|| "none".into(), |r| r.strategy),
    }
}

fn transcript_of(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The recorded corpus, loaded from a directory of tool outputs plus the facts each one turns
/// on.
///
/// Moved out of the test at M12.4 for the same reason route-bench was: an eval that lives only
/// inside a `#[test]` can be checked but never reported, and docs/12's weekly review is a
/// report. The facts stay in code rather than in a sidecar file — they are assertions about
/// what a human needs, and a `.txt` file of them would drift from the fixture it describes
/// without anything failing.
pub fn recorded_corpus(dir: &std::path::Path) -> Vec<Scenario> {
    let read = |name: &str| -> String {
        let path = dir.join(format!("{name}.txt"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    };
    vec![
        Scenario::new(
            "cargo test failure",
            "bash",
            read("cargo_test_failure"),
            // docs/15's own example of what a reducer must not eat.
            &["module::envelope_round_trips", "assertion `left == right`"],
        ),
        Scenario::new(
            "cargo build error",
            "bash",
            read("cargo_build_error"),
            // The code AND the location: keeping `error[E0308]` while dropping
            // `--> file:line:col` is the regression this pair exists to catch.
            &[
                "error[E0308]",
                "crates/panday-gateway/src/gateway.rs:142:23",
            ],
        ),
        Scenario::new(
            "pytest failure",
            "bash",
            read("pytest_failure"),
            &["test_token_expiry", "tests/test_auth.py:88"],
        ),
        Scenario::new(
            "git status",
            "bash",
            read("git_status"),
            &["crates/panday-sdk/src/file_1.rs"],
        )
        .clean(),
        Scenario::new(
            "large file read",
            "read_file",
            read("file_read_large"),
            &["generated_1"],
        )
        .clean(),
    ]
}

/// The shipping reducer stack, so the eval and the product cannot diverge.
pub fn production_reducer() -> Box<dyn Reducer> {
    Box::new(panday_reducer::SpillingReducer::new(
        panday_reducer::StructuralReducer::new(panday_reducer::GenericReducer::default()),
        std::sync::Arc::new(panday_reducer::MemoryArtifactStore::default()),
    ))
}

/// Run the corpus with reduction off and on, and diff task success.
///
/// `make_reducer` is a factory because a reducer carries per-session state (the
/// read ledger, the artifact store) and sharing one across scenarios would let
/// scenario N's dedup hide scenario N+1's regression.
pub async fn run_corpus(
    scenarios: &[Scenario],
    make_reducer: &dyn Fn() -> Box<dyn Reducer>,
) -> Report {
    let mut rows = Vec::new();
    for scenario in scenarios {
        // "Off" is `PassthroughReducer`, not "skip the reducer": the loop must be
        // identical in both arms, or the comparison measures the loop.
        let without = run(scenario, Box::new(panday_reducer::PassthroughReducer)).await;
        let with = run(scenario, make_reducer()).await;
        rows.push(Row {
            scenario: scenario.name.clone(),
            without_reduction: without,
            with_reduction: with,
        });
    }
    Report { rows }
}

#[cfg(test)]
mod cost_gate_tests {
    use super::{Outcome, Report, Row};

    fn outcome(solved: bool, raw: u32, kept: u32) -> Outcome {
        Outcome {
            solved,
            tokens_raw: raw,
            tokens_kept: kept,
            missing_facts: vec![],
            strategy: "test".into(),
        }
    }

    fn report(rows: Vec<(bool, u32, bool, u32)>) -> Report {
        Report {
            rows: rows
                .into_iter()
                .enumerate()
                .map(|(i, (s0, raw, s1, kept))| Row {
                    scenario: format!("s{i}"),
                    without_reduction: outcome(s0, raw, raw),
                    with_reduction: outcome(s1, raw, kept),
                })
                .collect(),
        }
    }

    #[test]
    fn cost_is_tokens_times_price_not_tokens_alone() {
        // The reason this dimension exists. Half the tokens at four times the price is *more*
        // expensive, and a ratio-only scorecard would have called it a 50% win.
        let r = report(vec![(true, 1000, true, 500)]);
        assert_eq!(r.mean_ratio(), 0.5, "half the tokens kept");

        let saving = r.cost_saving(1.0, 4.0).expect("a saving");
        assert!(
            saving < 0.0,
            "keeping half the tokens at 4x the price costs more, not less: {saving}"
        );
    }

    #[test]
    fn a_saving_against_nothing_is_not_a_number() {
        // An empty corpus, or one where the incumbent spends zero, has no percentage to report.
        // Returning 0.0 or 1.0 here would be inventing a result.
        assert_eq!(Report::default().cost_saving(1.0, 1.0), None);
        assert_eq!(report(vec![(true, 0, true, 0)]).cost_saving(1.0, 1.0), None);
    }

    #[test]
    fn the_gate_needs_both_halves() {
        // Cheap but broken: 90% saved, and a scenario that was solvable is now not.
        let broken = report(vec![(true, 1000, false, 100)]);
        assert!(
            !broken.cheaper_than(0.25, 1.0, 1.0),
            "a regression is not paid for by being cheap"
        );

        // Sound but not cheap enough: no regressions, 10% saved against a 25% bar.
        let timid = report(vec![(true, 1000, true, 900)]);
        assert!(!timid.cheaper_than(0.25, 1.0, 1.0));

        // Both: no regressions and 30% saved.
        let good = report(vec![(true, 1000, true, 700)]);
        assert!(good.cheaper_than(0.25, 1.0, 1.0));
    }

    #[test]
    fn the_two_gates_do_not_share_a_unit() {
        // route-bench's `Score::beats` takes percentage POINTS (M19.3: "≥10pt"); this one takes a
        // FRACTION of the incumbent's spend (M19.5: "≥25% cheaper"). They read alike at a call
        // site and mean different things, so each is pinned where it lives.
        let r = report(vec![(true, 1000, true, 700)]);
        assert!(
            r.cheaper_than(0.25, 1.0, 1.0),
            "0.25 is twenty-five percent"
        );
        assert!(
            !r.cheaper_than(25.0, 1.0, 1.0),
            "25.0 would be a 2500% saving, which nothing can meet"
        );
    }
}
