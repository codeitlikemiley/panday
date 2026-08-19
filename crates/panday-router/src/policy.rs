//! The routing policy file and its first-match engine (docs/12, M12.1).
//!
//! "A YAML file you can read beats a model you can't debug, until the eval
//! data says otherwise." Everything here is deliberately boring and
//! inspectable: rules are evaluated top to bottom, the first match wins, and
//! every decision names the rule that produced it so an audit row can explain
//! itself.

use crate::{BudgetPressure, Caps, RouteDecision, RouteError, RouteQuery, Router};
use panday_types::model::{ModelRef, TaskClass};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The policy file (docs/12 §Policy file v1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub version: u16,
    /// Named pools of model globs, e.g. `frontier: [anthropic/claude-opus-*]`.
    pub pools: BTreeMap<String, Vec<String>>,
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub constraints: Constraints,
}

/// One rule: a predicate and the pool it selects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// An empty match `{}` is the catch-all default.
    #[serde(rename = "match", default)]
    pub predicate: Match,
    #[serde(rename = "use")]
    pub pool: String,
    /// Appended to the chain after the primary pool (docs/11: route returns a
    /// chain, never a single target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// Rule predicate. Every field is optional; an absent field does not
/// constrain. All present fields must hold (AND).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskClass>,
    /// Plan tiers this rule applies to, e.g. `[pro, max]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<Range>,
}

/// A numeric bound. `{ lt: 8000 }` reads as "less than 8000".
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gte: Option<u32>,
}

impl Range {
    fn contains(&self, v: u32) -> bool {
        self.lt.is_none_or(|lt| v < lt) && self.gte.is_none_or(|gte| v >= gte)
    }

    /// True when no value can satisfy this range — `{lt: 10, gte: 10}`.
    fn is_empty(&self) -> bool {
        matches!((self.lt, self.gte), (Some(lt), Some(gte)) if gte >= lt)
    }
}

/// Cross-cutting overrides applied *after* a rule matches (docs/12).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constraints {
    /// Tenant privacy flag → only these pools are admissible.
    ///
    /// docs/12 is emphatic about why this restricts rather than denies:
    /// "Denying just `frontier` would still leak to workhorse/cheap
    /// providers." An allowlist is the only safe shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy_strict: Option<RestrictTo>,
    /// >80% spend: demote to a cheaper pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_soft: Option<DemoteTo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestrictTo {
    pub restrict_to: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemoteTo {
    pub demote_to: String,
}

impl Match {
    fn matches(&self, q: &RouteQuery) -> bool {
        if let Some(offline) = self.offline {
            if q.offline != offline {
                return false;
            }
        }
        if let Some(task) = self.task {
            if q.task != Some(task) {
                return false;
            }
        }
        if !self.plan.is_empty() && !self.plan.contains(&q.plan) {
            return false;
        }
        if let Some(range) = self.context_tokens {
            if !range.contains(q.context_tokens) {
                return false;
            }
        }
        true
    }

    /// True for the catch-all `match: {}`.
    fn is_catch_all(&self) -> bool {
        *self == Match::default()
    }

    /// True when `other` can only match requests this one also matches, so a
    /// later `other` is dead. Conservative: it may miss some dead rules, but
    /// it never reports a reachable rule as unreachable.
    fn subsumes(&self, other: &Match) -> bool {
        // Every constraint we impose must be at least as loose as theirs.
        let offline_ok = match (self.offline, other.offline) {
            (None, _) => true,
            (Some(a), Some(b)) => a == b,
            (Some(_), None) => false,
        };
        let task_ok = match (self.task, other.task) {
            (None, _) => true,
            (Some(a), Some(b)) => a == b,
            (Some(_), None) => false,
        };
        let plan_ok = self.plan.is_empty()
            || (!other.plan.is_empty() && other.plan.iter().all(|p| self.plan.contains(p)));
        let ctx_ok = match (self.context_tokens, other.context_tokens) {
            (None, _) => true,
            (Some(a), Some(b)) => {
                // Ours must cover theirs at both ends.
                let lower = match (a.gte, b.gte) {
                    (None, _) => true,
                    (Some(x), Some(y)) => y >= x,
                    (Some(_), None) => false,
                };
                let upper = match (a.lt, b.lt) {
                    (None, _) => true,
                    (Some(x), Some(y)) => y <= x,
                    (Some(_), None) => false,
                };
                lower && upper
            }
            (Some(_), None) => false,
        };
        offline_ok && task_ok && plan_ok && ctx_ok
    }
}

/// A problem found at load time.
#[derive(Debug, Clone, PartialEq)]
pub struct Lint {
    pub rule_index: usize,
    pub message: String,
}

impl Policy {
    /// Parse and validate. Validation is not optional: a policy file that
    /// silently misroutes is worse than one that refuses to load.
    pub fn from_yaml(src: &str) -> Result<Self, RouteError> {
        let policy: Policy =
            serde_yaml_ng::from_str(src).map_err(|e| RouteError::Policy(format!("parse: {e}")))?;
        policy.validate()?;
        Ok(policy)
    }

    fn validate(&self) -> Result<(), RouteError> {
        if self.version != 1 {
            return Err(RouteError::Policy(format!(
                "unsupported policy version {} (this build understands 1)",
                self.version
            )));
        }
        if self.rules.is_empty() {
            return Err(RouteError::Policy(
                "no rules; every request would fail to route".into(),
            ));
        }

        // Every referenced pool must exist — a typo here silently drops
        // traffic to "no route" at runtime, which is the worst time to learn.
        for (i, rule) in self.rules.iter().enumerate() {
            for pool in [Some(&rule.pool), rule.fallback.as_ref()]
                .into_iter()
                .flatten()
            {
                if !self.pools.contains_key(pool.as_str()) {
                    return Err(RouteError::Policy(format!(
                        "rule {i} references unknown pool `{pool}`"
                    )));
                }
            }
        }
        for name in self
            .constraints
            .privacy_strict
            .iter()
            .flat_map(|r| &r.restrict_to)
            .chain(self.constraints.budget_soft.iter().map(|d| &d.demote_to))
        {
            if !self.pools.contains_key(name.as_str()) {
                return Err(RouteError::Policy(format!(
                    "constraints reference unknown pool `{name}`"
                )));
            }
        }
        if self.pools.values().any(|models| models.is_empty()) {
            return Err(RouteError::Policy(
                "a pool is empty; it can never serve a request".into(),
            ));
        }

        // Unreachable rules are reported by `lint()`, not raised here: they
        // are always a bug, but refusing to boot on one would be a hostile
        // way to find out. Callers surface them (CI fails on a non-empty
        // lint; a running gateway logs and continues).
        Ok(())
    }

    /// Report rules that can never match (docs/12: "the file is validated at
    /// load with unreachable-rule detection").
    pub fn lint(&self) -> Vec<Lint> {
        let mut lints = Vec::new();
        for (i, rule) in self.rules.iter().enumerate() {
            if let Some(range) = rule.predicate.context_tokens {
                if range.is_empty() {
                    lints.push(Lint {
                        rule_index: i,
                        message: format!(
                            "context_tokens range {:?} is empty; no value satisfies it",
                            range
                        ),
                    });
                    continue;
                }
            }
            // Shadowed by any earlier, broader rule?
            if let Some(j) = self.rules[..i]
                .iter()
                .position(|earlier| earlier.predicate.subsumes(&rule.predicate))
            {
                let why = if self.rules[j].predicate.is_catch_all() {
                    format!("rule {j} is the catch-all `match: {{}}` and matches everything first")
                } else {
                    format!("rule {j} is broader and always matches first")
                };
                lints.push(Lint {
                    rule_index: i,
                    message: format!("unreachable: {why}"),
                });
            }
        }
        lints
    }

    fn pool_chain(&self, name: &str) -> Vec<ModelRef> {
        self.pools
            .get(name)
            .map(|models| models.iter().map(|m| ModelRef(m.clone())).collect())
            .unwrap_or_default()
    }
}

/// The first-match policy engine.
pub struct PolicyRouter {
    policy: Policy,
    /// Turns pool patterns into models that exist (M12.2).
    ///
    /// Optional, and absent means *pass patterns through* rather than *expand to nothing*: a
    /// deployment whose adapters know their own model names — `panday local`, a test, anything
    /// pinning concrete ids in its pools — routes correctly without a catalog, and gets the same
    /// behaviour it had before the catalog existed. Configuring an empty catalog is the other
    /// statement, and yields `NoRoute`.
    catalog: Option<crate::catalog::ModelCatalog>,
}

impl PolicyRouter {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            catalog: None,
        }
    }

    pub fn from_yaml(src: &str) -> Result<Self, RouteError> {
        Ok(Self::new(Policy::from_yaml(src)?))
    }

    /// Resolve pool patterns against this catalog, and enforce the hard capabilities in it.
    pub fn with_catalog(mut self, catalog: crate::catalog::ModelCatalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

    pub fn catalog(&self) -> Option<&crate::catalog::ModelCatalog> {
        self.catalog.as_ref()
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Does this concrete model reference satisfy a pool glob?
    /// Only a trailing `*` is supported — enough for `claude-opus-*`, and a
    /// full glob dialect is a maintenance surface nobody asked for.
    fn glob_matches(pattern: &str, model: &str) -> bool {
        match pattern.strip_suffix('*') {
            Some(prefix) => model.starts_with(prefix),
            None => pattern == model,
        }
    }
}

impl Router for PolicyRouter {
    fn route(&self, q: &RouteQuery) -> Result<RouteDecision, RouteError> {
        let p = &self.policy;

        // A concrete pin bypasses rule selection — the caller asked for a
        // specific model — but NOT the constraints below: privacy and budget
        // are the tenant's, not the caller's, to negotiate.
        let pinned = (!q.requested.is_auto()).then(|| q.requested.clone());

        let (mut chain, matched_rule, mut pool_name) = match &pinned {
            Some(model) => (vec![model.clone()], "pinned".to_string(), String::new()),
            None => {
                let (i, rule) = p
                    .rules
                    .iter()
                    .enumerate()
                    .find(|(_, r)| r.predicate.matches(q))
                    .ok_or(RouteError::NoRoute {
                        task: q.task,
                        offline: q.offline,
                    })?;

                let mut chain = p.pool_chain(&rule.pool);
                if let Some(fb) = &rule.fallback {
                    chain.extend(p.pool_chain(fb));
                }
                (chain, format!("rules[{i}]"), rule.pool.clone())
            }
        };

        // --- constraints, applied after selection ---

        // Budget pressure demotes before privacy restricts, so a demotion can
        // never widen the admissible set past what privacy allows.
        if q.budget_pressure == BudgetPressure::Soft {
            if let Some(d) = &p.constraints.budget_soft {
                // docs/12: demote *non-code* tasks. Code is the product's
                // reason to exist; degrading it to save pennies is a bad trade.
                if q.task != Some(TaskClass::Code) && pinned.is_none() {
                    chain = p.pool_chain(&d.demote_to);
                    pool_name = d.demote_to.clone();
                }
            }
        }

        if q.privacy_strict {
            if let Some(r) = &p.constraints.privacy_strict {
                let admissible: Vec<ModelRef> = r
                    .restrict_to
                    .iter()
                    .flat_map(|name| p.pool_chain(name))
                    .collect();
                // Intersect rather than replace, so a pin that is already
                // local survives and one that is not gets dropped. Pool
                // entries are globs, so this is a pattern test, not equality.
                let filtered: Vec<ModelRef> = chain
                    .iter()
                    .filter(|m| {
                        admissible
                            .iter()
                            .any(|pat| Self::glob_matches(&pat.0, &m.0))
                    })
                    .cloned()
                    .collect();
                chain = if filtered.is_empty() {
                    admissible
                } else {
                    filtered
                };
                pool_name = r.restrict_to.join("+");
            }
        }

        // Hard budget: only local pools remain (they cost nothing to run).
        if q.budget_pressure == BudgetPressure::Hard {
            chain.retain(|m| m.0.starts_with("local/"));
        }

        // Patterns become models. Last, so every constraint above still reasons about the pool
        // vocabulary the policy file is written in.
        chain = self.expand(chain);

        // Capability filtering: a model that cannot do what the request needs
        // is not a fallback, it is a failure waiting to happen.
        chain = self.filter_caps(chain, &q.needs, q.context_tokens);

        if chain.is_empty() {
            return Err(RouteError::NoRoute {
                task: q.task,
                offline: q.offline,
            });
        }

        // The head's profile, because that is the model the turn will actually run against; a
        // failover to a weaker leg is a different conversation, and one the harness is told about
        // when it happens rather than pre-emptively.
        let profile = self
            .catalog
            .as_ref()
            .and_then(|c| chain.first().and_then(|m| c.profile(m)));

        Ok(RouteDecision {
            chain,
            matched_rule,
            pool: pool_name,
            counterfactual: None,
            profile,
        })
    }
}

impl PolicyRouter {
    /// Pool patterns → models that exist, in catalog order, without duplicates.
    ///
    /// A pool and its fallback routinely overlap (`cheap` and `local-only` share the 4B model), and
    /// a chain that lists the same model twice would retry a model that just failed before moving
    /// on — failover that does nothing, twice as slowly.
    fn expand(&self, chain: Vec<ModelRef>) -> Vec<ModelRef> {
        let Some(catalog) = &self.catalog else {
            return chain;
        };
        let mut out: Vec<ModelRef> = Vec::new();
        for pattern in chain {
            for model in catalog.expand(&pattern.0) {
                if !out.contains(&model) {
                    out.push(model);
                }
            }
        }
        out
    }

    /// Drop models that *cannot* serve the request.
    ///
    /// Hard limits only: the context it cannot hold and the image it cannot see. The reliability
    /// numbers in a profile are deliberately not admission criteria — see `catalog` for why a
    /// router that filtered on them would make every local-only deployment unroutable.
    ///
    /// A model the catalog does not know is kept. The alternative is dropping a model because our
    /// file is incomplete, and silently dropping a capable model is as bad as keeping an incapable
    /// one. Without a catalog at all this is a no-op, as it was before M12.2.
    fn filter_caps(
        &self,
        chain: Vec<ModelRef>,
        needs: &Caps,
        context_tokens: u32,
    ) -> Vec<ModelRef> {
        let Some(catalog) = &self.catalog else {
            return chain;
        };
        // What the request declares it needs, or what it actually carries — whichever is larger. A
        // caller that says nothing still cannot fit 40k of context into a 16k model.
        let context_floor = needs.min_context.max(context_tokens);
        chain
            .into_iter()
            .filter(|model| match catalog.profile(model) {
                Some(profile) => {
                    (!needs.vision || profile.vision) && profile.max_context_tokens >= context_floor
                }
                None => true,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy shipped in `policy/default.yaml`. Testing the real file
    /// rather than a fixture means a broken default breaks the build.
    const DEFAULT: &str = include_str!("../policy/default.yaml");

    fn query() -> RouteQuery {
        RouteQuery {
            requested: ModelRef::auto(),
            task: None,
            context_tokens: 1_000,
            needs: Caps::default(),
            plan: "free".into(),
            privacy_strict: false,
            budget_pressure: BudgetPressure::Normal,
            offline: false,
        }
    }

    fn router() -> PolicyRouter {
        PolicyRouter::from_yaml(DEFAULT).expect("the shipped default policy must be valid")
    }

    fn first(d: &RouteDecision) -> &str {
        &d.chain[0].0
    }

    // -- the shipped default -------------------------------------------------

    #[test]
    fn the_shipped_default_policy_parses_and_lints_clean() {
        let p = Policy::from_yaml(DEFAULT).expect("must parse");
        assert!(
            p.lint().is_empty(),
            "the default policy has unreachable rules: {:?}",
            p.lint()
        );
    }

    // -- first-match table ---------------------------------------------------

    #[test]
    fn first_match_selects_the_expected_pool() {
        /// (name, how to shape the query, expected pool)
        type Case = (&'static str, fn(&mut RouteQuery), &'static str);

        let cases: Vec<Case> = vec![
            ("default falls through to workhorse", |_q| {}, "workhorse"),
            (
                "offline overrides everything",
                |q| q.offline = true,
                "local-only",
            ),
            (
                "routing tasks stay cheap",
                |q| q.task = Some(TaskClass::Route),
                "cheap",
            ),
            (
                "short summaries are cheap",
                |q| {
                    q.task = Some(TaskClass::Summarize);
                    q.context_tokens = 4_000;
                },
                "cheap",
            ),
            (
                "long summaries are NOT cheap - the range excludes them",
                |q| {
                    q.task = Some(TaskClass::Summarize);
                    q.context_tokens = 40_000;
                },
                "workhorse",
            ),
            (
                "code on a paid plan gets frontier",
                |q| {
                    q.task = Some(TaskClass::Code);
                    q.plan = "pro".into();
                },
                "frontier",
            ),
            (
                "code on free plan does not",
                |q| {
                    q.task = Some(TaskClass::Code);
                    q.plan = "free".into();
                },
                "workhorse",
            ),
        ];

        let r = router();
        for (name, mutate, expected) in cases {
            let mut q = query();
            mutate(&mut q);
            let d = r.route(&q).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(d.pool, expected, "{name}");
        }
    }

    #[test]
    fn offline_beats_a_more_specific_later_rule() {
        // Ordering is the semantics: an offline code request on a pro plan
        // must NOT reach frontier, even though a later rule matches it.
        let r = router();
        let d = r
            .route(&RouteQuery {
                offline: true,
                task: Some(TaskClass::Code),
                plan: "pro".into(),
                ..query()
            })
            .unwrap();
        assert_eq!(d.pool, "local-only");
        assert!(d.chain.iter().all(|m| m.0.starts_with("local/")));
    }

    // -- chains --------------------------------------------------------------

    #[test]
    fn a_rule_with_a_fallback_returns_a_chain_not_a_single_target() {
        // docs/11: "Route returns a chain, not a single target."
        let r = router();
        let d = r
            .route(&RouteQuery {
                task: Some(TaskClass::Code),
                plan: "max".into(),
                ..query()
            })
            .unwrap();
        assert!(
            d.chain.len() > 2,
            "frontier + workhorse fallback should concatenate: {:?}",
            d.chain
        );
        assert!(first(&d).starts_with("anthropic/claude-opus"));
        assert!(
            d.chain.iter().any(|m| m.0.contains("sonnet")),
            "the fallback leg must be present"
        );
    }

    #[test]
    fn a_concrete_pin_bypasses_rule_selection() {
        let r = router();
        let d = r
            .route(&RouteQuery {
                requested: ModelRef("openai/gpt-5-mini".into()),
                ..query()
            })
            .unwrap();
        assert_eq!(d.chain, vec![ModelRef("openai/gpt-5-mini".into())]);
        assert_eq!(d.matched_rule, "pinned");
    }

    // -- constraints ---------------------------------------------------------

    #[test]
    fn privacy_strict_restricts_to_an_allowlist_not_a_denylist() {
        // The whole point: denying `frontier` alone would still leak to the
        // workhorse/cheap providers.
        let r = router();
        let d = r
            .route(&RouteQuery {
                privacy_strict: true,
                task: Some(TaskClass::Code),
                plan: "max".into(),
                ..query()
            })
            .unwrap();
        assert!(
            d.chain.iter().all(|m| m.0.starts_with("local/")),
            "privacy_strict leaked a non-local target: {:?}",
            d.chain
        );
    }

    #[test]
    fn privacy_strict_drops_a_pin_that_would_leave_the_building() {
        let r = router();
        let d = r
            .route(&RouteQuery {
                requested: ModelRef("anthropic/claude-opus-4".into()),
                privacy_strict: true,
                ..query()
            })
            .unwrap();
        assert!(
            d.chain.iter().all(|m| m.0.starts_with("local/")),
            "a pin must not override the tenant's privacy flag: {:?}",
            d.chain
        );
    }

    #[test]
    fn privacy_strict_keeps_a_pin_that_is_already_local() {
        let r = router();
        let d = r
            .route(&RouteQuery {
                requested: ModelRef("local/qwen3.5-4b".into()),
                privacy_strict: true,
                ..query()
            })
            .unwrap();
        assert_eq!(d.chain, vec![ModelRef("local/qwen3.5-4b".into())]);
    }

    #[test]
    fn soft_budget_demotes_non_code_but_spares_code() {
        let r = router();

        let demoted = r
            .route(&RouteQuery {
                task: Some(TaskClass::Chat),
                budget_pressure: BudgetPressure::Soft,
                ..query()
            })
            .unwrap();
        assert_eq!(demoted.pool, "cheap");

        let spared = r
            .route(&RouteQuery {
                task: Some(TaskClass::Code),
                plan: "pro".into(),
                budget_pressure: BudgetPressure::Soft,
                ..query()
            })
            .unwrap();
        assert_eq!(
            spared.pool, "frontier",
            "code must not be degraded to save pennies"
        );
    }

    #[test]
    fn hard_budget_leaves_only_local_targets() {
        let r = router();
        // `route` tasks land on `cheap`, which mixes a hosted and a local
        // model; only the local one may survive a hit ceiling.
        let d = r
            .route(&RouteQuery {
                task: Some(TaskClass::Route),
                budget_pressure: BudgetPressure::Hard,
                ..query()
            })
            .unwrap();
        assert!(!d.chain.is_empty());
        assert!(
            d.chain.iter().all(|m| m.0.starts_with("local/")),
            "a hit ceiling must not leave a paid target in the chain: {:?}",
            d.chain
        );
    }

    #[test]
    fn hard_budget_with_no_local_target_is_no_route_not_a_silent_paid_call() {
        let yaml = r#"
version: 1
pools:
  paid: [anthropic/claude-sonnet-4-5]
rules:
  - match: {}
    use: paid
"#;
        let r = PolicyRouter::from_yaml(yaml).unwrap();
        let err = r
            .route(&RouteQuery {
                budget_pressure: BudgetPressure::Hard,
                ..query()
            })
            .unwrap_err();
        assert!(matches!(err, RouteError::NoRoute { .. }));
    }

    // -- validation ----------------------------------------------------------

    #[test]
    fn rejects_an_unknown_pool_reference() {
        let yaml = r#"
version: 1
pools:
  cheap: [local/m]
rules:
  - match: {}
    use: typo-pool
"#;
        let err = Policy::from_yaml(yaml).unwrap_err();
        assert!(
            err.to_string().contains("typo-pool"),
            "the error must name the offending pool: {err}"
        );
    }

    #[test]
    fn rejects_an_unknown_fallback_pool() {
        let yaml = r#"
version: 1
pools:
  cheap: [local/m]
rules:
  - match: {}
    use: cheap
    fallback: nope
"#;
        assert!(Policy::from_yaml(yaml).is_err());
    }

    #[test]
    fn rejects_a_future_policy_version() {
        let yaml = r#"
version: 99
pools: { cheap: [local/m] }
rules: [{ match: {}, use: cheap }]
"#;
        let err = Policy::from_yaml(yaml).unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn rejects_an_empty_pool() {
        let yaml = r#"
version: 1
pools: { cheap: [] }
rules: [{ match: {}, use: cheap }]
"#;
        assert!(Policy::from_yaml(yaml).is_err());
    }

    #[test]
    fn rejects_a_typo_in_a_match_key() {
        // deny_unknown_fields: `taks:` must fail loudly, not match everything.
        let yaml = r#"
version: 1
pools: { cheap: [local/m] }
rules: [{ match: { taks: code }, use: cheap }]
"#;
        assert!(Policy::from_yaml(yaml).is_err());
    }

    // -- the unreachable-rule linter ----------------------------------------

    #[test]
    fn flags_a_rule_shadowed_by_the_catch_all() {
        let yaml = r#"
version: 1
pools:
  cheap: [local/m]
  frontier: [anthropic/a]
rules:
  - match: {}
    use: cheap
  - match: { task: code }
    use: frontier
"#;
        let p = Policy::from_yaml(yaml).unwrap();
        let lints = p.lint();
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].rule_index, 1);
        assert!(
            lints[0].message.contains("catch-all"),
            "{}",
            lints[0].message
        );
    }

    #[test]
    fn flags_a_rule_shadowed_by_an_earlier_broader_rule() {
        let yaml = r#"
version: 1
pools:
  cheap: [local/m]
  frontier: [anthropic/a]
rules:
  - match: { task: code }
    use: cheap
  - match: { task: code, plan: [pro] }
    use: frontier
  - match: {}
    use: cheap
"#;
        let lints = Policy::from_yaml(yaml).unwrap().lint();
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].rule_index, 1, "the narrower later rule is dead");
    }

    #[test]
    fn flags_an_empty_context_range() {
        let yaml = r#"
version: 1
pools: { cheap: [local/m] }
rules:
  - match: { context_tokens: { lt: 100, gte: 100 } }
    use: cheap
  - match: {}
    use: cheap
"#;
        let lints = Policy::from_yaml(yaml).unwrap().lint();
        assert_eq!(lints[0].rule_index, 0);
        assert!(lints[0].message.contains("empty"));
    }

    #[test]
    fn does_not_flag_rules_that_merely_look_similar() {
        // Different tasks never shadow each other; a false positive here
        // would train people to ignore the linter.
        let yaml = r#"
version: 1
pools: { cheap: [local/m], frontier: [anthropic/a] }
rules:
  - match: { task: code }
    use: frontier
  - match: { task: chat }
    use: cheap
  - match: { task: summarize, context_tokens: { lt: 8000 } }
    use: cheap
  - match: {}
    use: cheap
"#;
        assert!(Policy::from_yaml(yaml).unwrap().lint().is_empty());
    }

    #[test]
    fn a_narrower_range_after_a_wider_one_is_flagged() {
        let yaml = r#"
version: 1
pools: { cheap: [local/m] }
rules:
  - match: { context_tokens: { lt: 10000 } }
    use: cheap
  - match: { context_tokens: { lt: 5000 } }
    use: cheap
  - match: {}
    use: cheap
"#;
        let lints = Policy::from_yaml(yaml).unwrap().lint();
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].rule_index, 1);
    }

    // -- glob matching -------------------------------------------------------

    #[test]
    fn trailing_star_globs_match_by_prefix() {
        assert!(PolicyRouter::glob_matches(
            "anthropic/claude-opus-*",
            "anthropic/claude-opus-4"
        ));
        assert!(!PolicyRouter::glob_matches(
            "anthropic/claude-opus-*",
            "anthropic/claude-sonnet-4"
        ));
        // No star: exact match only.
        assert!(PolicyRouter::glob_matches(
            "local/qwen3.5-4b",
            "local/qwen3.5-4b"
        ));
        assert!(!PolicyRouter::glob_matches(
            "local/qwen3.5-4b",
            "local/qwen3.5-4b-instruct"
        ));
    }
}
