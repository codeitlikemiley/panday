//! M12.5 — shadow-mode comparison (docs/12).
//!
//! Scoped as agreed: the `Classifier` trait is the swap-in slot, and this is the
//! harness that makes using it safe. The ONNX runtime lands with M19.3, when a
//! trained model exists to load — a native inference dependency for a model we do not
//! have would be weight without a payload.

use panday_router::classify::HeuristicClassifier;
use panday_router::shadow::{ShadowClassifier, MAX_SAMPLES};
use panday_router::Classifier;
use panday_types::id::{AccountId, RequestId};
use panday_types::model::{
    CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, TaskClass,
};

fn request(prompt: &str) -> ChatRequest {
    ChatRequest {
        model: ModelRef::auto(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: prompt.to_string(),
            }],
            call_id: None,
            provider_call_id: None,
        }],
        tools: vec![],
        sampling: Sampling::default(),
        cache: Default::default(),
        stream: false,
        metadata: CallMeta {
            account: AccountId::new(),
            request: RequestId::new(),
            session: None,
            turn: None,
            task: None,
        },
    }
}

/// A stand-in for a learned model: answers a fixed class with a fixed confidence.
struct Fixed(TaskClass, f32);

impl Classifier for Fixed {
    fn classify(&self, _req: &ChatRequest) -> (TaskClass, f32) {
        (self.0, self.1)
    }
}

/// A candidate that agrees with the heuristic — the "ready to promote" shape.
struct Mirror;

impl Classifier for Mirror {
    fn classify(&self, req: &ChatRequest) -> (TaskClass, f32) {
        HeuristicClassifier.classify(req)
    }
}

struct Panicking;

impl Classifier for Panicking {
    fn classify(&self, _req: &ChatRequest) -> (TaskClass, f32) {
        panic!("a candidate model with a bug");
    }
}

/// Stands in for docs/12's "1k replayed sessions". Prompts vary so the incumbent
/// produces a spread of classes rather than one.
fn replayed_sessions(n: usize) -> Vec<ChatRequest> {
    let shapes = [
        "fix the failing test in src/lib.rs",
        "summarise this changelog for the release notes",
        "extract the invoice totals as JSON",
        "what does this function do?",
        "refactor the parser to use a state machine",
        "which model should handle this request",
    ];
    (0..n)
        .map(|i| request(&format!("{} (session {i})", shapes[i % shapes.len()])))
        .collect()
}

#[test]
fn a_shadow_candidate_never_changes_a_route() {
    // The whole safety property. Enforced by construction — `classify` returns the
    // incumbent's answer — rather than by a flag, because "shadow mode, but live" is
    // a configuration nobody should be able to express by accident.
    let shadow = ShadowClassifier::new(HeuristicClassifier, Fixed(TaskClass::Embed, 1.0));
    for req in replayed_sessions(50) {
        let (class, _) = shadow.classify(&req);
        assert_ne!(
            class,
            TaskClass::Embed,
            "the candidate's answer reached the router"
        );
        assert_eq!(class, HeuristicClassifier.classify(&req).0);
    }
    // And it was still evaluated.
    assert_eq!(shadow.report().compared, 50);
}

#[test]
fn a_thousand_replayed_sessions_produce_a_scorecard() {
    let shadow = ShadowClassifier::new(HeuristicClassifier, Fixed(TaskClass::Code, 0.5));
    for req in replayed_sessions(1_000) {
        shadow.classify(&req);
    }
    let report = shadow.report();
    assert_eq!(report.compared, 1_000);
    assert!(report.agreed > 0, "some prompts really are code");
    assert!(!report.confusion.is_empty());

    let card = report.scorecard();
    assert!(card.contains("compared 1000"), "{card}");
    assert!(card.contains("agreement"), "{card}");
    assert!(card.contains("incumbent → candidate"), "{card}");
}

#[test]
fn the_report_carries_no_prompt_text() {
    // docs/21 T5. A "disagreement sample" holding the prompt would be the most
    // quotable content leak in the platform.
    let secret_prompt = "extract the totals from ACME-INVOICE-4417 for Jane Doe";
    let shadow = ShadowClassifier::new(HeuristicClassifier, Fixed(TaskClass::Embed, 0.9));
    shadow.classify(&request(secret_prompt));

    let report = shadow.report();
    let rendered = format!("{:?}{}", report, report.scorecard());
    assert!(!rendered.contains("ACME-INVOICE-4417"), "{rendered}");
    assert!(!rendered.contains("Jane Doe"), "{rendered}");
    // But it is joinable: a digest lets someone with log access find the original.
    assert_eq!(report.samples.len(), 1);
    assert_eq!(report.samples[0].digest.len(), 16);
}

#[test]
fn the_digest_is_stable_across_runs() {
    // Two runs over the same replayed session must produce the same digest, or this
    // week's report cannot be compared with last week's.
    let one = ShadowClassifier::new(HeuristicClassifier, Fixed(TaskClass::Embed, 0.9));
    let two = ShadowClassifier::new(HeuristicClassifier, Fixed(TaskClass::Embed, 0.9));
    one.classify(&request("summarise the changelog"));
    two.classify(&request("summarise the changelog"));
    assert_eq!(
        one.report().samples[0].digest,
        two.report().samples[0].digest
    );
}

#[test]
fn a_confident_disagreement_is_counted_separately() {
    // A candidate that is *more* confident while disagreeing is claiming to know
    // better, which is exactly what docs/12's confidence gate exists to catch.
    let bold = ShadowClassifier::new(HeuristicClassifier, Fixed(TaskClass::Embed, 1.0));
    let timid = ShadowClassifier::new(HeuristicClassifier, Fixed(TaskClass::Embed, 0.01));
    for req in replayed_sessions(20) {
        bold.classify(&req);
        timid.classify(&req);
    }
    assert!(bold.report().confident_disagreements > 0);
    assert_eq!(timid.report().confident_disagreements, 0);
}

#[test]
fn samples_are_capped() {
    let shadow = ShadowClassifier::new(HeuristicClassifier, Fixed(TaskClass::Embed, 0.9));
    for req in replayed_sessions(500) {
        shadow.classify(&req);
    }
    assert_eq!(shadow.report().samples.len(), MAX_SAMPLES);
    // The counts are not capped — only the human-readable samples are.
    assert_eq!(shadow.report().compared, 500);
}

#[test]
fn a_panicking_candidate_does_not_take_the_request_with_it() {
    // A shadow evaluation is never worth an outage.
    let shadow = ShadowClassifier::new(HeuristicClassifier, Panicking);
    let req = request("fix the failing test");
    let (class, _) = shadow.classify(&req);
    assert_eq!(class, HeuristicClassifier.classify(&req).0);
    // And it is counted as a bad disagreement rather than silently as agreement — a
    // candidate that cannot answer is not a candidate.
    let report = shadow.report();
    assert_eq!(report.agreed, 0);
    assert_eq!(report.confident_disagreements, 1);
}

#[test]
fn readiness_needs_traffic_agreement_and_humility() {
    let mirror = ShadowClassifier::new(HeuristicClassifier, Mirror);
    for req in replayed_sessions(1_000) {
        mirror.classify(&req);
    }
    let report = mirror.report();
    assert_eq!(report.agreement_rate(), 1.0);
    assert!(report.ready_for_review(1_000));
    // Not enough traffic is not ready, however good it looks.
    assert!(!report.ready_for_review(5_000));

    let bold = ShadowClassifier::new(HeuristicClassifier, Fixed(TaskClass::Embed, 1.0));
    for req in replayed_sessions(1_000) {
        bold.classify(&req);
    }
    assert!(!bold.report().ready_for_review(1_000));
}

#[test]
fn readiness_is_not_a_claim_about_accuracy() {
    // A candidate that mirrors a *wrong* incumbent agrees perfectly. Shadow mode
    // measures change; whether change is improvement needs labels, which is M12.3's
    // misclassification harness — and this test exists so nobody reads
    // `ready_for_review` as "it is better".
    struct AlwaysChat;
    impl Classifier for AlwaysChat {
        fn classify(&self, _req: &ChatRequest) -> (TaskClass, f32) {
            (TaskClass::Chat, 0.5)
        }
    }
    let shadow = ShadowClassifier::new(AlwaysChat, AlwaysChat);
    for req in replayed_sessions(1_000) {
        shadow.classify(&req);
    }
    let report = shadow.report();
    assert!(report.ready_for_review(1_000));
    assert_eq!(report.agreement_rate(), 1.0);
    // Both classifiers are useless; the report cannot tell, and does not claim to.
}
