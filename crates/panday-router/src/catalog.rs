//! The model catalog (M12.2).
//!
//! Pools in a policy file are *patterns* — `anthropic/claude-opus-*` — because a policy should
//! outlive a model release. Something has to turn a pattern into models that exist, and that
//! something is this: an ordered list of every model the deployment knows about, with the two facts
//! the router can legitimately act on (usable context, vision) and the prices the meter and the
//! ledger need.
//!
//! **Order is preference.** Expanding a glob yields catalog order, so the failover chain a pool
//! produces is the order somebody wrote down rather than whatever a hash map felt like. Rewriting
//! the file reorders failover; that is the intended control.
//!
//! **What the router does not filter on.** `json_reliability` and `tool_reliability` are in the
//! profile and are deliberately *not* admission criteria. They are soft numbers the harness adapts
//! to (docs/18: "the model is told what it is"), and a router that dropped a 0.6-tool-reliability
//! model from the chain would make every local-only deployment unroutable the moment a request
//! carried a tool. Hard limits — the context it cannot hold, the image it cannot see — are
//! different in kind, and those are enforced.

use panday_types::capability::{CapabilityProfile, Provenance};
use panday_types::model::ModelRef;
use panday_types::pricing::{PriceTable, Pricing};
use serde::{Deserialize, Serialize};

/// One model the deployment can actually call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// `provider/model`, exactly as an adapter expects it.
    pub id: String,
    #[serde(flatten)]
    pub profile: ProfileSpec,
    /// Absent means *unpriced*, which is not the same claim as free
    /// (`panday_types::pricing::CostModel`). A local model gets an explicit zero price; a model
    /// whose price nobody has filled in gets none, and the meter counts it as unpriced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<PriceSpec>,
    /// Still callable by exact name, never expanded into by a glob.
    ///
    /// Deprecating is how a model leaves a pool without breaking the pinned requests that still name
    /// it — deleting the row would turn those into `ModelUnavailable` on the next deploy.
    #[serde(default)]
    pub deprecated: bool,
}

/// The capability numbers, in the file's own vocabulary.
///
/// Flattened into the entry so a catalog row reads as one thing. Kept separate from
/// `CapabilityProfile` because that type is the protocol's, and a file format that is the same type
/// as a wire type cannot be changed without changing both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileSpec {
    /// The context the model is *usable* at, not the advertised number (docs/19 M19.2).
    pub context: u32,
    #[serde(default)]
    pub json: f32,
    #[serde(default)]
    pub tools: f32,
    #[serde(default)]
    pub vision: bool,
    #[serde(default = "one")]
    pub max_subagents: u8,
    /// `declared` until the eval suite has run against it. Honest, and clearly labelled.
    #[serde(default = "declared")]
    pub provenance: Provenance,
}

fn one() -> u8 {
    1
}
fn declared() -> Provenance {
    Provenance::Declared
}

/// Prices in micro-dollars per million tokens — integers, like everywhere else money appears.
///
/// Not dollars-as-floats: a catalog is edited by hand, and `3.0` parsing to something that is not
/// exactly three is the class of bug that shows up as a cent of drift a month later.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PriceSpec {
    pub input_per_mtok_micros: u64,
    pub output_per_mtok_micros: u64,
    #[serde(default = "ten")]
    pub cache_read_pct: u32,
    #[serde(default = "hundred")]
    pub cache_write_pct: u32,
    #[serde(default = "hundred")]
    pub cache_write_1h_pct: u32,
}

fn ten() -> u32 {
    10
}
fn hundred() -> u32 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub version: u32,
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error("catalog parse: {0}")]
    Parse(String),
    #[error("catalog version {0} is not supported")]
    Version(u32),
    #[error("{0}")]
    Invalid(String),
}

/// The catalog shipped with the binary: the models docs/12's default policy names.
pub const DEFAULT_CATALOG: &str = include_str!("../catalog/default.yaml");

impl ModelCatalog {
    pub fn from_yaml(src: &str) -> Result<Self, CatalogError> {
        let catalog: ModelCatalog =
            serde_yaml_ng::from_str(src).map_err(|e| CatalogError::Parse(e.to_string()))?;
        if catalog.version != 1 {
            return Err(CatalogError::Version(catalog.version));
        }
        catalog.validate()?;
        Ok(catalog)
    }

    /// The shipped default. Panics on a malformed file, which is a build-time mistake: the file is
    /// compiled in, so if it is wrong every deployment is wrong and a test says so first.
    pub fn shipped() -> Self {
        Self::from_yaml(DEFAULT_CATALOG).expect("the shipped catalog must parse")
    }

    /// An empty catalog: every glob expands to nothing.
    ///
    /// Distinct from *no* catalog, which is what a `PolicyRouter` has by default and which passes
    /// patterns through untouched. A deployment that configures an empty catalog is saying "I have
    /// no models", and getting `NoRoute` is the correct answer to that.
    pub fn empty() -> Self {
        Self {
            version: 1,
            models: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), CatalogError> {
        let mut seen = std::collections::BTreeSet::new();
        for entry in &self.models {
            if entry.id.contains('*') {
                return Err(CatalogError::Invalid(format!(
                    "`{}` is a pattern, not a model — the catalog is what patterns resolve *to*",
                    entry.id
                )));
            }
            if !entry.id.contains('/') {
                return Err(CatalogError::Invalid(format!(
                    "`{}` has no provider prefix; nothing can route to it",
                    entry.id
                )));
            }
            if !seen.insert(entry.id.clone()) {
                // Two rows for one model means two answers to "what is its context", and which one
                // wins would depend on file order — a silent, position-dependent bug.
                return Err(CatalogError::Invalid(format!(
                    "`{}` appears twice",
                    entry.id
                )));
            }
        }
        Ok(())
    }

    pub fn get(&self, model: &ModelRef) -> Option<&ModelEntry> {
        self.models.iter().find(|e| e.id == model.0)
    }

    pub fn profile(&self, model: &ModelRef) -> Option<CapabilityProfile> {
        self.get(model).map(|e| e.profile.to_profile())
    }

    /// Expand one pool entry into concrete models, in catalog order.
    ///
    /// An exact id resolves to itself *if the catalog has it* — including a deprecated one, since a
    /// request that names a model by hand is not asking for a substitute. A pattern never yields a
    /// deprecated model.
    pub fn expand(&self, pattern: &str) -> Vec<ModelRef> {
        if !pattern.contains('*') {
            return self
                .models
                .iter()
                .filter(|e| e.id == pattern)
                .map(|e| ModelRef(e.id.clone()))
                .collect();
        }
        self.models
            .iter()
            .filter(|e| !e.deprecated && glob_match(pattern, &e.id))
            .map(|e| ModelRef(e.id.clone()))
            .collect()
    }

    /// Every price the catalog carries, as the table the meter and the ledger take.
    pub fn price_table(&self) -> PriceTable {
        self.models.iter().fold(PriceTable::new(), |table, entry| {
            match &entry.price {
                Some(p) => table.with(entry.id.clone(), p.to_pricing()),
                // Left out on purpose: an absent row is what makes the model count as unpriced
                // rather than free.
                None => table,
            }
        })
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.models.iter().map(|e| e.id.as_str())
    }
}

impl ProfileSpec {
    fn to_profile(&self) -> CapabilityProfile {
        CapabilityProfile {
            max_context_tokens: self.context,
            json_reliability: self.json,
            tool_reliability: self.tools,
            vision: self.vision,
            max_subagents: self.max_subagents,
            provenance: self.provenance,
        }
    }
}

impl PriceSpec {
    fn to_pricing(self) -> Pricing {
        Pricing {
            input_per_mtok_micros: self.input_per_mtok_micros,
            output_per_mtok_micros: self.output_per_mtok_micros,
            cache_read_pct: self.cache_read_pct,
            cache_write_pct: self.cache_write_pct,
            cache_write_1h_pct: self.cache_write_1h_pct,
        }
    }
}

/// `*` matches any run of characters, anywhere, any number of times. Nothing else is special.
///
/// Hand-written rather than a glob crate: the whole grammar is one metacharacter, matching happens
/// against model ids and not paths (so `/` must not be special, which is exactly where a path glob
/// would differ), and docs/02's dependency table is a thing you ask before adding to.
fn glob_match(pattern: &str, candidate: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return true;
    };
    let Some(mut rest) = candidate.strip_prefix(first) else {
        return false;
    };

    let tail: Vec<&str> = parts.collect();
    let Some((last, middle)) = tail.split_last() else {
        // No `*` at all: the prefix had to be the whole thing.
        return rest.is_empty();
    };

    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    // The trailing segment must land at the end, or `gpt-5*-mini` would match `gpt-5-mini-preview`.
    rest.len() >= last.len() && rest.ends_with(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> ModelCatalog {
        ModelCatalog::from_yaml(
            r#"
version: 1
models:
  - id: anthropic/claude-opus-4-1
    context: 200000
    vision: true
  - id: anthropic/claude-opus-4
    context: 200000
    vision: true
    deprecated: true
  - id: anthropic/claude-sonnet-4-5
    context: 200000
    price: { input_per_mtok_micros: 3000000, output_per_mtok_micros: 15000000 }
  - id: local/qwen3.5-4b
    context: 16000
    price: { input_per_mtok_micros: 0, output_per_mtok_micros: 0 }
"#,
        )
        .unwrap()
    }

    #[test]
    fn a_pattern_expands_in_catalog_order() {
        // Not sorted, not hashed — the order somebody wrote, because that order *is* the failover
        // preference.
        let c = catalog();
        assert_eq!(
            c.expand("anthropic/*")
                .iter()
                .map(|m| m.0.as_str())
                .collect::<Vec<_>>(),
            ["anthropic/claude-opus-4-1", "anthropic/claude-sonnet-4-5"]
        );
    }

    #[test]
    fn a_deprecated_model_is_reachable_by_name_but_never_by_pattern() {
        let c = catalog();
        assert!(c
            .expand("anthropic/claude-opus-*")
            .iter()
            .all(|m| m.0 != "anthropic/claude-opus-4"));
        // A request that pinned it still works: deprecation is about what pools grow into, not
        // about breaking callers who named a model on purpose.
        assert_eq!(c.expand("anthropic/claude-opus-4").len(), 1);
    }

    #[test]
    fn an_unknown_model_expands_to_nothing_rather_than_to_itself() {
        // The alternative — passing an unknown id through — is how a typo becomes a call to a model
        // that does not exist, reported by the provider instead of by us.
        assert!(catalog().expand("anthropic/claude-opus-9").is_empty());
        assert!(catalog().expand("openai/*").is_empty());
    }

    #[test]
    fn stars_match_where_they_are_written() {
        assert!(glob_match("a*", "abc"));
        assert!(glob_match("*c", "abc"));
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "ac"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match(
            "together/qwen3.5-*-instruct",
            "together/qwen3.5-9b-instruct"
        ));
        assert!(!glob_match(
            "together/qwen3.5-*-instruct",
            "together/qwen3.5-9b"
        ));
        assert!(!glob_match("gpt-5*-mini", "gpt-5-mini-preview"));
        assert!(!glob_match("a*", "b"));
        // `/` is not special, unlike in a path glob — which is the reason this is hand-written.
        assert!(glob_match("anthropic/*", "anthropic/claude-opus-4-1"));
    }

    #[test]
    fn an_unpriced_model_is_absent_from_the_table_not_zero_in_it() {
        let table = catalog().price_table();
        assert!(table
            .get(&ModelRef("anthropic/claude-opus-4-1".into()))
            .is_none());
        assert_eq!(
            table
                .get(&ModelRef("local/qwen3.5-4b".into()))
                .unwrap()
                .input_per_mtok_micros,
            0,
            "local is explicitly free, which is a different claim from unpriced"
        );
    }

    #[test]
    fn a_pattern_in_the_catalog_is_rejected() {
        let err = ModelCatalog::from_yaml("version: 1\nmodels: [{id: 'anthropic/*', context: 1}]");
        assert!(matches!(err, Err(CatalogError::Invalid(_))), "{err:?}");
    }

    #[test]
    fn a_duplicate_row_is_rejected() {
        let err = ModelCatalog::from_yaml(
            "version: 1\nmodels: [{id: a/b, context: 1}, {id: a/b, context: 2}]",
        );
        assert!(matches!(err, Err(CatalogError::Invalid(_))), "{err:?}");
    }

    #[test]
    fn a_model_without_a_provider_cannot_be_routed_to() {
        let err = ModelCatalog::from_yaml("version: 1\nmodels: [{id: qwen, context: 1}]");
        assert!(matches!(err, Err(CatalogError::Invalid(_))), "{err:?}");
    }

    #[test]
    fn the_shipped_catalog_parses_and_covers_the_shipped_policy() {
        let c = ModelCatalog::shipped();
        assert!(!c.is_empty());
        // Every pool entry in every shipped policy must name at least one real model, or the
        // deployment has a pool that silently routes nowhere.
        for policy_src in [
            include_str!("../policy/default.yaml"),
            include_str!("../policy/dev.yaml"),
            include_str!("../policy/local.yaml"),
        ] {
            let policy = crate::Policy::from_yaml(policy_src).unwrap();
            for (pool, entries) in &policy.pools {
                for entry in entries {
                    assert!(
                        !c.expand(entry).is_empty(),
                        "pool `{pool}` names `{entry}`, which no catalog model matches"
                    );
                }
            }
        }
    }
}
