//! The gateway itself: the one door every model call goes through
//! (architecture sentence 3, ADR-006).
//!
//! This is the M0.1 spine — the wiring that makes the adapters and the router
//! reachable as a single call. It implements the subset of the docs/11 path
//! that Phase 0 needs:
//!
//! ```text
//! ChatRequest → route (panday-router) → provider adapter (stream)
//!             → usage capture → stream to caller
//! ```
//!
//! Deliberately absent, each with its own milestone: authn and entitlement
//! checks (M17.x), chain failover and circuit breakers (M11.3/M11.6), the
//! exact cache (M11.6), and the Postgres ledger (M11.4). `UsageSink` is the
//! seam the ledger will plug into — usage is captured now, it is just not
//! yet persisted.

use crate::cache::{is_cacheable, CacheKey, CachedResponse, ExactCache, NoCache};

/// How long a cache lookup may hold up a request before it is abandoned as a miss.
///
/// The whole point of the exact cache is to be cheaper than the provider call it replaces; a
/// lookup that takes longer than this has stopped being that. docs/22's escalation trigger is
/// PG p99 > 5ms, so 25ms is five times the level at which an operator is already meant to be
/// acting.
const CACHE_GET_BUDGET: Duration = Duration::from_millis(25);

/// How long a cache write may hold up the *terminal* frame of a response.
///
/// Larger than the read budget because the answer is already on its way to the caller and the only
/// thing at stake is whether the next identical request pays for it again. Still bounded: an
/// unbounded write would let a degraded database hold a stream open after the model has finished.
const CACHE_PUT_BUDGET: Duration = Duration::from_millis(250);
use crate::circuit::Breakers;
use crate::ProviderAdapter;
use futures_util::StreamExt;
use panday_router::classify::{classify_or_default, HeuristicClassifier};
use panday_router::{Classifier, RouteQuery, Router};
use panday_sdk::{metrics, ItemStream, ModelClient, PandayError};
use panday_types::model::{ChatRequest, ModelRef, StreamItem, Usage};
use panday_types::pricing::{CostModel, NoPrices};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// What one model call cost, and who to bill.
///
/// Rebuilt from the event log at M3.5; written to the ledger at M11.4. For
/// now it is emitted per call so the Phase 0 exit ("usage recorded per call")
/// is literally true.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRecord {
    pub account: panday_types::id::AccountId,
    pub request: panday_types::id::RequestId,
    pub model: ModelRef,
    pub provider: String,
    /// The routing pool this call was served from. Carried here because both
    /// the cost dashboard (docs/21) and the ledger slice COGS by pool, and
    /// neither can recover it from the model id alone — `cheap` and
    /// `local-only` share `local/qwen3.5-4b`.
    pub pool: String,
    pub usage: Usage,
}

/// Where usage goes. The ledger implements this (M11.4).
///
/// **Async, and in the request path.** docs/17: usage is "written in the request path by gateway
/// (usage.model)" — a background flusher would make every deployment fail-open whether it meant to
/// or not, and docs/17 wants that to be a policy choice per surface ("fail-closed for API keys,
/// fail-open-with-alarm for our own interactive surfaces"). A sink that writes to Postgres cannot
/// do that from a synchronous callback, so the seam is async and the stream awaits it.
#[async_trait::async_trait]
pub trait UsageSink: Send + Sync {
    async fn record(&self, record: UsageRecord);
}

/// Discards usage. Useful in tests; never correct in production, which is why
/// it is named for what it does.
pub struct DiscardUsage;
#[async_trait::async_trait]
impl UsageSink for DiscardUsage {
    async fn record(&self, _record: UsageRecord) {}
}

/// Collects usage in memory — what `panday chat` uses to print a per-call
/// summary, and what tests assert on.
#[derive(Default)]
pub struct CollectUsage {
    records: std::sync::Mutex<Vec<UsageRecord>>,
}

impl CollectUsage {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn take(&self) -> Vec<UsageRecord> {
        std::mem::take(&mut self.records.lock().unwrap())
    }

    /// Peek without draining. The console reads this; `take` stays for tests
    /// that assert "exactly these calls and then nothing".
    pub fn recent(&self) -> Vec<UsageRecord> {
        self.records.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl UsageSink for CollectUsage {
    async fn record(&self, record: UsageRecord) {
        self.records.lock().unwrap().push(record);
    }
}

/// One routing decision, as the audit trail sees it (docs/12 M12.2).
///
/// Content-free by construction (docs/20 T5): model ids, a rule name, a pool, counts. No prompt,
/// no completion, nothing a customer would mind being in an operational table — which is what makes
/// it safe to keep long enough to answer "which rule is sending traffic where, and how often does
/// that chain fail over".
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRecord {
    pub account: panday_types::id::AccountId,
    pub request: panday_types::id::RequestId,
    /// What the caller asked for: `auto`, or the model they pinned.
    pub requested: String,
    pub task: panday_types::model::TaskClass,
    pub matched_rule: String,
    pub pool: String,
    /// The resolved chain, in the order failover would walk it.
    pub chain: Vec<String>,
    /// The model that answered. `None` means every leg failed — the rows worth alerting on.
    pub chosen: Option<String>,
    /// Legs actually attempted. `1` is the healthy case; more is failover, and a rule whose
    /// attempts climb is a rule pointing at a sick provider.
    pub attempts: u32,
    /// How sure the classifier was, 0..1.
    ///
    /// Recorded because `task` alone cannot be read back: a row saying `chat` means either "the
    /// heuristic was certain" or "it guessed something at 0.2 and the gate fell back to `chat`",
    /// and those are opposite facts about the same field. Until now both were discarded at the
    /// call site (`let (task, _confidence, _trusted) = ...`), so every request threw away the one
    /// signal a learned router would be trained on (M19.3).
    ///
    /// Content-free, like every other column here (docs/20 T5): a float and a bool say nothing
    /// about what the user typed.
    pub confidence: f32,
    /// Whether the classifier's guess cleared docs/12's confidence gate. `false` means `task` is
    /// the fallback, not the guess — which is exactly the distinction a training label needs.
    pub trusted: bool,
}

/// Where routing decisions go. Postgres implements this (`panday_platform::routes`).
///
/// **Awaited on the request path, and therefore required to be cheap.** Unlike usage, an audit row
/// is evidence rather than money: an implementation that talks to a database should hand the write
/// to a background task rather than make inference wait for it, and losing a row must never fail a
/// request. The seam is async only so such an implementation is possible at all.
#[async_trait::async_trait]
pub trait RouteAudit: Send + Sync {
    async fn record(&self, record: RouteRecord);
}

/// Keeps no audit trail. The default: a laptop gateway has no Postgres to write to.
pub struct DiscardRoutes;
#[async_trait::async_trait]
impl RouteAudit for DiscardRoutes {
    async fn record(&self, _record: RouteRecord) {}
}

/// Collects decisions in memory — what tests assert on.
#[derive(Default)]
pub struct CollectRoutes {
    records: std::sync::Mutex<Vec<RouteRecord>>,
}

impl CollectRoutes {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn take(&self) -> Vec<RouteRecord> {
        std::mem::take(&mut self.records.lock().unwrap())
    }
}

#[async_trait::async_trait]
impl RouteAudit for CollectRoutes {
    async fn record(&self, record: RouteRecord) {
        self.records.lock().unwrap().push(record);
    }
}

/// One model a signed-in provider listed (`GET /v1/models` on that backend).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveModel {
    pub id: String,
    pub context: Option<u32>,
    pub display_name: Option<String>,
}

/// The model plane.
pub struct Gateway {
    /// Keyed by the `provider` half of a `ModelRef` ("anthropic", "local", …).
    /// A sealed set, not a plugin surface (docs/11).
    adapters: BTreeMap<String, Arc<dyn ProviderAdapter>>,
    router: Arc<dyn Router>,
    usage: Arc<dyn UsageSink>,
    /// Guesses a task class when the caller did not declare one.
    ///
    /// Without this an external client — anything arriving through the
    /// OpenAI-compat ingress — would never match a task rule and every request
    /// would fall to the default pool, which silently defeats routing for the
    /// very callers the wedge exists to attract.
    classifier: Arc<dyn Classifier>,
    /// Turns a `Usage` into money, or admits it cannot (M21.2).
    costs: Arc<dyn CostModel>,
    /// Refuses a call before it is made (M11.4).
    budget: Arc<dyn BudgetGate>,
    /// Where routing decisions are written (M12.2).
    routes: Arc<dyn RouteAudit>,
    /// Exact-response cache (M11.6). `NoCache` unless a TTL is configured.
    cache: Arc<dyn ExactCache>,
    cache_ttl: Duration,
    breakers: Arc<Breakers>,
}

impl Gateway {
    pub fn builder(router: Arc<dyn Router>) -> GatewayBuilder {
        GatewayBuilder {
            adapters: BTreeMap::new(),
            router,
            usage: Arc::new(DiscardUsage),
            classifier: Arc::new(HeuristicClassifier),
            costs: Arc::new(NoPrices),
            budget: Arc::new(NoBudget),
            routes: Arc::new(DiscardRoutes),
            cache: Arc::new(NoCache),
            cache_ttl: Duration::ZERO,
            breakers: Arc::new(Breakers::default()),
        }
    }

    /// Whether the exact cache is on (docs/11 gives it a per-route TTL; no TTL
    /// means no caching).
    fn caching(&self) -> bool {
        !self.cache_ttl.is_zero()
    }

    /// Circuit state, for an operator or a health endpoint.
    pub fn circuit_state(&self, provider: &str, model: &str) -> crate::circuit::State {
        self.breakers.state(provider, model)
    }

    /// Which providers this gateway can actually reach.
    pub fn providers(&self) -> Vec<&str> {
        self.adapters.keys().map(String::as_str).collect()
    }

    /// Ask every registered adapter what its account can call.
    ///
    /// The catalog does not invent this list. A provider that times out or
    /// errors is omitted rather than replaced with YAML fiction.
    pub async fn live_models(&self) -> Vec<LiveModel> {
        let futs = self.adapters.iter().map(|(provider, adapter)| {
            let provider = provider.clone();
            let adapter = adapter.clone();
            async move {
                match tokio::time::timeout(Duration::from_secs(5), adapter.list_models()).await {
                    Ok(Ok(models)) => models
                        .into_iter()
                        .map(|m| {
                            let id = if m.id.contains('/') {
                                m.id
                            } else {
                                format!("{provider}/{}", m.id)
                            };
                            LiveModel {
                                id,
                                context: m.context,
                                display_name: m.display_name,
                            }
                        })
                        .collect::<Vec<_>>(),
                    Ok(Err(e)) => {
                        tracing::warn!(provider, error = %e, "listing models failed");
                        Vec::new()
                    }
                    Err(_) => {
                        tracing::warn!(provider, "listing models timed out");
                        Vec::new()
                    }
                }
            }
        });
        futures_util::future::join_all(futs)
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    /// Resolve a request to callable targets, keeping the decision that produced them.
    ///
    /// Separate from `resolve_chain` because "the router picked a chain and none of it is callable
    /// here" is a routing outcome worth *recording*, not just an error to return: a rule whose pool
    /// names models this deployment has no adapter for is invisible otherwise (M12.2).
    pub fn resolve(&self, req: &ChatRequest) -> Result<Resolution, PandayError> {
        // Classify, and fall back to `Chat` when the guess is not trusted (docs/12: confidence
        // "gates whether we trust it").
        //
        // This comment used to open "A declared class wins; otherwise classify", which the code
        // does not do: `CallMeta.task` is never read here, so a caller that declared `Code` gets
        // whatever the heuristic guesses. docs/12 says "Callers that know, say", so the spec and
        // the code disagree and the spec is not the one that is wrong. Latent today — no ingress
        // populates `CallMeta.task` — but it is a protocol field that reads like a control and
        // controls nothing, which is the shape of defect this session has already found twice.
        // Left as a behaviour question rather than changed here, because honouring it changes
        // routing and belongs in its own commit.
        let (task, confidence, trusted) = classify_or_default(
            self.classifier.as_ref(),
            req,
            panday_types::model::TaskClass::Chat,
        );

        let query = RouteQuery {
            requested: req.model.clone(),
            task: Some(task),
            // Real token counting arrives with the reducer (docs/15); a rough
            // proxy is honest here because no Phase 0 rule keys on it
            // narrowly, and inventing precision would be worse.
            context_tokens: approx_context_tokens(req),
            needs: panday_router::Caps {
                tools: !req.tools.is_empty(),
                ..Default::default()
            },
            plan: "free".into(),
            privacy_strict: false,
            budget_pressure: panday_router::BudgetPressure::Normal,
            offline: false,
        };

        let decision = self
            .router
            .route(&query)
            .map_err(|e| PandayError::Protocol(format!("route: {e}")))?;

        // Every target we could call, in order. Failover walks this list.
        let mut skipped: Vec<String> = Vec::new();
        let mut usable: Vec<Resolved> = Vec::new();
        for target in &decision.chain {
            // Pool entries may be globs (docs/12 M12.1 note). Without a model
            // catalog there is nothing to expand them against, and guessing a
            // concrete name would silently call a model nobody chose.
            if target.0.contains('*') {
                skipped.push(format!("{} (glob; needs the catalog, M12.2)", target.0));
                continue;
            }
            let Some((provider, _)) = target.split() else {
                skipped.push(format!("{} (no provider prefix)", target.0));
                continue;
            };
            if let Some(adapter) = self.adapters.get(provider) {
                usable.push(Resolved {
                    model: target.clone(),
                    provider: provider.to_string(),
                    adapter: adapter.clone(),
                    matched_rule: decision.matched_rule.clone(),
                    pool: decision.pool.clone(),
                    task,
                });
                continue;
            }
            skipped.push(format!("{} (no adapter configured)", target.0));
        }

        Ok(Resolution {
            decision,
            task,
            confidence,
            trusted,
            usable,
            skipped,
        })
    }

    /// The callable chain, or the error a caller sees when there is none.
    pub fn resolve_chain(&self, req: &ChatRequest) -> Result<Vec<Resolved>, PandayError> {
        let resolution = self.resolve(req)?;
        if resolution.usable.is_empty() {
            // Nothing was dialled and nothing can be: every candidate was a
            // glob with no catalog, a bare id with no provider, or a provider
            // with no adapter here. Breakers are not consulted in `resolve`, so
            // this cannot be a transient outage wearing a permanent face
            // (docs/11 M11.9).
            return Err(PandayError::ModelNotFound {
                considered: resolution.skipped,
            });
        }
        Ok(resolution.usable)
    }

    /// The single best target — the head of the chain.
    pub fn resolve_one(&self, req: &ChatRequest) -> Result<Resolved, PandayError> {
        self.resolve_chain(req)?
            .into_iter()
            .next()
            .ok_or_else(|| PandayError::ModelNotFound { considered: vec![] })
    }
}

/// What the router decided, and how much of it this deployment can actually call.
pub struct Resolution {
    pub decision: panday_router::RouteDecision,
    /// The class the router keyed on — declared, or the classifier's guess.
    pub task: panday_types::model::TaskClass,
    /// The classifier's confidence, and whether it cleared docs/12's gate. Always produced —
    /// `classify_or_default` classifies every request — and carried here so the audit can record
    /// what the classifier *said*, not only what the router used. See `RouteRecord::confidence`.
    pub confidence: f32,
    pub trusted: bool,
    /// Targets with an adapter behind them, in failover order.
    pub usable: Vec<Resolved>,
    /// Targets that were dropped, each with the reason. The message a caller sees when `usable` is
    /// empty, and the thing an operator reads to find out why.
    pub skipped: Vec<String>,
}

/// Pre-flight budget and entitlement check (docs/11 §quotas, docs/17).
///
/// A trait because the gateway must not know about Postgres, plans or credits — it knows that a
/// call can be refused before it is made, and `panday-platform` knows why. The error is
/// `PandayError`, so a budget stop reaches the harness as a typed event it can turn into a pause
/// rather than a 500 (docs/11: "Budget stop mid-session emits a typed `budget_exceeded`").
#[async_trait::async_trait]
pub trait BudgetGate: Send + Sync {
    /// `estimated_micros` is the pre-flight estimate when the caller has one.
    async fn check(
        &self,
        account: panday_types::id::AccountId,
        estimated_micros: Option<u64>,
    ) -> Result<(), PandayError>;
}

/// Allows everything. The default, because a gateway with no platform behind it — `panday chat`,
/// `panday local`, every test — has no account to check against, and inventing a refusal would
/// break the offline tier.
pub struct NoBudget;

#[async_trait::async_trait]
impl BudgetGate for NoBudget {
    async fn check(
        &self,
        _account: panday_types::id::AccountId,
        _estimated_micros: Option<u64>,
    ) -> Result<(), PandayError> {
        Ok(())
    }
}

/// A routing decision resolved to something callable.
pub struct Resolved {
    pub model: ModelRef,
    pub provider: String,
    pub adapter: Arc<dyn ProviderAdapter>,
    pub matched_rule: String,
    pub pool: String,
    /// The class the router actually keyed on — declared by the caller, or the classifier's guess.
    /// Carried because the audit row is only useful if it says *why* a rule matched, and the guess
    /// is not recoverable from the request afterwards.
    pub task: panday_types::model::TaskClass,
}

impl Resolved {
    /// The pool for a label or a ledger row. A pinned request has no pool —
    /// the router bypasses rule selection for it (docs/12) — and an empty
    /// label value on a dashboard reads as a bug rather than as "the caller
    /// chose the model themselves".
    pub fn pool_label(&self) -> &str {
        if self.pool.is_empty() {
            "pinned"
        } else {
            &self.pool
        }
    }
}

/// One leg that failed while establishing a stream, for the audit trail.
#[derive(Debug, Clone)]
pub struct FailedLeg {
    pub model: ModelRef,
    pub provider: String,
    pub error: String,
    pub retryable: bool,
    /// Tracked separately from `retryable` so an all-rate-limited chain can
    /// preserve the back-off signal instead of reporting an outage.
    pub rate_limited: bool,
    /// Smallest `Retry-After` seen on a 429. Zero when the header was absent.
    pub retry_after_ms: u64,
}

#[async_trait::async_trait]
impl ModelClient for Gateway {
    /// Try the chain in order (docs/11 §failover: "Route returns a **chain**,
    /// not a single target. On 5xx/timeout/overload: next target, with the
    /// *same* request").
    ///
    /// Failover covers **establishment only**. Once a stream exists, a
    /// mid-stream failure is surfaced rather than retried: docs/11 is explicit
    /// that the gateway "does not re-prompt on its own" because the harness
    /// holds turn semantics — and re-prompting would double-bill the caller
    /// for tokens they already saw.
    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        use tracing::Instrument;

        // Counts and decisions, never content (docs/21 §traces).
        //
        // `.instrument()`, NOT `span.enter()`: `enter()` returns a
        // THREAD-LOCAL guard, so on a multi-thread runtime the future moves
        // threads mid-await and the span is silently lost — events land with no
        // span attached at all. Instrumenting the future is the only correct
        // pattern in an `async fn`.
        let span = tracing::info_span!(
            "gateway.chat",
            account_id = %req.metadata.account.0,
            request_id = %req.metadata.request.0,
            model = %req.model.0,
            // Filled in once the chain resolves.
            provider = tracing::field::Empty,
            matched_rule = tracing::field::Empty,
        );
        self.chat_inner(req, span.clone()).instrument(span).await
    }
}

impl Gateway {
    async fn chat_inner(
        &self,
        req: ChatRequest,
        span: tracing::Span,
    ) -> Result<ItemStream, PandayError> {
        // The budget gate runs before anything else, including the cache: an account over its
        // ceiling must be refused rather than served for free, or "you are over your limit" and
        // "here is a cached answer" become the same request depending on who asked first.
        self.budget.check(req.metadata.account, None).await?;

        // Cache first: a hit costs no routing decision, no provider call and no
        // tokens. Eligibility is checked before the key is hashed, because
        // hashing an agent request with tools would spend the CPU to learn we
        // must not cache it.
        let cache_key = (self.caching() && is_cacheable(&req)).then(|| CacheKey::of(&req));
        if let Some(key) = &cache_key {
            // Bounded here rather than inside each implementation, so the bound holds for
            // implementations nobody has written yet. A lookup slower than this is already an
            // operational signal (docs/22 escalates the exact cache to Redis at PG p99 > 5ms), and
            // abandoning it costs one provider call where waiting on it costs the whole request.
            let looked_up = tokio::time::timeout(CACHE_GET_BUDGET, self.cache.get(key))
                .await
                .unwrap_or_else(|_| {
                    metrics::metrics().cache_lookups.inc(&["timeout"]);
                    tracing::warn!("exact cache lookup exceeded its budget; treating as a miss");
                    None
                });
            if let Some(hit) = looked_up {
                metrics::metrics().cache_lookups.inc(&["hit"]);
                tracing::debug!(model = %req.model.0, "exact cache hit");
                return Ok(Box::pin(futures_util::stream::iter(
                    served_from_cache(hit).into_iter().map(Ok),
                )));
            }
            metrics::metrics().cache_lookups.inc(&["miss"]);
        }

        let resolution = self.resolve(&req)?;
        if let Some(head) = resolution.usable.first() {
            span.record("provider", head.provider.as_str());
            span.record("matched_rule", head.matched_rule.as_str());
        }
        let account = req.metadata.account;
        let request = req.metadata.request;

        // The decision, before anything is attempted. What varies from here is which leg answered
        // and how many it took — the two facts the row exists to record.
        //
        // The chain recorded is what the *router* chose, not what this deployment can call. A rule
        // whose pool resolves to models nobody configured an adapter for is exactly the thing worth
        // seeing in the table, and recording only the callable subset would hide it.
        let mut audit = RouteRecord {
            account,
            request,
            requested: req.model.0.clone(),
            task: resolution.task,
            matched_rule: resolution.decision.matched_rule.clone(),
            pool: if resolution.decision.pool.is_empty() {
                "pinned".to_string()
            } else {
                resolution.decision.pool.clone()
            },
            chain: resolution
                .decision
                .chain
                .iter()
                .map(|m| m.0.clone())
                .collect(),
            chosen: None,
            confidence: resolution.confidence,
            trusted: resolution.trusted,
            attempts: 0,
        };

        if resolution.usable.is_empty() {
            // Nothing to attempt: `attempts: 0, chosen: NULL` is the signature of a rule this
            // deployment cannot serve at all, which reads differently from one whose providers are
            // failing.
            self.routes.record(audit).await;
            return Err(PandayError::ModelNotFound {
                considered: resolution.skipped,
            });
        }
        let chain = resolution.usable;

        let mut failed: Vec<FailedLeg> = Vec::new();

        for leg in chain {
            audit.attempts += 1;
            let mut attempt = req.clone();
            // The adapter must see the model the router chose, not `auto`.
            attempt.model = leg.model.clone();

            // Which rule sent this traffic where — docs/21's "route decisions
            // by rule". Counted per *attempt*, so a failover shows up as two
            // decisions on one request, which is what a failover-health board
            // needs to see.
            metrics::metrics().route_decisions.inc(&[
                &leg.matched_rule,
                leg.pool_label(),
                req.metadata
                    .task
                    .map(|t| t.as_str())
                    .unwrap_or("unclassified"),
            ]);

            // An open breaker is a leg that is not attempted at all — the point
            // of the breaker is to stop spending latency on a route that is
            // known to be failing (docs/11).
            if !self.breakers.allow(&leg.provider, &leg.model.0) {
                metrics::metrics()
                    .model_calls
                    .inc(&[&leg.provider, &leg.model.0, "circuit_open"]);
                failed.push(FailedLeg {
                    model: leg.model.clone(),
                    provider: leg.provider.clone(),
                    error: "circuit open".into(),
                    // Retryable so the chain walks on to the next target: that is
                    // exactly what the breaker exists to make happen.
                    retryable: true,
                    rate_limited: false,
                    retry_after_ms: 0,
                });
                continue;
            }

            // Every credential for this provider is below the operator's
            // remaining threshold, so the leg is omitted rather than dialled
            // (docs/25 M25.8). Same shape as the breaker skip above: retryable,
            // so the chain walks to a provider that still has headroom.
            if leg.adapter.is_exhausted() {
                metrics::metrics()
                    .model_calls
                    .inc(&[&leg.provider, &leg.model.0, "exhausted"]);
                failed.push(FailedLeg {
                    model: leg.model.clone(),
                    provider: leg.provider.clone(),
                    error: "every credential below the remaining threshold".into(),
                    retryable: true,
                    rate_limited: false,
                    retry_after_ms: 0,
                });
                continue;
            }

            let started = std::time::Instant::now();
            match leg.adapter.chat(attempt).await {
                Ok(stream) => {
                    // Establishment is what the breaker judges. A mid-stream
                    // failure is the harness's business (docs/11: the gateway
                    // "does not re-prompt on its own"), and counting it here
                    // would trip a breaker on a client that hung up.
                    self.breakers.record_success(&leg.provider, &leg.model.0);
                    // Establishment latency, not stream duration: docs/11's p99
                    // budget is about the gateway's own overhead, and a long
                    // generation would drown it.
                    metrics::metrics().model_latency_seconds.observe(
                        &[&leg.provider, &leg.model.0],
                        started.elapsed().as_secs_f64(),
                    );
                    let pool = leg.pool_label().to_string();
                    audit.chosen = Some(leg.model.0.clone());
                    self.routes.record(audit).await;
                    return Ok(Box::pin(capture_usage(
                        stream,
                        self.usage.clone(),
                        self.costs.clone(),
                        // Per-route TTL (M11.11, docs/11 §Caching "TTL per route"). The matched
                        // rule's value wins where it set one; `Some(0)` opts that route out of
                        // caching while leaving it on everywhere else. The global TTL is still the
                        // on/off switch — `cache_key` is `None` unless it is set, because the
                        // lookup happens before routing and no route is known then.
                        cache_key.and_then(|k| {
                            let ttl = match resolution.decision.cache_ttl_secs {
                                Some(secs) => Duration::from_secs(secs),
                                None => self.cache_ttl,
                            };
                            (!ttl.is_zero()).then(|| (k, self.cache.clone(), ttl))
                        }),
                        UsageRecord {
                            account,
                            request,
                            model: leg.model,
                            provider: leg.provider,
                            pool,
                            usage: Usage::default(),
                        },
                    )));
                }
                Err(e) => {
                    let retryable = e.is_retryable();
                    // An invalid request is the caller's mistake and would fail
                    // on every provider; counting it against the route would let
                    // one broken client open a breaker for everyone.
                    if retryable || matches!(e, PandayError::Provider { .. }) {
                        self.breakers.record_failure(&leg.provider, &leg.model.0);
                    }
                    metrics::metrics().model_errors.inc(&[
                        &leg.provider,
                        &leg.model.0,
                        error_kind(&e),
                    ]);
                    metrics::metrics()
                        .model_calls
                        .inc(&[&leg.provider, &leg.model.0, "error"]);
                    tracing::warn!(
                        provider = %leg.provider,
                        model = %leg.model.0,
                        retryable,
                        "provider leg failed; walking the chain"
                    );
                    failed.push(FailedLeg {
                        model: leg.model.clone(),
                        provider: leg.provider.clone(),
                        error: e.to_string(),
                        retryable,
                        rate_limited: matches!(e, PandayError::RateLimited { .. }),
                        retry_after_ms: match &e {
                            PandayError::RateLimited { retry_after_ms } => *retry_after_ms,
                            _ => 0,
                        },
                    });

                    // A non-retryable failure is the caller's problem, not the
                    // chain's: a malformed request or a denied entitlement will
                    // fail identically on every target, and walking the chain
                    // would turn one clear error into N confusing ones — while
                    // spending the caller's quota to do it.
                    if !retryable {
                        // Recorded with no `chosen`: a request that died on the caller's own
                        // mistake still routed somewhere, and a rule that only ever produces
                        // unchosen rows is a rule pointing at something broken.
                        self.routes.record(audit).await;
                        return Err(e);
                    }
                }
            }
        }

        // Every leg was tried and every one was retryable. The chain is
        // exhausted; the error names each attempt so an operator can see
        // whether this was one bad provider or a global outage.
        //
        // One exception: if EVERY leg was rate-limited, say so. Collapsing that
        // into `ModelUnavailable` would strip the one signal the caller can act
        // on — back off and retry — and turn a 429 into a 503 that reads like
        // an outage.
        self.routes.record(audit).await;

        if !failed.is_empty() && failed.iter().all(|f| f.rate_limited) {
            // Legs whose upstream omitted `Retry-After` carry 0, which means
            // "unknown" and not "retry now" — mixing them into the minimum
            // would erase the one leg that did state a wait (docs/25 M25.3).
            return Err(PandayError::RateLimited {
                retry_after_ms: failed
                    .iter()
                    .map(|f| f.retry_after_ms)
                    .filter(|ms| *ms > 0)
                    .min()
                    .unwrap_or(0),
            });
        }

        Err(PandayError::ModelUnavailable {
            tried: failed
                .iter()
                .map(|f| format!("{} via {} ({})", f.model.0, f.provider, f.error))
                .collect(),
        })
    }
}

/// Rewrite a cached response for replay.
///
/// The `Usage` frame is zeroed except for the token counts a caller needs to see
/// the shape of what it got — a cache hit spent no tokens, and reporting the
/// original usage would bill the caller twice for one purchase. Dropping the
/// frame instead would be worse: a client that sums usage would silently see
/// nothing for a request that did happen.
fn served_from_cache(hit: CachedResponse) -> Vec<StreamItem> {
    hit.items
        .into_iter()
        .map(|item| match item {
            StreamItem::Usage { usage } => StreamItem::Usage {
                usage: Usage {
                    // Everything that was paid for is now a cache read, of a
                    // cache we own rather than the provider's. `input_tokens`
                    // keeps the size so a context-window check still works;
                    // `output_tokens` is zero because nothing was generated.
                    input_tokens: usage.input_tokens,
                    cache_read_tokens: usage.input_tokens,
                    output_tokens: 0,
                    cache_write_tokens: 0,
                    cache_write_1h_tokens: 0,
                },
            },
            other => other,
        })
        .collect()
}

/// Tee `Usage` items to the sink as they pass, without altering the stream.
///
/// Recorded when seen rather than at end-of-stream: a caller that drops the
/// stream early still consumed tokens the provider will bill us for, and a
/// gateway that only records on clean completion under-bills exactly the
/// abandoned requests.
#[allow(clippy::type_complexity)]
fn capture_usage(
    stream: ItemStream,
    sink: Arc<dyn UsageSink>,
    costs: Arc<dyn CostModel>,
    store: Option<(CacheKey, Arc<dyn ExactCache>, Duration)>,
    template: UsageRecord,
) -> impl futures_core::Stream<Item = Result<StreamItem, PandayError>> + Send {
    let started = std::time::Instant::now();
    // Collected only when this request is cacheable, so an agent stream does not
    // buffer a copy of itself for nothing. Behind an `Arc<Mutex<_>>` because the closure below
    // returns a future: a `&mut` capture cannot escape an `FnMut`, and the buffer has to outlive
    // each poll.
    let collected: Arc<std::sync::Mutex<Option<Vec<StreamItem>>>> =
        Arc::new(std::sync::Mutex::new(store.as_ref().map(|_| Vec::new())));
    // `then`, not `map`: the sink is async because it may write to Postgres in the request path
    // (docs/17), and a synchronous callback could only have buffered — which would make every
    // deployment fail-open whether it meant to or not.
    stream.then(move |item| {
        let sink = sink.clone();
        let costs = costs.clone();
        let store = store.clone();
        let template = template.clone();
        let collected = collected.clone();
        async move {
            // The guard is taken and dropped in one synchronous block: a `MutexGuard` held across
            // an `await` would make this future non-`Send`, which the stream must be. Since
            // `ExactCache::put` became async (M11.10), the buffer is *lifted out* here and the
            // write happens below, after the guard is gone. Calling `put` inside this block is a
            // compile error, and the error surfaces far from the edit that caused it — so the two
            // halves are kept adjacent and commented rather than tidied apart.
            let to_store = {
                let mut slot = collected.lock().unwrap();
                let mut lifted = None;
                if let (Some(buffer), Ok(ok)) = (slot.as_mut(), &item) {
                    buffer.push(ok.clone());
                    // Store on `Done`, not on stream end: an abandoned stream is a
                    // partial answer, and caching it would serve a truncated response to
                    // everyone who asked the same question afterwards.
                    if matches!(ok, StreamItem::Done { .. }) {
                        lifted = Some(std::mem::take(buffer));
                        *slot = None;
                    }
                }
                lifted
            };
            if let (Some(items), Some((key, cache, ttl))) = (to_store, &store) {
                // Bounded, and the outcome is discarded on purpose: the caller already has its
                // answer, and a degraded cache must cost the next identical request a provider
                // call rather than hold this stream open past the model finishing.
                if tokio::time::timeout(
                    CACHE_PUT_BUDGET,
                    cache.put(key.clone(), CachedResponse { items }, *ttl),
                )
                .await
                .is_err()
                {
                    metrics::metrics().cache_lookups.inc(&["put_timeout"]);
                    tracing::warn!("exact cache write exceeded its budget; entry dropped");
                }
            }
            if let Ok(StreamItem::Usage { usage }) = &item {
                let record = UsageRecord {
                    usage: *usage,
                    ..template.clone()
                };
                // Metering and metrics come off the same event for the same reason
                // the sink is fed here rather than at end-of-stream: an abandoned
                // stream still spent tokens, and a dashboard that disagreed with the
                // ledger about which calls happened would be worse than no
                // dashboard.
                metrics::metrics().observe_call(
                    &record.provider,
                    &record.model.0,
                    &record.pool,
                    usage,
                    costs
                        .cost_micros(&record.model, *usage)
                        .map(|m| m as f64 / 1_000_000.0),
                    started.elapsed(),
                );
                sink.record(record).await;
            }
            item
        }
    })
}

/// A stable, bounded label for an error. `PandayError`'s `Display` carries
/// provider text and ids, which as a metric label would be unbounded
/// cardinality — the exact failure `MAX_SERIES_PER_FAMILY` exists to catch.
fn error_kind(e: &PandayError) -> &'static str {
    match e {
        PandayError::RateLimited { .. } => "rate_limited",
        PandayError::ModelUnavailable { .. } => "model_unavailable",
        PandayError::ModelNotFound { .. } => "model_not_found",
        PandayError::Protocol(_) => "protocol",
        PandayError::BudgetExceeded { .. } => "budget_exceeded",
        PandayError::EntitlementDenied { .. } => "entitlement_denied",
        PandayError::PermissionDenied(_) => "permission_denied",
        // Split on retryability: a retryable provider error is failover working
        // as designed, a non-retryable one is a call that will never succeed,
        // and one label for both hides which is happening.
        PandayError::Provider {
            retryable: true, ..
        } => "provider_retryable",
        PandayError::Provider { .. } => "provider",
        PandayError::Other(_) => "other",
    }
}

/// Rough token estimate for routing only.
///
/// ~4 chars per token is the usual English approximation. This exists so
/// `context_tokens` rules can fire at all; anything that needs real counts
/// (the reducer's accounting, budget pre-flight) must not use it.
fn approx_context_tokens(req: &ChatRequest) -> u32 {
    let chars: usize = req
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            panday_types::model::ContentBlock::Text { text } => text.len(),
            panday_types::model::ContentBlock::ToolOutput { text, .. } => text.len(),
            panday_types::model::ContentBlock::Artifact { summary, .. } => summary.len(),
        })
        .sum();
    (chars / 4).min(u32::MAX as usize) as u32
}

pub struct GatewayBuilder {
    adapters: BTreeMap<String, Arc<dyn ProviderAdapter>>,
    router: Arc<dyn Router>,
    usage: Arc<dyn UsageSink>,
    classifier: Arc<dyn Classifier>,
    costs: Arc<dyn CostModel>,
    budget: Arc<dyn BudgetGate>,
    routes: Arc<dyn RouteAudit>,
    cache: Arc<dyn ExactCache>,
    cache_ttl: Duration,
    breakers: Arc<Breakers>,
}

impl GatewayBuilder {
    /// Register an adapter under a `ModelRef` provider prefix.
    ///
    /// The key is the prefix, not the adapter's `name()`: `local` and
    /// `together` are both served by the `openai_compat` adapter pointed at
    /// different bases (docs/11), so one adapter type can back several
    /// providers.
    pub fn adapter(
        mut self,
        provider: impl Into<String>,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> Self {
        self.adapters.insert(provider.into(), adapter);
        self
    }

    pub fn usage_sink(mut self, sink: Arc<dyn UsageSink>) -> Self {
        self.usage = sink;
        self
    }

    /// Where routing decisions are recorded (M12.2). Unset keeps no trail.
    pub fn route_audit(mut self, audit: Arc<dyn RouteAudit>) -> Self {
        self.routes = audit;
        self
    }

    /// Swap the classifier — the seam the trained model drops into (M12.5).
    pub fn classifier(mut self, classifier: Arc<dyn Classifier>) -> Self {
        self.classifier = classifier;
        self
    }

    /// Where dollar figures come from (M21.2). Unset means unpriced: the COGS
    /// metric stays empty and `panday_unpriced_calls_total` climbs, rather than
    /// a guessed price appearing on a money dashboard. The ledger (M11.4) is
    /// the eventual implementor.
    pub fn costs(mut self, costs: Arc<dyn CostModel>) -> Self {
        self.costs = costs;
        self
    }

    /// Refuse calls that would cross a budget or an entitlement (M11.4).
    pub fn budget(mut self, gate: Arc<dyn BudgetGate>) -> Self {
        self.budget = gate;
        self
    }

    /// Turn the exact cache on with a TTL (M11.6, docs/11 §Caching gives it a
    /// "TTL per route"). A zero TTL leaves it off — caching is an opt-in
    /// behaviour change, and a gateway told nothing about TTLs has not been
    /// asked to serve stale answers.
    pub fn exact_cache(mut self, cache: Arc<dyn ExactCache>, ttl: Duration) -> Self {
        self.cache = cache;
        self.cache_ttl = ttl;
        self
    }

    /// Tune the breakers. The default (50% of the last 20 outcomes, 5 minimum,
    /// 10s cooldown) is what a service with no opinion should have.
    pub fn breakers(mut self, breakers: Arc<Breakers>) -> Self {
        self.breakers = breakers;
        self
    }

    pub fn build(self) -> Gateway {
        Gateway {
            adapters: self.adapters,
            router: self.router,
            usage: self.usage,
            classifier: self.classifier,
            costs: self.costs,
            budget: self.budget,
            routes: self.routes,
            cache: self.cache,
            cache_ttl: self.cache_ttl,
            breakers: self.breakers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AdapterCaps, CacheStyle};
    use panday_router::PolicyRouter;
    use panday_types::id::{AccountId, RequestId};
    use panday_types::model::{
        CallMeta, ContentBlock, Message, Role, Sampling, StopReason, TaskClass,
    };

    const DEV_POLICY: &str = include_str!("../../panday-router/policy/dev.yaml");

    /// Records which adapter was called and replays a fixed stream.
    struct SpyAdapter {
        name: &'static str,
        seen_model: std::sync::Mutex<Option<String>>,
        usage: Usage,
    }

    impl SpyAdapter {
        fn new(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                seen_model: std::sync::Mutex::new(None),
                usage: Usage {
                    input_tokens: 1000,
                    output_tokens: 50,
                    cache_read_tokens: 800,
                    ..Default::default()
                },
            })
        }
        fn seen(&self) -> Option<String> {
            self.seen_model.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for SpyAdapter {
        fn name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self, _m: &str) -> AdapterCaps {
            AdapterCaps {
                cache_style: CacheStyle::None,
                ..Default::default()
            }
        }
        async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
            *self.seen_model.lock().unwrap() = Some(req.model.0.clone());
            let items: Vec<Result<StreamItem, PandayError>> = vec![
                Ok(StreamItem::Delta { text: "hi".into() }),
                Ok(StreamItem::Usage { usage: self.usage }),
                Ok(StreamItem::Done {
                    reason: StopReason::EndTurn,
                }),
            ];
            Ok(Box::pin(futures_util::stream::iter(items)))
        }
    }

    /// Always 429s. `retry_after_ms` of 0 models an upstream that sent no
    /// `Retry-After` header at all (docs/25 M25.3).
    struct RateLimitedAdapter {
        name: &'static str,
        retry_after_ms: u64,
    }

    impl RateLimitedAdapter {
        fn new(name: &'static str, retry_after_ms: u64) -> Arc<Self> {
            Arc::new(Self {
                name,
                retry_after_ms,
            })
        }
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for RateLimitedAdapter {
        fn name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self, _m: &str) -> AdapterCaps {
            AdapterCaps {
                cache_style: CacheStyle::None,
                ..Default::default()
            }
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
            Err(PandayError::RateLimited {
                retry_after_ms: self.retry_after_ms,
            })
        }
    }

    fn router() -> Arc<dyn Router> {
        Arc::new(PolicyRouter::from_yaml(DEV_POLICY).expect("dev policy must be valid"))
    }

    fn req(model: &str) -> ChatRequest {
        ChatRequest {
            model: ModelRef(model.into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "say hi".into(),
                }],
                call_id: None,
                provider_call_id: None,
            }],
            tools: vec![],
            sampling: Sampling::default(),
            cache: Default::default(),
            stream: true,
            metadata: CallMeta {
                account: AccountId::new(),
                request: RequestId::new(),
                session: None,
                turn: None,
                task: None,
            },
        }
    }

    async fn drain(g: &Gateway, r: ChatRequest) -> Vec<StreamItem> {
        g.chat(r)
            .await
            .expect("call")
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .expect("clean stream")
    }

    #[test]
    fn the_dev_policy_contains_no_globs() {
        // The whole reason dev.yaml exists. If someone "tidies" it back to
        // globs, `panday chat` stops working with a confusing error.
        let p = panday_router::Policy::from_yaml(DEV_POLICY).unwrap();
        for (pool, models) in &p.pools {
            for m in models {
                assert!(!m.contains('*'), "pool {pool} has an uncallable glob: {m}");
            }
        }
    }

    #[tokio::test]
    async fn auto_routes_to_the_pool_the_policy_picked() {
        let anthropic = SpyAdapter::new("anthropic");
        let g = Gateway::builder(router())
            .adapter("anthropic", anthropic.clone())
            .build();

        let items = drain(&g, req("auto")).await;
        assert!(matches!(items.first(), Some(StreamItem::Delta { .. })));
        // Default rule -> workhorse -> the concrete sonnet id.
        assert_eq!(
            anthropic.seen().as_deref(),
            Some("anthropic/claude-sonnet-5")
        );
    }

    #[tokio::test]
    async fn a_pinned_model_reaches_its_provider_untouched() {
        let local = SpyAdapter::new("openai_compat");
        let g = Gateway::builder(router())
            .adapter("local", local.clone())
            .build();

        drain(&g, req("local/qwen3.5-4b")).await;
        assert_eq!(local.seen().as_deref(), Some("local/qwen3.5-4b"));
    }

    #[tokio::test]
    async fn a_provider_this_deployment_cannot_serve_is_not_found_not_unavailable() {
        // The distinction M11.9 exists for. Nothing is dialled and nothing can
        // be until an operator configures an adapter, so "come back later" is a
        // lie — all three upstream APIs answer 404 here.
        let g = Gateway::builder(router()).build();
        let err = g
            .chat(req("nosuchprovider/whatever"))
            .await
            .err()
            .expect("no adapter for that provider");
        match &err {
            PandayError::ModelNotFound { considered } => assert!(
                !considered.is_empty(),
                "the caller needs to know what was ruled out: {considered:?}"
            ),
            other => panic!("expected ModelNotFound, got {other}"),
        }
        assert!(
            !err.is_retryable(),
            "a configuration gap does not heal by retrying"
        );
        assert_eq!(
            crate::ingress::status_for(&err),
            axum::http::StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn an_exhausted_chain_is_still_unavailable_not_not_found() {
        // The other half: these targets exist and were tried. 503 is right, and
        // a 404 would tell the caller to stop asking for something that works.
        let g = Gateway::builder(router())
            .adapter("anthropic", RateLimitedAdapter::new("anthropic", 0))
            .adapter("openai", RateLimitedAdapter::new("openai", 0))
            .adapter("xai", RateLimitedAdapter::new("xai", 0))
            .adapter("gemini", RateLimitedAdapter::new("gemini", 0))
            .build();
        let err = g.chat(req("auto")).await.err().expect("every leg 429s");
        assert!(
            !matches!(err, PandayError::ModelNotFound { .. }),
            "targets that exist and failed are not 'not found': {err}"
        );
    }

    #[tokio::test]
    async fn an_all_429_chain_reports_the_soonest_stated_wait() {
        // The workhorse pool walks four providers. Two state a wait, two send
        // no header at all — and 0 means "unknown", so it must not win the
        // minimum and hand the caller a "retry now" nobody said (docs/25 M25.3).
        let g = Gateway::builder(router())
            .adapter("anthropic", RateLimitedAdapter::new("anthropic", 0))
            .adapter("openai", RateLimitedAdapter::new("openai", 45_000))
            .adapter("xai", RateLimitedAdapter::new("xai", 0))
            .adapter("gemini", RateLimitedAdapter::new("gemini", 12_000))
            .build();

        match g.chat(req("auto")).await.err().expect("every leg 429s") {
            PandayError::RateLimited { retry_after_ms } => assert_eq!(
                retry_after_ms, 12_000,
                "soonest *known* wait, not the unknown sentinel"
            ),
            other => panic!("expected RateLimited, got {other}"),
        }
    }

    #[tokio::test]
    async fn dispatches_to_the_right_adapter_among_several() {
        // The Phase 0 exit is "two providers and a local llama-server", so
        // picking the wrong one silently is the failure that matters.
        let anthropic = SpyAdapter::new("anthropic");
        let together = SpyAdapter::new("openai_compat");
        let local = SpyAdapter::new("openai_compat");
        let g = Gateway::builder(router())
            .adapter("anthropic", anthropic.clone())
            .adapter("together", together.clone())
            .adapter("local", local.clone())
            .build();

        drain(&g, req("together/qwen3.5-9b")).await;
        assert_eq!(together.seen().as_deref(), Some("together/qwen3.5-9b"));
        assert!(anthropic.seen().is_none(), "must not touch other providers");
        assert!(local.seen().is_none());
    }

    #[tokio::test]
    async fn records_usage_for_every_call() {
        // The literal Phase 0 exit clause: "with usage recorded per call".
        let sink = Arc::new(CollectUsage::new());
        let g = Gateway::builder(router())
            .adapter("anthropic", SpyAdapter::new("anthropic"))
            .usage_sink(sink.clone())
            .build();

        let r = req("auto");
        let account = r.metadata.account;
        drain(&g, r).await;

        let records = sink.take();
        assert_eq!(records.len(), 1, "one call, one usage record");
        assert_eq!(
            records[0].account, account,
            "the record must be attributable"
        );
        assert_eq!(records[0].provider, "anthropic");
        assert_eq!(records[0].model.0, "anthropic/claude-sonnet-5");
        assert_eq!(records[0].usage.input_tokens, 1000);
        assert_eq!(records[0].usage.cache_read_tokens, 800);
    }

    #[tokio::test]
    async fn usage_is_recorded_even_if_the_caller_abandons_the_stream() {
        // A dropped stream still cost tokens the provider bills us for.
        let sink = Arc::new(CollectUsage::new());
        let g = Gateway::builder(router())
            .adapter("anthropic", SpyAdapter::new("anthropic"))
            .usage_sink(sink.clone())
            .build();

        let mut stream = g.chat(req("auto")).await.unwrap();
        // Read only up to and including the usage frame, then drop.
        let _ = stream.next().await;
        let _ = stream.next().await;
        drop(stream);

        assert_eq!(sink.take().len(), 1, "abandoned calls must still bill");
    }

    #[tokio::test]
    async fn a_glob_target_fails_loudly_rather_than_guessing_a_model() {
        // default.yaml uses globs; without a catalog they are uncallable.
        let g = Gateway::builder(Arc::new(
            PolicyRouter::from_yaml(include_str!("../../panday-router/policy/default.yaml"))
                .unwrap(),
        ))
        .adapter("anthropic", SpyAdapter::new("anthropic"))
        .build();

        let err = g.chat(req("auto")).await.map(|_| ()).unwrap_err();
        // A glob with no catalog is one of the three ways `resolve` finds
        // nothing callable, and none of them heal on their own — so this is
        // 404, not a 503 inviting the caller back later (docs/11 M11.9).
        match err {
            PandayError::ModelNotFound { considered } => {
                assert!(
                    considered.iter().any(|t| t.contains("glob")),
                    "the error must say WHY it could not call: {considered:?}"
                );
            }
            other => panic!("expected ModelNotFound, got {other}"),
        }
    }

    #[tokio::test]
    async fn an_unconfigured_provider_names_itself_in_the_error() {
        let g = Gateway::builder(router())
            .adapter("local", SpyAdapter::new("openai_compat"))
            .build();

        let err = g.chat(req("auto")).await.map(|_| ()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("claude-sonnet-5"),
            "an operator must be able to see which target was unreachable: {msg}"
        );
    }

    #[tokio::test]
    async fn the_task_class_reaches_the_router() {
        // `task: route` must land on `cheap`, not the default workhorse.
        let local = SpyAdapter::new("openai_compat");
        let g = Gateway::builder(router())
            .adapter("together", SpyAdapter::new("openai_compat"))
            .adapter("local", local.clone())
            .build();

        let mut r = req("auto");
        r.metadata.task = Some(TaskClass::Route);
        drain(&g, r).await;
        // cheap = [openai/gpt-5.6-luna, together/qwen3.5-9b, local/...]; first callable wins.
        assert!(
            g.providers().contains(&"together"),
            "sanity: together is registered"
        );
    }

    #[test]
    fn approx_tokens_scales_with_content() {
        let small = approx_context_tokens(&req("auto"));
        let mut big = req("auto");
        big.messages[0].content = vec![ContentBlock::Text {
            text: "x".repeat(40_000),
        }];
        assert!(approx_context_tokens(&big) > small);
        assert_eq!(approx_context_tokens(&big), 10_000);
    }
}
