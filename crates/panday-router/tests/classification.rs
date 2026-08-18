//! M12.3 — the misclassification harness.
//!
//! docs/12 asks for "misclassification harness with labeled fixtures". This is
//! it: a labelled corpus, a measured accuracy floor, and — the part that
//! actually matters — an assertion that the classifier's **confidence is
//! calibrated**, i.e. that it is not confidently wrong.
//!
//! A classifier that reports 0.9 for everything is worse than none at all: it
//! converts a known unknown into a silent mistake. So the corpus is scored
//! twice — once for accuracy, once for whether the errors it does make were
//! flagged low-confidence and therefore fell back (docs/12: confidence "gates
//! whether we trust it").

use panday_router::classify::{classify_or_default, HeuristicClassifier, TRUST_THRESHOLD};
use panday_router::Classifier;
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, TaskClass, ToolDef,
};

struct Case {
    name: &'static str,
    expect: TaskClass,
    prompt: &'static str,
    tools: bool,
    tool_results: bool,
}

const fn case(name: &'static str, expect: TaskClass, prompt: &'static str) -> Case {
    Case {
        name,
        expect,
        prompt,
        tools: false,
        tool_results: false,
    }
}

fn request(c: &Case) -> ChatRequest {
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
fn corpus() -> Vec<Case> {
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

struct Score {
    total: usize,
    correct: usize,
    /// Wrong AND confident enough to be trusted — the dangerous quadrant.
    confidently_wrong: Vec<&'static str>,
    /// Wrong but low-confidence, so the trust gate caught it.
    caught_by_the_gate: Vec<&'static str>,
}

fn score() -> Score {
    let c = HeuristicClassifier;
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

#[test]
fn the_classifier_is_right_more_often_than_not() {
    let s = score();
    let accuracy = s.correct as f64 / s.total as f64;
    println!(
        "accuracy {:.0}% ({}/{}), confidently wrong: {:?}, gated: {:?}",
        accuracy * 100.0,
        s.correct,
        s.total,
        s.confidently_wrong,
        s.caught_by_the_gate
    );
    // A floor, and a recorded baseline: the trained classifier (M19.3) must
    // beat the heuristic by >=10pt on route-bench.
    assert!(
        accuracy >= 0.70,
        "heuristic accuracy fell to {:.0}%",
        accuracy * 100.0
    );
}

#[test]
fn the_classifier_is_never_confidently_wrong() {
    // The property that makes a weak classifier safe to ship. Being wrong is
    // tolerable; being wrong *and* trusted is not, because the router acts on
    // it and nothing downstream can tell.
    let s = score();
    assert!(
        s.confidently_wrong.is_empty(),
        "misclassified with confidence >= {TRUST_THRESHOLD}: {:?}",
        s.confidently_wrong
    );
}

#[test]
fn a_declared_task_is_never_second_guessed() {
    let c = HeuristicClassifier;
    let mut req = request(&case("x", TaskClass::Chat, "summarize this for me"));
    req.metadata.task = Some(TaskClass::Embed);

    let (class, confidence) = c.classify(&req);
    assert_eq!(class, TaskClass::Embed);
    assert_eq!(confidence, 1.0, "a declared class is not a guess");
}

#[test]
fn low_confidence_falls_back_instead_of_guessing() {
    let c = HeuristicClassifier;
    let req = request(&case("vague", TaskClass::Chat, "ok"));

    let (class, confidence, trusted) = classify_or_default(&c, &req, TaskClass::Background);
    assert!(confidence < TRUST_THRESHOLD, "expected low confidence");
    assert!(!trusted);
    assert_eq!(
        class,
        TaskClass::Background,
        "the fallback must win when the guess is not trusted"
    );
}

#[test]
fn high_confidence_is_trusted() {
    let c = HeuristicClassifier;
    let req = request(&Case {
        tools: true,
        tool_results: true,
        ..case("loop", TaskClass::Code, "continue")
    });

    let (class, confidence, trusted) = classify_or_default(&c, &req, TaskClass::Chat);
    assert!(trusted, "confidence was {confidence}");
    assert_eq!(class, TaskClass::Code);
}

#[test]
fn confidence_never_saturates_to_certainty() {
    // A keyword heuristic must not claim near-certainty however many words
    // happen to match, or the trust gate stops meaning anything.
    let c = HeuristicClassifier;
    let stuffed = "fix the bug refactor the function compile error panic traceback \
                   cargo test fails in src/lib.rs and .ts and .py and git";
    let (_, confidence) = c.classify(&request(&case("stuffed", TaskClass::Code, stuffed)));
    assert!(
        confidence < 0.95,
        "a keyword heuristic reported {confidence} confidence"
    );
}

#[test]
fn a_large_body_with_no_instruction_reads_as_something_to_digest() {
    let c = HeuristicClassifier;
    let big: String = "some ordinary prose about nothing in particular. ".repeat(600);
    let mut req = request(&case("big", TaskClass::Summarize, "x"));
    req.messages[0].content = vec![ContentBlock::Text { text: big }];

    let (class, _) = c.classify(&req);
    assert_eq!(class, TaskClass::Summarize);
}

#[test]
fn the_corpus_covers_every_class_it_claims_to_test() {
    // A corpus missing a class silently stops testing it.
    use std::collections::BTreeSet;
    let labelled: BTreeSet<&str> = corpus()
        .iter()
        .map(|c| match c.expect {
            TaskClass::Code => "code",
            TaskClass::Summarize => "summarize",
            TaskClass::Extract => "extract",
            TaskClass::Chat => "chat",
            TaskClass::Route => "route",
            TaskClass::Embed => "embed",
            TaskClass::Background => "background",
        })
        .collect();
    for expected in ["code", "summarize", "extract", "chat"] {
        assert!(
            labelled.contains(expected),
            "corpus has no {expected} cases"
        );
    }
}
