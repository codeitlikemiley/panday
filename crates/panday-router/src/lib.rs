//! # panday-router
//!
//! Policy-first model routing (docs/12-router.md). This seed defines the
//! decision types and traits; the YAML policy engine is M12.1.

use panday_types::model::{ChatRequest, ModelRef, TaskClass};
use serde::{Deserialize, Serialize};

/// What the router needs to decide (docs/12 §inputs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteQuery {
    pub requested: ModelRef,
    pub task: Option<TaskClass>,
    pub context_tokens: u32,
    pub needs: Caps,
    pub plan: String,
    pub privacy_strict: bool,
    pub budget_pressure: BudgetPressure,
    pub offline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPressure {
    #[default]
    Normal,
    /// >80% of ceiling: demotions may apply.
    Soft,
    /// Ceiling hit: only free/local pools admissible.
    Hard,
}

/// Capabilities a request needs / a model offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Caps {
    pub tools: bool,
    pub vision: bool,
    pub json_strict: bool,
    pub min_context: u32,
}

/// The decision: an ordered failover chain, never a single target
/// (docs/11 §failover), plus the audit trail fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteDecision {
    pub chain: Vec<ModelRef>,
    pub matched_rule: String,
    pub pool: String,
    /// Filled in shadow mode: what the learned policy would have picked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterfactual: Option<Vec<ModelRef>>,
}

#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error("no pool admits this request (task {task:?}, offline {offline})")]
    NoRoute {
        task: Option<TaskClass>,
        offline: bool,
    },
    #[error("policy invalid: {0}")]
    Policy(String),
}

pub mod classify;
pub mod policy;

pub use classify::{classify_or_default, TRUST_THRESHOLD};
pub use policy::{Policy, PolicyRouter};

pub trait Router: Send + Sync {
    fn route(&self, q: &RouteQuery) -> Result<RouteDecision, RouteError>;
}

/// Task classification (heuristic v1 → trained ModernBERT-class model at
/// M12.5, docs/19 §19.5 Model 1) behind one trait so the swap is invisible.
pub trait Classifier: Send + Sync {
    /// Returns the class and a confidence in [0,1]; low confidence falls
    /// back to the caller-provided class or `Chat`.
    fn classify(&self, req: &ChatRequest) -> (TaskClass, f32);
}

pub mod bench;
pub mod shadow;

pub use classify::HeuristicClassifier;
pub use shadow::{Disagreement, ShadowClassifier, ShadowReport};
