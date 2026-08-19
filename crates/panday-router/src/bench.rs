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

    /// docs/19's gate, as a function: a classifier ships only if it clears the accuracy floor
    /// and is never confidently wrong.
    pub fn meets_the_gate(&self, floor: f64) -> bool {
        self.accuracy() >= floor && self.confidently_wrong.is_empty()
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
