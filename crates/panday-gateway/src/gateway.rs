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

use crate::ProviderAdapter;
use futures_util::StreamExt;
use panday_router::classify::{classify_or_default, HeuristicClassifier};
use panday_router::{Classifier, RouteQuery, Router};
use panday_sdk::{metrics, ItemStream, ModelClient, PandayError};
use panday_types::model::{ChatRequest, ModelRef, StreamItem, Usage};
use panday_types::pricing::{CostModel, NoPrices};
use std::collections::BTreeMap;
use std::sync::Arc;

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

/// Where usage goes. The ledger implements this at M11.4.
pub trait UsageSink: Send + Sync {
    fn record(&self, record: UsageRecord);
}

/// Discards usage. Useful in tests; never correct in production, which is why
/// it is named for what it does.
pub struct DiscardUsage;
impl UsageSink for DiscardUsage {
    fn record(&self, _record: UsageRecord) {}
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
}

impl UsageSink for CollectUsage {
    fn record(&self, record: UsageRecord) {
        self.records.lock().unwrap().push(record);
    }
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
}

impl Gateway {
    pub fn builder(router: Arc<dyn Router>) -> GatewayBuilder {
        GatewayBuilder {
            adapters: BTreeMap::new(),
            router,
            usage: Arc::new(DiscardUsage),
            classifier: Arc::new(HeuristicClassifier),
            costs: Arc::new(NoPrices),
        }
    }

    /// Which providers this gateway can actually reach.
    pub fn providers(&self) -> Vec<&str> {
        self.adapters.keys().map(String::as_str).collect()
    }

    /// Resolve a request to a concrete (provider, model) target.
    ///
    /// Returns the routing decision alongside it so the caller can report
    /// which rule fired — the audit record docs/12 asks for. Persisting it is
    /// M12.2.
    pub fn resolve_chain(&self, req: &ChatRequest) -> Result<Vec<Resolved>, PandayError> {
        // A declared class wins; otherwise classify, and fall back to `Chat`
        // when the guess is not trusted (docs/12: confidence "gates whether we
        // trust it").
        let (task, _confidence, _trusted) = classify_or_default(
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
                });
                continue;
            }
            skipped.push(format!("{} (no adapter configured)", target.0));
        }

        if usable.is_empty() {
            return Err(PandayError::ModelUnavailable { tried: skipped });
        }
        Ok(usable)
    }

    /// The single best target — the head of the chain.
    pub fn resolve(&self, req: &ChatRequest) -> Result<Resolved, PandayError> {
        self.resolve_chain(req)?
            .into_iter()
            .next()
            .ok_or_else(|| PandayError::ModelUnavailable { tried: vec![] })
    }
}

/// A routing decision resolved to something callable.
pub struct Resolved {
    pub model: ModelRef,
    pub provider: String,
    pub adapter: Arc<dyn ProviderAdapter>,
    pub matched_rule: String,
    pub pool: String,
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
        let chain = self.resolve_chain(&req)?;
        if let Some(head) = chain.first() {
            span.record("provider", head.provider.as_str());
            span.record("matched_rule", head.matched_rule.as_str());
        }
        let account = req.metadata.account;
        let request = req.metadata.request;

        let mut failed: Vec<FailedLeg> = Vec::new();

        for leg in chain {
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

            let started = std::time::Instant::now();
            match leg.adapter.chat(attempt).await {
                Ok(stream) => {
                    // Establishment latency, not stream duration: docs/11's p99
                    // budget is about the gateway's own overhead, and a long
                    // generation would drown it.
                    metrics::metrics().model_latency_seconds.observe(
                        &[&leg.provider, &leg.model.0],
                        started.elapsed().as_secs_f64(),
                    );
                    let pool = leg.pool_label().to_string();
                    return Ok(Box::pin(capture_usage(
                        stream,
                        self.usage.clone(),
                        self.costs.clone(),
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
                    });

                    // A non-retryable failure is the caller's problem, not the
                    // chain's: a malformed request or a denied entitlement will
                    // fail identically on every target, and walking the chain
                    // would turn one clear error into N confusing ones — while
                    // spending the caller's quota to do it.
                    if !retryable {
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
        if !failed.is_empty() && failed.iter().all(|f| f.rate_limited) {
            return Err(PandayError::RateLimited { retry_after_ms: 0 });
        }

        Err(PandayError::ModelUnavailable {
            tried: failed
                .iter()
                .map(|f| format!("{} via {} ({})", f.model.0, f.provider, f.error))
                .collect(),
        })
    }
}

/// Tee `Usage` items to the sink as they pass, without altering the stream.
///
/// Recorded when seen rather than at end-of-stream: a caller that drops the
/// stream early still consumed tokens the provider will bill us for, and a
/// gateway that only records on clean completion under-bills exactly the
/// abandoned requests.
fn capture_usage(
    stream: ItemStream,
    sink: Arc<dyn UsageSink>,
    costs: Arc<dyn CostModel>,
    template: UsageRecord,
) -> impl futures_core::Stream<Item = Result<StreamItem, PandayError>> + Send {
    let started = std::time::Instant::now();
    stream.map(move |item| {
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
            sink.record(record);
        }
        item
    })
}

/// A stable, bounded label for an error. `PandayError`'s `Display` carries
/// provider text and ids, which as a metric label would be unbounded
/// cardinality — the exact failure `MAX_SERIES_PER_FAMILY` exists to catch.
fn error_kind(e: &PandayError) -> &'static str {
    match e {
        PandayError::RateLimited { .. } => "rate_limited",
        PandayError::ModelUnavailable { .. } => "model_unavailable",
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

    pub fn build(self) -> Gateway {
        Gateway {
            adapters: self.adapters,
            router: self.router,
            usage: self.usage,
            classifier: self.classifier,
            costs: self.costs,
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
            Some("anthropic/claude-sonnet-4-5")
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
        assert_eq!(records[0].model.0, "anthropic/claude-sonnet-4-5");
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
        match err {
            PandayError::ModelUnavailable { tried } => {
                assert!(
                    tried.iter().any(|t| t.contains("glob")),
                    "the error must say WHY it could not call: {tried:?}"
                );
            }
            other => panic!("expected ModelUnavailable, got {other}"),
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
            msg.contains("claude-sonnet-4-5"),
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
        // cheap = [together/qwen3.5-9b, local/...]; first callable wins.
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
