//! `route-bench` — the classifier eval, as a library (M12.3's corpus, M12.4's input).
//!
//! docs/19 §gates: "a model (base or tuned, cloud or GGUF) enters routing pools only with a
//! scorecard". A scorecard needs the eval to be callable, not just assertable — so the corpus
//! and the scoring live here and the test suite is one caller among several. `cargo xtask
//! scorecard` is the other.
//!
//! The corpus moved out of `tests/classification.rs` at M12.4 for exactly that reason: an
//! eval that only exists inside a `#[test]` can be *checked* but never *reported*, and
//! docs/12's weekly review is a report.

use crate::classify::TRUST_THRESHOLD;
use crate::Classifier;
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, TaskClass, ToolDef,
};

pub struct Case {
    pub name: &'static str,
    pub expect: TaskClass,
    pub prompt: &'static str,
    pub tools: bool,
    pub tool_results: bool,
}

pub const fn case(name: &'static str, expect: TaskClass, prompt: &'static str) -> Case {
    Case {
        name,
        expect,
        prompt,
        tools: false,
        tool_results: false,
    }
}

pub fn request(c: &Case) -> ChatRequest {
    let mut messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: c.prompt.to_string(),
        }],
        call_id: None,
        provider_call_id: None,
    }];
    if c.tool_results {
        messages.push(Message {
            role: Role::Tool,
            content: vec![ContentBlock::Text {
                text: "test result: FAILED. 1 failed".into(),
            }],
            call_id: None,
            provider_call_id: None,
        });
    }

    ChatRequest {
        model: ModelRef::auto(),
        messages,
        tools: if c.tools {
            vec![ToolDef {
                name: "bash".into(),
                description: "run a command".into(),
                parameters: serde_json::json!({"type": "object"}),
            }]
        } else {
            vec![]
        },
        sampling: Sampling::default(),
        cache: Default::default(),
        stream: true,
        metadata: CallMeta {
            account: AccountId::new(),
            request: RequestId::new(),
            session: None,
            turn: None,
            // Unset: the whole point is that the classifier guesses.
            task: None,
        },
    }
}

/// The labelled corpus. Deliberately includes cases the heuristic is expected
/// to find hard — a corpus of only easy examples measures nothing.
pub fn corpus() -> Vec<Case> {
    vec![
        case(
            "fix a failing test",
            TaskClass::Code,
            "cargo test fails on main, please fix the bug",
        ),
        case(
            "compiler error",
            TaskClass::Code,
            "error[E0308]: mismatched types in gateway.rs:142",
        ),
        case(
            "refactor ask",
            TaskClass::Code,
            "refactor this function to take a slice",
        ),
        case(
            "stack trace",
            TaskClass::Code,
            "here is a traceback, the app panics on startup",
        ),
        case(
            "file reference",
            TaskClass::Code,
            "why does src/lib.rs not compile?",
        ),
        Case {
            tools: true,
            tool_results: true,
            ..case("mid agent loop", TaskClass::Code, "keep going")
        },
        Case {
            tools: true,
            ..case(
                "tools offered",
                TaskClass::Code,
                "have a look around the repo",
            )
        },
        case(
            "explicit summarise",
            TaskClass::Summarize,
            "summarize this thread for me",
        ),
        case(
            "tldr",
            TaskClass::Summarize,
            "tldr of the discussion above?",
        ),
        case(
            "key points",
            TaskClass::Summarize,
            "give me the key points, condense it",
        ),
        case(
            "extract json",
            TaskClass::Extract,
            "extract the invoice fields as json",
        ),
        case(
            "list all",
            TaskClass::Extract,
            "list all email addresses in this text",
        ),
        case(
            "return only",
            TaskClass::Extract,
            "return only the version numbers, as json",
        ),
        case("greeting", TaskClass::Chat, "hey, how are you doing today?"),
        case(
            "open question",
            TaskClass::Chat,
            "what do you think about remote work?",
        ),
        case(
            "opinion",
            TaskClass::Chat,
            "which city would you rather live in?",
        ),
        // --- deliberately hard ---
        //
        // A corpus the classifier aces measures nothing. These are the cases
        // where two classes genuinely compete, plus keyword traps where a
        // marker appears in ordinary prose. The suite does not require the
        // heuristic to get them right — it requires it not to be CONFIDENTLY
        // wrong about them.
        case(
            "summarise a function (code vs summarize)",
            TaskClass::Code,
            "can you summarize what this function in parser.rs does?",
        ),
        case(
            "extract from a log (extract vs code)",
            TaskClass::Extract,
            "extract the error message from this build log",
        ),
        case(
            "tldr on a failure (summarize vs code)",
            TaskClass::Code,
            "tldr on why the cargo build broke?",
        ),
        case(
            "list failing tests (extract vs code)",
            TaskClass::Code,
            "list all the tests that fail right now",
        ),
        // Keyword traps: a marker word used in ordinary conversation.
        case(
            "trap: class dismissed",
            TaskClass::Chat,
            "the teacher said class dismissed and everyone left",
        ),
        case(
            "trap: fix the meeting",
            TaskClass::Chat,
            "can we fix a time to talk next week?",
        ),
        case(
            "trap: git as a word",
            TaskClass::Chat,
            "he is a legit good cook, honestly",
        ),
        case(
            "trap: implement a policy",
            TaskClass::Chat,
            "should the company implement a four day week?",
        ),
        // --- M19.1: more of the real shapes, still hand-labelled ---
        //
        // Grown by hand rather than generated. A template × variable corpus at this size would
        // measure whether the classifier learned our templates; the phrasings below are the ways
        // people actually open a request, including the ones with no verb in them at all.
        case(
            "stack trace pasted",
            TaskClass::Code,
            "thread 'main' panicked at src/gateway.rs:412: index out of bounds",
        ),
        case(
            "bare file path",
            TaskClass::Code,
            "crates/panday-router/src/policy.rs",
        ),
        case(
            "diff review",
            TaskClass::Code,
            "does this patch look right to you? @@ -12,7 +12,9 @@",
        ),
        case(
            "add a test",
            TaskClass::Code,
            "write a test that covers the empty-chain case",
        ),
        case(
            "dependency bump",
            TaskClass::Code,
            "bump sqlx to 0.9 and fix whatever breaks",
        ),
        case(
            "performance ask",
            TaskClass::Code,
            "this endpoint takes 800ms, find out why",
        ),
        case(
            "migration",
            TaskClass::Code,
            "add a column for last_used_at and backfill it",
        ),
        case(
            "no verb, error text",
            TaskClass::Code,
            "error[E0277]: the trait bound `T: Send` is not satisfied",
        ),
        case(
            "shell one-liner",
            TaskClass::Code,
            "how do I find every file over 10MB in this repo",
        ),
        case(
            "release notes",
            TaskClass::Summarize,
            "turn these forty commits into release notes",
        ),
        case(
            "meeting notes",
            TaskClass::Summarize,
            "condense this transcript into the decisions and the owners",
        ),
        case(
            "long doc",
            TaskClass::Summarize,
            "give me the gist of this 40-page RFC",
        ),
        case(
            "thread catch-up",
            TaskClass::Summarize,
            "what happened in this thread while I was away",
        ),
        case(
            "pull the numbers",
            TaskClass::Extract,
            "pull every account id out of this log",
        ),
        case(
            "table from prose",
            TaskClass::Extract,
            "turn the prices in this page into a table",
        ),
        case(
            "fields from json",
            TaskClass::Extract,
            "give me just the model and cost fields from these events",
        ),
        case(
            "dates",
            TaskClass::Extract,
            "list the dates mentioned in this changelog",
        ),
        case(
            "which model",
            TaskClass::Route,
            "which model should handle a 200k-token refactor",
        ),
        case(
            "cheap or good",
            TaskClass::Route,
            "is this worth sending to opus or should sonnet do",
        ),
        case("greeting", TaskClass::Chat, "morning! how's it going"),
        case(
            "open question",
            TaskClass::Chat,
            "what do you think about monorepos",
        ),
        case("thanks", TaskClass::Chat, "that worked, thanks"),
        // More traps: markers inside ordinary sentences, which is where a keyword heuristic earns
        // its confidence gate or loses it.
        case(
            "trap: test as a noun",
            TaskClass::Chat,
            "the driving test is on tuesday, wish me luck",
        ),
        case(
            "trap: build a habit",
            TaskClass::Chat,
            "how long does it take to build a habit",
        ),
        case(
            "trap: summarise a film",
            TaskClass::Summarize,
            "summarise the plot of this film for me",
        ),
        case(
            "trap: route as a road",
            TaskClass::Chat,
            "what's the best route from the airport",
        ),
    ]
}

/// What one classifier scored on the corpus.
///
/// `confidently_wrong` is the field that matters: docs/12 gates on confidence, so being wrong
/// is tolerable and being wrong *and trusted* is not — the router acts on it and nothing
/// downstream can tell.
#[derive(Debug, Clone)]
pub struct Score {
    pub total: usize,
    pub correct: usize,
    /// Wrong AND confident enough to be trusted — the dangerous quadrant.
    pub confidently_wrong: Vec<&'static str>,
    /// Wrong but low-confidence, so the trust gate caught it.
    pub caught_by_the_gate: Vec<&'static str>,
}

impl Score {
    pub fn accuracy(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.correct as f64 / self.total as f64
    }

    /// The scorecard lines for this classifier. Accuracy first, then the dangerous
    /// quadrant — in that order because a reader who stops after one line should have read
    /// the number that decides whether it ships.
    pub fn scorecard(&self, name: &str) -> String {
        let mut out = format!(
            "  {name}: accuracy {:.0}% ({}/{}), confidently wrong {}, gated {}\n",
            self.accuracy() * 100.0,
            self.correct,
            self.total,
            self.confidently_wrong.len(),
            self.caught_by_the_gate.len()
        );
        if !self.confidently_wrong.is_empty() {
            out.push_str(&format!(
                "    ← BLOCKING: wrong and trusted on {}\n",
                self.confidently_wrong.join(", ")
            ));
        }
        if !self.caught_by_the_gate.is_empty() {
            out.push_str(&format!(
                "    gated (wrong but not trusted): {}\n",
                self.caught_by_the_gate.join(", ")
            ));
        }
        out
    }

    /// The same run as a `Scorecard` — the artifact format every suite emits (docs/19 M19.1).
    ///
    /// A confidently-wrong case and a gated one are both failures here, but they carry different
    /// details: a scorecard that flattened them would lose the distinction the gate is built on.
    pub fn artifact(&self, subject: &str, at: &str) -> panday_types::scorecard::Scorecard {
        let mut card = panday_types::scorecard::Scorecard::new("route-bench", subject, at);
        for _ in 0..self.correct {
            card.record("", true, "");
        }
        for case in &self.confidently_wrong {
            card.record(case, false, "wrong AND trusted — the router acts on this");
        }
        for case in &self.caught_by_the_gate {
            card.record(case, false, "wrong, but below the trust threshold");
        }
        card.metric("accuracy", self.accuracy());
        card.metric("confidently_wrong", self.confidently_wrong.len() as f64);
        card.metric("caught_by_the_gate", self.caught_by_the_gate.len() as f64);
        card
    }

    /// docs/19's gate, as a function: a classifier ships only if it clears the accuracy floor
    /// and is never confidently wrong.
    pub fn meets_the_gate(&self, floor: f64) -> bool {
        self.accuracy() >= floor && self.confidently_wrong.is_empty()
    }

    /// **M19.3's gate**: "classifier ... beats heuristic on route-bench by ≥10pt".
    ///
    /// [`meets_the_gate`](Self::meets_the_gate) is an absolute floor; this is the relative one the
    /// milestone actually states, and until now it existed only as prose. A gate that is not a
    /// function is a gate nobody can fail, which is the state docs/19 M19.1 was written against.
    ///
    /// `margin_points` is **percentage points**, not a fraction: M19.3 says "≥10pt", so the call
    /// is `beats(&heuristic, 10.0)`. Accuracy is a 0..1 ratio internally and the conversion
    /// happens here, once — passing `0.10` and meaning ten points is the obvious way to get this
    /// wrong, so `the_margin_is_percentage_points_not_a_fraction` pins it.
    ///
    /// **Being confidently wrong disqualifies a challenger regardless of margin.** That is not an
    /// extra condition bolted on: the dangerous quadrant is the thing route-bench exists to
    /// measure (a wrong answer the router *trusts* and acts on), and a caller who checked only the
    /// margin would ship a model that is more accurate on average and catastrophic on the cases
    /// that matter. The incumbent's own quadrant is not consulted — the question is whether the
    /// *challenger* is safe to deploy, not whether it is less bad than what is there.
    pub fn beats(&self, incumbent: &Score, margin_points: f64) -> bool {
        let gained = (self.accuracy() - incumbent.accuracy()) * 100.0;
        gained >= margin_points && self.confidently_wrong.is_empty()
    }
}

/// Score any classifier against the corpus. `HeuristicClassifier` today; a learned one at
/// M19.3, compared on the same cases.
pub fn score(c: &dyn Classifier) -> Score {
    let mut s = Score {
        total: 0,
        correct: 0,
        confidently_wrong: Vec::new(),
        caught_by_the_gate: Vec::new(),
    };

    for k in corpus() {
        let req = request(&k);
        let (class, confidence) = c.classify(&req);
        s.total += 1;

        if class == k.expect {
            s.correct += 1;
            continue;
        }
        if confidence >= TRUST_THRESHOLD {
            s.confidently_wrong.push(k.name);
        } else {
            s.caught_by_the_gate.push(k.name);
        }
    }
    s
}

#[cfg(test)]
mod gate_tests {
    use super::Score;

    fn score(correct: usize, total: usize, confidently_wrong: Vec<&'static str>) -> Score {
        Score {
            total,
            correct,
            confidently_wrong,
            caught_by_the_gate: vec![],
        }
    }

    #[test]
    fn the_margin_is_percentage_points_not_a_fraction() {
        // M19.3 says "≥10pt". Accuracy is a 0..1 ratio internally, so the obvious mistake is to
        // pass 0.10 and mean ten points — which would let a challenger through on a *tenth* of a
        // percentage point. The unit is pinned here because the gate is unfalsifiable by
        // inspection: both readings compile and both look right.
        let incumbent = score(70, 100, vec![]);
        let exactly_ten = score(80, 100, vec![]);
        let just_under = score(79, 100, vec![]);

        assert!(
            exactly_ten.beats(&incumbent, 10.0),
            "10pt clears a 10pt bar"
        );
        assert!(!just_under.beats(&incumbent, 10.0), "9pt does not");
    }

    #[test]
    fn a_confidently_wrong_challenger_does_not_ship_however_far_ahead() {
        // The whole point of route-bench's dangerous quadrant. A model can be twenty points more
        // accurate and still be the worse thing to deploy, because the router *acts* on a
        // confident answer.
        let incumbent = score(50, 100, vec![]);
        let brilliant_but_reckless = score(90, 100, vec!["a-case-it-got-wrong-and-trusted"]);
        assert!(
            !brilliant_but_reckless.beats(&incumbent, 10.0),
            "40pt ahead, and still not shippable"
        );
    }

    #[test]
    fn losing_ground_is_not_a_pass() {
        let incumbent = score(90, 100, vec![]);
        let worse = score(60, 100, vec![]);
        assert!(!worse.beats(&incumbent, 10.0));
        // And equal is not "beats" either: the milestone asks for a margin, not parity.
        assert!(!incumbent.beats(&incumbent, 10.0));
        assert!(
            incumbent.beats(&incumbent, 0.0),
            "a zero-point bar is met by parity"
        );
    }
}
