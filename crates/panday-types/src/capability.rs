//! What a model can actually do (docs/18 §degraded-capability honesty, docs/12 §offline).
//!
//! > "Local models are worse. Pretending otherwise ruins trust in the product. The router
//! > returns a `CapabilityProfile` (max context, JSON reliability, tool-call reliability,
//! > no vision) and the harness **adapts** ... Profiles are per-model entries in the
//! > catalog, measured by the eval suite — not vibes."
//!
//! ## Measured or declared, never blurred
//!
//! The spec's last four words are the design constraint, so every profile carries
//! [`Provenance`]. A declared profile is somebody's estimate; a measured one has a
//! scorecard behind it (docs/19). They are allowed to be equally *useful* and are never
//! allowed to look the same: the system prompt says "estimated" when it is, because a model
//! told "your JSON reliability is 0.7" by a number nobody measured is being lied to
//! precisely.
//!
//! This lives in `panday-types` because three components read it — the router returns it,
//! the harness adapts to it, the catalog stores it — and a type owned by any one of them
//! would make the other two depend on it sideways.

use serde::{Deserialize, Serialize};

/// Where a profile's numbers came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Measured by the eval suite; the scorecard is the evidence (docs/19 M19.2).
    Measured,
    /// Somebody's estimate. Honest, useful, and clearly labelled as such.
    Declared,
}

/// How reliably a model does something, in [0,1].
///
/// A float rather than an enum because the adaptations are threshold decisions and the
/// thresholds belong to the caller: `docs/18`'s "tool schemas shrink to the minimal set" is
/// one rule reading this, and a coarse Low/Medium/High would force every rule to share one
/// person's cut points.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CapabilityProfile {
    /// The model's usable context, in tokens. Not the advertised number — the one the
    /// eval suite still gets coherent answers at.
    pub max_context_tokens: u32,
    /// Emits schema-valid JSON on demand.
    pub json_reliability: f32,
    /// Calls tools with well-formed arguments, and stops when it should.
    pub tool_reliability: f32,
    pub vision: bool,
    /// Sub-agent fan-out this model can supervise without losing the thread.
    pub max_subagents: u8,
    pub provenance: Provenance,
}

impl CapabilityProfile {
    /// A frontier cloud model: everything works, and this is the baseline the others are
    /// worse than.
    pub fn frontier() -> Self {
        Self {
            max_context_tokens: 200_000,
            json_reliability: 0.99,
            tool_reliability: 0.98,
            vision: true,
            max_subagents: 4,
            provenance: Provenance::Declared,
        }
    }

    /// A small local model (4B-class), as shipped in docs/18's default catalog.
    ///
    /// `Declared`: nobody has run the eval suite against it yet (M19.2), and the numbers
    /// being pessimistic is not the same as their being measured.
    pub fn small_local() -> Self {
        Self {
            // Advertised context is usually far larger; this is where a 4B model stops
            // following a long transcript, which is the number that matters.
            max_context_tokens: 16_000,
            json_reliability: 0.7,
            tool_reliability: 0.6,
            vision: false,
            // One at a time. A model that loses the thread supervising itself will lose it
            // faster supervising three others.
            max_subagents: 0,
            provenance: Provenance::Declared,
        }
    }

    /// Whether the harness should shrink the tool set for this model.
    ///
    /// docs/18: "tool schemas shrink to the minimal set". A model that gets tool calls
    /// wrong four times in ten does worse with twelve tools than with four — the schemas
    /// themselves are what it is failing to read.
    pub fn wants_minimal_tools(&self) -> bool {
        self.tool_reliability < 0.8
    }

    /// Whether the reducer should run aggressively.
    ///
    /// Not "is this a local model" but "is this context small": the reducer's job is to fit
    /// the work into the window, and a small window is a small window whoever serves it.
    pub fn wants_aggressive_reduction(&self) -> bool {
        self.max_context_tokens < 64_000
    }

    /// Whether a task should carry docs/18's "this will be slower/rougher locally" notice.
    pub fn wants_expectation_notice(&self) -> bool {
        self.json_reliability < 0.9 || self.tool_reliability < 0.9
    }

    /// The lines the system prompt gains, or nothing for a model with no constraints worth
    /// stating.
    ///
    /// Phrased as facts about the model rather than instructions about behaviour: "you
    /// cannot see images" is checkable, while "be careful with JSON" is advice a model
    /// cannot act on. And it says **estimated** when the profile is declared, because a
    /// number nobody measured should not be quoted as one that was.
    pub fn prompt_constraints(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        let hedge = match self.provenance {
            Provenance::Measured => "",
            Provenance::Declared => " (estimated)",
        };

        // Only when the window is tight enough to change behaviour. Telling a frontier
        // model it has 200k tokens is a sentence paid for on every turn of every session to
        // say nothing — and a constraints section full of non-constraints teaches the
        // reader to skip it, which is how the real constraints get missed.
        if self.wants_aggressive_reduction() {
            lines.push(format!(
                "Your usable context is about {} tokens{hedge}. Long transcripts are \
                 summarised before they reach you; ask for what you need rather than \
                 assuming you can still see it.",
                self.max_context_tokens
            ));
        }
        if !self.vision {
            lines.push("You cannot see images. Ask for a text description instead.".into());
        }
        if self.json_reliability < 0.9 {
            lines.push(
                "Your structured output is unreliable enough that it is checked and \
                 retried. Prefer the simplest schema that answers the question."
                    .into(),
            );
        }
        if self.tool_reliability < 0.9 {
            lines.push(
                "Tool calls are validated before they run, and a malformed one comes \
                 back to you as an error. One tool at a time is more likely to work \
                 than several."
                    .into(),
            );
        }
        if self.max_subagents == 0 {
            lines.push("You cannot spawn sub-agents; do the work yourself.".into());
        }

        if lines.is_empty() {
            return String::new();
        }
        format!("\n\n# What you are\n\n{}", lines.join("\n"))
    }
}
