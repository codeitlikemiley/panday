//! The scorecard artifact (docs/19 M19.1).
//!
//! > "a model (base or tuned, cloud or GGUF) enters routing pools only with a scorecard"
//!
//! A gate is only a gate if something can read it without a human. So the artifact is JSON with a
//! version on it, and markdown is a *rendering* of that JSON rather than the thing itself — a gate
//! that parses a table out of prose breaks the first time somebody improves the wording.
//!
//! **No clock inside.** `at` is supplied by the caller. A type that read the system clock would
//! make every scorecard a different byte string, which breaks both golden-file tests and the
//! "regenerate and diff" review this repo uses everywhere else.
//!
//! **Failures are carried, capped, and counted.** An artifact that lists nothing is unusable for
//! debugging; one that lists ten thousand is unusable for reading. It keeps the first `MAX_FAILURES`
//! and says how many it did not keep, which is honest in both directions.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How many individual failures an artifact carries.
pub const MAX_FAILURES: usize = 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Scorecard {
    /// Format version. Bump rules follow docs/03: additive fields are always ok.
    pub version: u32,
    /// `route-bench`, `json-bench`, `reduce-bench`, `agent-bench`.
    pub suite: String,
    /// What was measured: a model id, or a classifier name.
    pub subject: String,
    /// RFC 3339 UTC, from the caller.
    pub at: String,
    pub cases: u32,
    pub passed: u32,
    /// Suite-specific numbers, sorted so two runs of the same suite diff cleanly.
    #[serde(default)]
    pub metrics: BTreeMap<String, f64>,
    #[serde(default)]
    pub failures: Vec<Failure>,
    /// Failures beyond `MAX_FAILURES`. Present so a short list is never mistaken for a short tail.
    #[serde(default)]
    pub failures_omitted: u32,
    /// What produced this, so a number can be traced back to a build (docs/19 §gates).
    #[serde(default)]
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Failure {
    pub case: String,
    /// What went wrong, in one line. Never the model's full output — a scorecard is an artifact
    /// people commit, and a transcript in it is a content-retention decision nobody made.
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Provenance {
    /// Git commit of the tree that produced this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The quantization the subject was measured at.
    ///
    /// docs/19 §gates: "quantize → then eval; quality dies at the quant step, so that's where you
    /// measure". A scorecard that does not say which quantization it measured is a number that
    /// cannot be compared to the artifact anyone actually runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl Scorecard {
    pub fn new(suite: &str, subject: &str, at: &str) -> Self {
        Self {
            version: 1,
            suite: suite.into(),
            subject: subject.into(),
            at: at.into(),
            cases: 0,
            passed: 0,
            metrics: BTreeMap::new(),
            failures: Vec::new(),
            failures_omitted: 0,
            provenance: Provenance::default(),
        }
    }

    /// Record one outcome.
    pub fn record(&mut self, case: &str, passed: bool, detail: impl Into<String>) {
        self.cases += 1;
        if passed {
            self.passed += 1;
            return;
        }
        if self.failures.len() < MAX_FAILURES {
            self.failures.push(Failure {
                case: case.into(),
                detail: detail.into(),
            });
        } else {
            self.failures_omitted += 1;
        }
    }

    pub fn metric(&mut self, name: &str, value: f64) -> &mut Self {
        self.metrics.insert(name.into(), value);
        self
    }

    /// Pass rate in [0,1]. An empty run scores zero rather than one: "nothing ran" must never read
    /// as "everything passed", which is the failure mode of every gate that divides by a count.
    pub fn rate(&self) -> f64 {
        if self.cases == 0 {
            return 0.0;
        }
        self.passed as f64 / self.cases as f64
    }

    /// The gate: does this clear a floor?
    pub fn meets(&self, floor: f64) -> bool {
        self.cases > 0 && self.rate() >= floor
    }

    /// **The relative gate**: did this run beat an earlier one on the same suite?
    ///
    /// [`meets`](Self::meets) answers "is it good enough"; several milestones ask "is it *better*"
    /// and had no function to ask with — M19.7 ("SFT stage beats base on agent-bench") is the
    /// live one. A gate that exists only in prose is a gate nobody can fail.
    ///
    /// **`None` means the two cannot be compared, and that is the point of the return type.**
    /// Comparing a tuned run over ten tasks against a base run over forty-one is the way this
    /// measurement gets faked without anyone lying: both numbers are real, the ratio is
    /// meaningless, and a `bool` would have hidden it. Refused when the suites differ, when either
    /// ran nothing, or when the case counts differ — a shorter run is a different corpus, not a
    /// better model. A caller that writes `.unwrap_or(false)` fails closed, which is the right
    /// default for a shipping decision.
    ///
    /// Strictly greater, with no margin: M19.7 says "beats", and a tie is not one. Where a
    /// milestone names a margin instead — M19.3's "≥10pt" — that gate lives with its suite and
    /// takes the margin explicitly, because the unit is the suite's business, not this type's.
    pub fn beats(&self, base: &Scorecard) -> Option<bool> {
        if self.suite != base.suite || self.cases == 0 || base.cases == 0 {
            return None;
        }
        if self.cases != base.cases {
            return None;
        }
        Some(self.passed > base.passed)
    }

    /// The human rendering. Derived from the artifact, never the other way round.
    pub fn to_markdown(&self) -> String {
        let mut out = format!(
            "# {} — {}\n\n{}/{} passed ({:.0}%) · {}\n",
            self.suite,
            self.subject,
            self.passed,
            self.cases,
            self.rate() * 100.0,
            self.at
        );
        if let Some(q) = &self.provenance.quantization {
            out.push_str(&format!("quantization: {q}\n"));
        }
        if let Some(c) = &self.provenance.commit {
            out.push_str(&format!("commit: {c}\n"));
        }
        if !self.metrics.is_empty() {
            out.push_str("\n| metric | value |\n|---|---|\n");
            for (name, value) in &self.metrics {
                out.push_str(&format!("| {name} | {value:.3} |\n"));
            }
        }
        if !self.failures.is_empty() {
            out.push_str("\n## failures\n\n");
            for failure in &self.failures {
                out.push_str(&format!("- **{}** — {}\n", failure.case, failure.detail));
            }
            if self.failures_omitted > 0 {
                out.push_str(&format!("- …and {} more\n", self.failures_omitted));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_run_scores_zero_not_one() {
        // The failure mode of every gate that divides by a count: a suite that failed to run at
        // all reports 100% and everything downstream believes it.
        let card = Scorecard::new("json-bench", "local/qwen3.5-4b", "2026-08-19T00:00:00Z");
        assert_eq!(card.rate(), 0.0);
        assert!(!card.meets(0.0), "a gate must not pass on an empty run");
    }

    #[test]
    fn failures_are_capped_and_the_remainder_is_counted() {
        // A short list must never be mistaken for a short tail.
        let mut card = Scorecard::new("json-bench", "m", "2026-08-19T00:00:00Z");
        for i in 0..MAX_FAILURES + 7 {
            card.record(&format!("case-{i}"), false, "invalid json");
        }
        assert_eq!(card.failures.len(), MAX_FAILURES);
        assert_eq!(card.failures_omitted, 7);
        assert_eq!(card.cases, (MAX_FAILURES + 7) as u32);
        assert!(card.to_markdown().contains("…and 7 more"));
    }

    #[test]
    fn the_artifact_round_trips() {
        // It is JSON first: something machine-read has to survive a write and a read unchanged.
        let mut card = Scorecard::new("route-bench", "heuristic", "2026-08-19T00:00:00Z");
        card.record("a", true, "");
        card.record("b", false, "expected code, got chat");
        card.metric("confidently_wrong", 0.0);
        card.provenance.quantization = Some("Q4_K_M".into());

        let json = serde_json::to_string(&card).unwrap();
        let back: Scorecard = serde_json::from_str(&json).unwrap();
        assert_eq!(back, card);
    }

    #[test]
    fn the_same_run_produces_the_same_bytes() {
        // No clock inside: a scorecard that timestamped itself would diff every time it was
        // regenerated, and a file that always diffs is one nobody reviews.
        let build = || {
            let mut card = Scorecard::new("json-bench", "m", "2026-08-19T00:00:00Z");
            card.record("a", true, "");
            card.metric("schema_validity", 0.97);
            serde_json::to_string(&card).unwrap()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn markdown_is_a_rendering_of_the_artifact() {
        let mut card = Scorecard::new("json-bench", "local/qwen3.5-4b", "2026-08-19T00:00:00Z");
        card.record("nested object", false, "missing required field `city`");
        card.record("flat object", true, "");
        card.provenance.quantization = Some("Q4_K_M".into());

        let md = card.to_markdown();
        assert!(md.contains("1/2 passed (50%)"));
        assert!(md.contains("quantization: Q4_K_M"));
        assert!(md.contains("missing required field `city`"));
    }
}

#[cfg(test)]
mod relative_gate_tests {
    use super::Scorecard;

    fn card(suite: &str, passed: u32, cases: u32) -> Scorecard {
        let mut c = Scorecard::new(suite, "subject", "2026-08-24T00:00:00Z");
        for i in 0..cases {
            c.record(&format!("case{i}"), i < passed, "");
        }
        c
    }

    #[test]
    fn a_shorter_run_is_not_a_better_model() {
        // The failure this return type exists for. Both numbers are real, the comparison is not:
        // 9/10 against 30/41 is a different corpus, not an improvement. A `bool` would have said
        // "better" and nobody would have noticed.
        let base = card("agent-bench", 30, 41);
        let tuned_on_fewer = card("agent-bench", 9, 10);
        assert_eq!(tuned_on_fewer.beats(&base), None);
    }

    #[test]
    fn suites_do_not_cross() {
        let a = card("agent-bench", 40, 41);
        let b = card("route-bench", 10, 41);
        assert_eq!(a.beats(&b), None, "same case count, different question");
    }

    #[test]
    fn nothing_ran_is_not_a_win() {
        let base = card("agent-bench", 0, 0);
        let tuned = card("agent-bench", 5, 5);
        assert_eq!(tuned.beats(&base), None);
        assert_eq!(base.beats(&tuned), None);
    }

    #[test]
    fn beating_is_strict() {
        let base = card("agent-bench", 30, 41);
        assert_eq!(card("agent-bench", 31, 41).beats(&base), Some(true));
        assert_eq!(
            card("agent-bench", 30, 41).beats(&base),
            Some(false),
            "a tie is not a win"
        );
        assert_eq!(card("agent-bench", 29, 41).beats(&base), Some(false));
    }

    #[test]
    fn an_incomparable_pair_fails_closed_for_a_careless_caller() {
        // `.unwrap_or(false)` is what a caller will write. It must mean "do not ship".
        let base = card("agent-bench", 30, 41);
        let mismatched = card("agent-bench", 41, 42);
        assert!(!mismatched.beats(&base).unwrap_or(false));
    }
}
