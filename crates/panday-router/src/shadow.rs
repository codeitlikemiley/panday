//! Shadow mode: running a candidate classifier next to the incumbent (M12.5,
//! docs/12).
//!
//! > "ONNX classifier slot behind `Classifier` trait; shadow-mode comparison report
//! > (heuristic vs learned) over 1k replayed sessions."
//!
//! The `Classifier` trait is already the slot — a learned model implements it and
//! nothing above changes. What was missing is the part that makes swapping one safe:
//! a way to run the candidate against real traffic **without letting it route
//! anything**, and a report that says whether it is actually better.
//!
//! ## Why shadow mode and not an A/B split
//!
//! An A/B split sends some fraction of real requests to the candidate, which means
//! its mistakes reach users and its wins are measured on different traffic than its
//! losses. Shadow mode runs both on *the same* request and keeps the incumbent's
//! answer, so every disagreement is a like-for-like comparison and a bad candidate
//! costs nothing but CPU. docs/19's gate ("a model enters routing pools only with a
//! scorecard") is what this produces the scorecard for.
//!
//! ## What the report may contain
//!
//! Counts, classes and confidences — never prompt text. docs/21 T5 keeps content out
//! of anything that gets shipped to a dashboard, and a "disagreement sample" holding
//! the prompt would be the most quotable content leak in the platform. Disagreements
//! are identified by a stable hash so a human with log access can find the original,
//! which is the property that makes the report actionable without making it a leak.

use crate::Classifier;
use panday_types::model::{ChatRequest, TaskClass};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// One disagreement, content-free.
#[derive(Debug, Clone, PartialEq)]
pub struct Disagreement {
    /// Stable hash of the classified text — the join key to the log, not the text.
    pub digest: String,
    pub incumbent: (TaskClass, f32),
    pub candidate: (TaskClass, f32),
}

/// Accumulated comparison.
#[derive(Debug, Clone, Default)]
pub struct ShadowReport {
    pub compared: u64,
    pub agreed: u64,
    /// `(incumbent, candidate) -> count`, so the shape of the disagreement is
    /// visible rather than only its size: a candidate that turns `chat` into `code`
    /// is a different problem from one that turns `code` into `chat`.
    pub confusion: BTreeMap<(TaskClass, TaskClass), u64>,
    /// Capped — a report is read by a person, and an unbounded list of samples is
    /// a memory leak that also nobody reads.
    pub samples: Vec<Disagreement>,
    /// Disagreements where the candidate was *more* confident than the incumbent.
    /// The dangerous ones: a confident wrong answer is what docs/12's confidence
    /// gate exists to catch, and a candidate that is confidently different is
    /// claiming to know better.
    pub confident_disagreements: u64,
}

pub const MAX_SAMPLES: usize = 64;

impl ShadowReport {
    pub fn agreement_rate(&self) -> f64 {
        if self.compared == 0 {
            return 0.0;
        }
        self.agreed as f64 / self.compared as f64
    }

    /// Whether a candidate looks ready to promote, by the only criteria this report
    /// can support: it has seen enough traffic, it mostly agrees, and where it
    /// disagrees it is not confidently overriding the incumbent.
    ///
    /// Deliberately **not** "is it more accurate" — that needs labels, which live in
    /// the misclassification harness (M12.3). Shadow mode measures *change*, and
    /// change is not improvement until someone scores it.
    pub fn ready_for_review(&self, min_compared: u64) -> bool {
        self.compared >= min_compared
            && self.agreement_rate() >= 0.9
            && self.confident_disagreements * 20 <= self.compared.max(1)
    }

    pub fn scorecard(&self) -> String {
        let mut out = String::from("shadow-mode comparison (M12.5)\n\n");
        out.push_str(&format!(
            "  compared {} · agreement {:.1}% · confident disagreements {}\n",
            self.compared,
            self.agreement_rate() * 100.0,
            self.confident_disagreements
        ));
        if !self.confusion.is_empty() {
            out.push_str("\n  incumbent → candidate            count\n");
            for ((from, to), n) in &self.confusion {
                out.push_str(&format!(
                    "  {:12} → {:12} {n:>8}\n",
                    from.as_str(),
                    to.as_str()
                ));
            }
        }
        out.push_str(&format!(
            "\n  {} sample(s) retained, by digest — the prompts stay in the log \
             (docs/21 T5)\n",
            self.samples.len()
        ));
        out
    }
}

/// Runs `candidate` alongside `incumbent` and returns the **incumbent's** answer.
///
/// The return value is the whole safety property: a shadow classifier that could
/// change a route would not be a shadow. It is enforced by construction here rather
/// than by a flag someone can flip, because "shadow mode, but live" is a
/// configuration nobody should be able to express by accident.
pub struct ShadowClassifier<I, C> {
    incumbent: I,
    candidate: C,
    report: Mutex<ShadowReport>,
}

impl<I: Classifier, C: Classifier> ShadowClassifier<I, C> {
    pub fn new(incumbent: I, candidate: C) -> Self {
        Self {
            incumbent,
            candidate,
            report: Mutex::new(ShadowReport::default()),
        }
    }

    pub fn report(&self) -> ShadowReport {
        self.report.lock().unwrap().clone()
    }
}

/// Stable content-free identifier for a classified request.
///
/// sha256 of the text the classifier saw. Not a random id: two runs over the same
/// replayed session must produce the same digest, or a report cannot be compared
/// with the one from last week.
fn digest(req: &ChatRequest) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for m in &req.messages {
        for b in &m.content {
            match b {
                panday_types::model::ContentBlock::Text { text } => h.update(text.as_bytes()),
                panday_types::model::ContentBlock::ToolOutput { text, .. } => {
                    h.update(text.as_bytes())
                }
                panday_types::model::ContentBlock::Artifact { summary, .. } => {
                    h.update(summary.as_bytes())
                }
            }
        }
    }
    format!("{:x}", h.finalize())[..16].to_string()
}

impl<I: Classifier, C: Classifier> Classifier for ShadowClassifier<I, C> {
    fn classify(&self, req: &ChatRequest) -> (TaskClass, f32) {
        let live = self.incumbent.classify(req);

        // A candidate that panics must not take the request with it: it is not in
        // the serving path, and a shadow evaluation is never worth an outage.
        let shadow = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.candidate.classify(req)
        }));

        let mut report = self.report.lock().unwrap();
        report.compared += 1;
        match shadow {
            Ok(shadow) => {
                if shadow.0 == live.0 {
                    report.agreed += 1;
                } else {
                    *report.confusion.entry((live.0, shadow.0)).or_insert(0) += 1;
                    if shadow.1 > live.1 {
                        report.confident_disagreements += 1;
                    }
                    if report.samples.len() < MAX_SAMPLES {
                        report.samples.push(Disagreement {
                            digest: digest(req),
                            incumbent: live,
                            candidate: shadow,
                        });
                    }
                }
            }
            Err(_) => {
                // Counted as a disagreement of the worst kind: a candidate that
                // cannot answer is not a candidate, and silently ignoring the panic
                // would make it look like perfect agreement.
                *report
                    .confusion
                    .entry((live.0, TaskClass::Chat))
                    .or_insert(0) += 1;
                report.confident_disagreements += 1;
            }
        }

        live
    }
}
