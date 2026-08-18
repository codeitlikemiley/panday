//! Heuristic task classification (docs/12, M12.3).
//!
//! > "a heuristic classifier guesses (regex + length + tools-present rules to
//! > start). The trained classifier (19) replaces the heuristic behind the
//! > same trait ... and its **confidence** gates whether we trust it."
//!
//! Confidence is the load-bearing output, not the label. A heuristic that is
//! wrong 20% of the time is useful if it *knows* which 20%, because the router
//! can fall back to the caller's declared class or a safe default. One that
//! reports 0.9 for everything is worse than no classifier at all: it converts
//! a known unknown into a confident mistake.

use crate::Classifier;
use panday_types::model::{ChatRequest, ContentBlock, Role, TaskClass};

/// Confidence reported when two marker families both fire.
///
/// Deliberately below [`TRUST_THRESHOLD`]: a genuinely ambiguous request
/// should fall back, not be resolved by whichever family happened to match
/// one more keyword.
pub const AMBIGUOUS_CONFIDENCE: f32 = 0.5;

/// Confidence below which the caller's own class (or `Chat`) wins.
///
/// docs/12 says confidence "gates whether we trust it"; this is that gate.
pub const TRUST_THRESHOLD: f32 = 0.55;

/// v1 heuristic: cheap, debuggable, replaceable.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicClassifier;

/// Signals extracted once, so the rules below read as rules.
struct Signals {
    text: String,
    chars: usize,
    has_tools: bool,
    /// A transcript with tool results is mid-task, not a fresh question.
    has_tool_results: bool,
    turns: usize,
}

fn signals(req: &ChatRequest) -> Signals {
    let mut text = String::new();
    let mut has_tool_results = false;

    for m in &req.messages {
        if m.role == Role::Tool {
            has_tool_results = true;
        }
        for block in &m.content {
            match block {
                ContentBlock::Text { text: t } => {
                    text.push_str(t);
                    text.push('\n');
                }
                ContentBlock::ToolOutput { .. } => has_tool_results = true,
                ContentBlock::Artifact { summary, .. } => {
                    text.push_str(summary);
                    text.push('\n');
                }
            }
        }
    }

    Signals {
        chars: text.len(),
        text: text.to_lowercase(),
        has_tools: !req.tools.is_empty(),
        has_tool_results,
        turns: req.messages.len(),
    }
}

/// Markers as regexes, so each one controls its own boundaries.
///
/// Plain substring matching was the first version and it was wrong in a way
/// only a trap case reveals: `"legit good"` contains `git `, `"class
/// dismissed"` contains `class `, and `"can we fix a time"` contains `fix`.
/// Every one of those was classified as `Code` with confidence. Word
/// boundaries are not a nicety here — they are the difference between a
/// signal and a coincidence.
///
/// Markers that survive are ones whose *word* is code-specific. Genuinely
/// ambiguous verbs (`fix`, `implement`, `class`) were removed rather than
/// boundary-matched, because "fix a time" and "implement a four day week" are
/// ordinary English and no boundary saves them.
const CODE_MARKERS: &[&str] = &[
    r"\bbugs?\b",
    r"\brefactor",
    r"\bcompiles?\b",
    r"\bcompiler\b",
    r"\bpanics?\b",
    r"\btraceback\b",
    r"\bstack trace\b",
    r"\bcrash(es|ed)?\b",
    r"\bfunction\b",
    r"\bcargo\b",
    r"\bnpm\b",
    r"\bgit\b",
    r"error\[",
    r"\.rs\b",
    r"\.ts\b",
    r"\.py\b",
    r"\.go\b",
    r"\bsrc/",
    r"failing tests?\b",
    r"tests? fails?\b",
    // Bare "test" is code-specific enough in this context, and it is what
    // makes "list all the tests that fail" register as ambiguous (extract by
    // shape, code by subject) instead of being confidently called Extract.
    r"\btests?\b",
    r"\bnot compile\b",
];

const SUMMARIZE_MARKERS: &[&str] = &[
    r"\bsummari[sz]e\b",
    r"\btl;?dr\b",
    r"\bkey points\b",
    r"\bcondense\b",
    r"\bin short\b",
    r"\bbrief overview\b",
];

const EXTRACT_MARKERS: &[&str] = &[
    r"\bextract\b",
    r"\blist all\b",
    r"\bpull out\b",
    r"\bas json\b",
    r"\bjson schema\b",
    r"\breturn only\b",
    r"\bfields:",
    r"\btable of\b",
];

/// Compiled once. A classifier on the request path must not recompile regexes.
fn compiled(markers: &'static [&'static str]) -> &'static Vec<regex::Regex> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<usize, &'static Vec<regex::Regex>>>> = OnceLock::new();

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = markers.as_ptr() as usize;
    let mut guard = cache.lock().unwrap();
    guard.entry(key).or_insert_with(|| {
        let compiled: Vec<regex::Regex> = markers
            .iter()
            .map(|m| regex::Regex::new(m).expect("marker regexes are compile-time constants"))
            .collect();
        Box::leak(Box::new(compiled))
    })
}

fn hits(text: &str, markers: &'static [&'static str]) -> usize {
    compiled(markers)
        .iter()
        .filter(|re| re.is_match(text))
        .count()
}

impl Classifier for HeuristicClassifier {
    fn classify(&self, req: &ChatRequest) -> (TaskClass, f32) {
        let s = signals(req);

        // A caller that already said is not guessed at. This is the highest
        // confidence available because it is not a guess.
        if let Some(declared) = req.metadata.task {
            return (declared, 1.0);
        }

        let code = hits(&s.text, CODE_MARKERS);
        let summarize = hits(&s.text, SUMMARIZE_MARKERS);
        let extract = hits(&s.text, EXTRACT_MARKERS);

        // Tools present + tool results already in the transcript is the
        // strongest signal in the whole heuristic: something is *running* a
        // task, not asking a question.
        if s.has_tools && s.has_tool_results {
            return (TaskClass::Code, 0.85);
        }

        // When two families both fire, the request is genuinely ambiguous —
        // "tldr on why the cargo build broke" is honestly both. Reporting the
        // stronger one *confidently* would be a guess dressed as a fact, so
        // the winner is returned BELOW the trust gate and the caller's
        // declared class (or the default) wins instead. This is the whole
        // reason confidence is part of the trait.
        let families = [code, summarize, extract]
            .iter()
            .filter(|n| **n > 0)
            .count();
        if families > 1 {
            let (class, _) = [
                (TaskClass::Code, code),
                (TaskClass::Summarize, summarize),
                (TaskClass::Extract, extract),
            ]
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .expect("non-empty");
            return (class, AMBIGUOUS_CONFIDENCE);
        }

        // An explicit ask wins over shape.
        if extract > 0 {
            return (TaskClass::Extract, confidence_from(extract, 0.6));
        }
        if summarize > 0 {
            // A summarise request over a large body is nearly certain; over
            // one line it is probably someone saying "in short" in passing.
            let base = if s.chars > 4_000 { 0.85 } else { 0.62 };
            return (TaskClass::Summarize, base);
        }
        if code > 0 {
            return (TaskClass::Code, confidence_from(code, 0.58));
        }

        // Tool schemas offered but nothing said yet: an agent loop is starting.
        if s.has_tools {
            return (TaskClass::Code, 0.6);
        }

        // A very large body with no instruction reads as something to digest.
        if s.chars > 20_000 {
            return (TaskClass::Summarize, 0.6);
        }

        // Nothing matched. Report Chat at LOW confidence rather than pretending
        // — this is the case the trust gate exists for.
        let confidence = if s.turns <= 1 { 0.45 } else { 0.4 };
        (TaskClass::Chat, confidence)
    }
}

/// More independent markers → more confidence, saturating.
///
/// Saturating matters: twelve keyword hits does not make a heuristic certain,
/// and letting it claim 0.99 would defeat the trust gate.
fn confidence_from(hit_count: usize, base: f32) -> f32 {
    let bonus = (hit_count.min(4) as f32 - 1.0) * 0.08;
    (base + bonus).min(0.88)
}

/// Apply the trust gate: keep the guess, or fall back.
///
/// Returns the class the router should use and whether the guess was trusted,
/// so a caller can log the difference (M12.5's shadow comparison needs it).
pub fn classify_or_default(
    classifier: &dyn Classifier,
    req: &ChatRequest,
    fallback: TaskClass,
) -> (TaskClass, f32, bool) {
    let (class, confidence) = classifier.classify(req);
    if confidence >= TRUST_THRESHOLD {
        (class, confidence, true)
    } else {
        (fallback, confidence, false)
    }
}
