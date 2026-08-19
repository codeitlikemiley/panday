//! M12.3 — the misclassification harness.
//!
//! The corpus and the scoring moved into `panday_router::bench` at M12.4: an eval that lives
//! only inside a `#[test]` can be checked but never reported, and docs/12's weekly review is
//! a report. This file is now one caller of that library; `cargo xtask scorecard` is the
//! other, and both see the same numbers by construction.
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

use panday_router::bench::{case, corpus, request, score, Case};
use panday_router::classify::{classify_or_default, HeuristicClassifier, TRUST_THRESHOLD};
use panday_router::Classifier;
use panday_types::model::{ContentBlock, TaskClass};

#[test]
fn the_classifier_is_right_more_often_than_not() {
    let s = score(&HeuristicClassifier);
    let accuracy = s.accuracy();
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
    let s = score(&HeuristicClassifier);
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
