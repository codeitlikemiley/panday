//! Credential inner-loop for one provider name (docs/25).
//!
//! The registry still sees one adapter (`"xai"`, `"anthropic"`, …). Members are
//! API keys or OAuth tokens. `RateLimited` / retryable errors walk to the next
//! key; a non-retryable 400 does not. Every member 429s → `RateLimited` so the
//! model chain can still fail over to another provider.
//!
//! [`Rotate::Failover`] always starts at the first key. [`Rotate::RoundRobin`]
//! spreads new requests, then still walks on 429.

use super::anthropic::Anthropic;
use super::openai_compat::OpenAiCompat;
use crate::circuit::CredentialBreakers;
use crate::{AdapterCaps, ProviderAdapter};
use panday_sdk::providers::transport::RatelimitRemaining;
use panday_sdk::providers::RemoteModel;
use panday_sdk::{ItemStream, PandayError};
use panday_types::id::SessionId;
use panday_types::model::ChatRequest;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// How the next request picks its first credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotate {
    /// Always try member 0, then 1, … on 429. Predictable; sticky-ish.
    Failover,
    /// Each new request starts one member further on. Still walks on 429.
    RoundRobin,
    /// Try the credential with the most remaining first (docs/25 M25.8).
    ///
    /// "Most remaining" is the **scarcer** of the operator's declared grant
    /// (M25.6) and the provider's own short-window headroom (M25.7), for the
    /// same reason headroom itself takes the scarcer of requests and tokens: a
    /// credential with a fat monthly grant and a nearly-spent minute window is
    /// about to 429, and picking it because the kinder number looked good would
    /// be choosing the one most likely to fail.
    MostRemaining,
}

impl Rotate {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "failover" | "fail-over" | "ordered" => Some(Rotate::Failover),
            "round_robin" | "round-robin" | "rr" => Some(Rotate::RoundRobin),
            "most_remaining" | "most-remaining" | "remaining" => Some(Rotate::MostRemaining),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Rotate::Failover => "failover",
            Rotate::RoundRobin => "round_robin",
            Rotate::MostRemaining => "most_remaining",
        }
    }
}

/// An operator-declared grant for one credential (docs/25 M25.6).
///
/// Counted in **calls**, not tokens or spend. `UsageRecord` carries no
/// `credential_id`, so tokens cannot be attributed to a credential yet; and a
/// flat-rate seat has no per-call price, so spend would be meaningless for
/// exactly the credentials pooling exists to manage. Calls is also how
/// subscription grants are actually expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    /// Calls permitted per window.
    pub ceiling: u64,
    /// The window the ceiling applies to. Providers differ — a few hours for a
    /// subscription seat, a month for an API key — so it is declared per
    /// credential rather than assumed.
    pub window: std::time::Duration,
}

/// Where a member's adapter reports the ratelimit headers it saw.
///
/// Holds a handle to the pool's shared map rather than to the pool, so there is
/// no reference cycle to reason about and a revoked member's sink simply writes
/// to a key nobody reads.
struct MemberSink {
    map: RemainingMap,
    id: String,
}

impl panday_sdk::providers::transport::RemainingSink for MemberSink {
    fn observe(&self, remaining: RatelimitRemaining) {
        self.map
            .lock()
            .expect("pool remaining")
            .insert(self.id.clone(), remaining);
    }
}

type RemainingMap = Arc<Mutex<std::collections::BTreeMap<String, RatelimitRemaining>>>;

/// What one credential has spent against its grant, right now.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberUsage {
    pub id: String,
    pub used: u64,
    /// `None` when the operator has declared no ceiling. Distinct from a
    /// ceiling of 0: unknown is not the same as exhausted, and a card that
    /// renders "100% remaining" for a credential nobody has measured is a lie.
    pub ceiling: Option<u64>,
    pub window_secs: Option<u64>,
    pub remaining_pct: Option<f64>,
    /// Set when a 429 arrives with nothing left in the window (docs/25).
    /// Cleared when the window rolls.
    pub exhausted: bool,
    /// Fraction of the provider's own short window still available, from its
    /// response headers (docs/25 M25.7). `None` when the upstream sends none —
    /// SuperGrok, Claude Max and Codex OAuth do not, and we do not scrape.
    pub headroom_pct: Option<f64>,
    /// The raw counts behind `headroom_pct`, for an operator who wants the
    /// number rather than the ratio.
    pub headroom_requests: Option<u64>,
    pub headroom_tokens: Option<u64>,
}

/// How much of a credential is left, as one number, for ranking.
///
/// The **scarcer** of the operator's declared grant (M25.6) and the provider's
/// short-window headroom (M25.7). `None` when neither is known — which is the
/// normal case for an OAuth subscription with no declared ceiling, and must not
/// be confused with zero.
fn scarcer(grant_pct: Option<f64>, headroom_pct: Option<f64>) -> Option<f64> {
    match (grant_pct, headroom_pct) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Fraction at or below which a credential is treated as spent for routing.
///
/// Defaults to 0.0, meaning **never omit**. The funnel is opt-in on purpose: an
/// operator's declared ceiling is an estimate, and M25.6 already established
/// that only a 429 proves a credential is actually spent. Omitting a provider
/// from a request's chain on the strength of a guess would turn a wrong estimate
/// into an outage, so an operator has to ask for it with
/// `PANDAY_REMAINING_THRESHOLD=0.05`.
pub fn remaining_threshold() -> f64 {
    std::env::var("PANDAY_REMAINING_THRESHOLD")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
        .unwrap_or(0.0)
}

/// Live counters for one credential's window.
#[derive(Debug, Default)]
struct Meter {
    used: u64,
    window_started: Option<Instant>,
    exhausted: bool,
}

impl Meter {
    /// Rolls the window if it has elapsed, then returns the live counters.
    fn roll(&mut self, grant: Option<Grant>) {
        let Some(grant) = grant else { return };
        let started = *self.window_started.get_or_insert_with(Instant::now);
        if started.elapsed() >= grant.window {
            self.used = 0;
            self.exhausted = false;
            self.window_started = Some(Instant::now());
        }
    }
}

/// Public row. Never contains the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberMeta {
    pub id: String,
    pub provider: String,
    pub kind: String,
    pub label: String,
    pub last4: String,
}

struct Member {
    meta: MemberMeta,
    adapter: Arc<dyn ProviderAdapter>,
    /// Operator-declared, absent until someone declares it (docs/25 M25.6).
    grant: Option<Grant>,
    meter: Meter,
}

/// One credential to try: its stable id, and the adapter holding its secret.
/// The id travels with the adapter because every outcome is recorded against
/// that credential's own breaker (docs/25 M25.5).
type PoolMember = (String, Arc<dyn ProviderAdapter>);

/// Several credentials behind one registry name. Interior-mutable so the
/// operator console can add/revoke without restarting the process.
pub struct PooledAdapter {
    dialect_name: &'static str,
    provider: String,
    rotate: Mutex<Rotate>,
    cursor: Mutex<usize>,
    members: Mutex<Vec<Member>>,
    /// Latest ratelimit headers per credential id (docs/25 M25.7). Separate
    /// from `members` so a sink can write without taking the member lock that
    /// the request path holds.
    remaining: RemainingMap,
    /// One breaker per credential (docs/25 M25.5). Without this, a single dead
    /// key's failures accumulate against the `(provider, model)` breaker and
    /// eventually open the route for every healthy sibling too.
    breakers: CredentialBreakers,
}

impl PooledAdapter {
    pub fn empty(provider: impl Into<String>, dialect_name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            dialect_name,
            provider: provider.into(),
            rotate: Mutex::new(Rotate::Failover),
            cursor: Mutex::new(0),
            members: Mutex::new(Vec::new()),
            remaining: RemainingMap::default(),
            breakers: CredentialBreakers::default(),
        })
    }

    pub fn new(members: Vec<Arc<dyn ProviderAdapter>>) -> Self {
        let dialect_name = members.first().map(|m| m.name()).unwrap_or("openai_compat");
        Self {
            dialect_name,
            provider: String::new(),
            rotate: Mutex::new(Rotate::Failover),
            cursor: Mutex::new(0),
            members: Mutex::new(
                members
                    .into_iter()
                    .map(|adapter| Member {
                        grant: None,
                        meter: Meter::default(),
                        meta: MemberMeta {
                            id: uuid::Uuid::new_v4().to_string(),
                            provider: String::new(),
                            kind: "api_key".into(),
                            label: String::new(),
                            last4: String::new(),
                        },
                        adapter,
                    })
                    .collect(),
            ),
            remaining: RemainingMap::default(),
            breakers: CredentialBreakers::default(),
        }
    }

    pub fn set_rotate(&self, policy: Rotate) {
        *self.rotate.lock().expect("pool rotate") = policy;
    }

    pub fn rotate(&self) -> Rotate {
        *self.rotate.lock().expect("pool rotate")
    }

    pub fn is_empty(&self) -> bool {
        self.members.lock().expect("pool members").is_empty()
    }

    pub fn list(&self) -> Vec<MemberMeta> {
        self.members
            .lock()
            .expect("pool members")
            .iter()
            .map(|m| m.meta.clone())
            .collect()
    }

    pub fn push(
        &self,
        kind: &str,
        label: &str,
        last4: &str,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> MemberMeta {
        let meta = MemberMeta {
            id: uuid::Uuid::new_v4().to_string(),
            provider: self.provider.clone(),
            kind: kind.to_string(),
            label: if label.trim().is_empty() {
                format!("{}-{last4}", self.provider)
            } else {
                label.trim().to_string()
            },
            last4: last4.to_string(),
        };
        let out = meta.clone();
        // The adapter only learns which credential it is when it joins a pool,
        // so the sink is wired here rather than at construction (docs/25 M25.7).
        adapter.set_remaining_sink(Arc::new(MemberSink {
            map: self.remaining.clone(),
            id: meta.id.clone(),
        }));
        self.members.lock().expect("pool members").push(Member {
            meta,
            adapter,
            grant: None,
            meter: Meter::default(),
        });
        out
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut members = self.members.lock().expect("pool members");
        let before = members.len();
        members.retain(|m| m.meta.id != id);
        // Ids are per-member and freshly generated, but clearing the history
        // keeps a revoked credential from leaving a tripped breaker behind for
        // an id that no longer exists (docs/25 M25.5).
        self.breakers.reset(id);
        self.remaining.lock().expect("pool remaining").remove(id);
        before != members.len()
    }

    /// The members to try, in order, plus where to start.
    ///
    /// Ids come back with the adapters because the caller records each outcome
    /// against that credential's breaker (docs/25 M25.5).
    fn snapshot(&self, session: Option<SessionId>) -> (Rotate, usize, Vec<PoolMember>) {
        let rotate = *self.rotate.lock().expect("pool rotate");
        let headers = self.remaining.lock().expect("pool remaining").clone();
        let members = self.members.lock().expect("pool members");
        let n = members.len();
        let start = if n == 0 {
            0
        } else {
            match (rotate, session) {
                // Ranked, not offset: "most remaining" is an ordering over all
                // members, which a start index cannot express. Handled by the
                // caller reordering `adapters`; start stays 0.
                (Rotate::MostRemaining, _) => 0,
                // Failover means "always start at member 0"; a session does not
                // change that, and under Failover the pool is already sticky.
                (Rotate::Failover, _) => 0,
                // Sticky per session (M25.5). Round-robin exists to spread load,
                // but spreading it *within* one conversation makes every turn
                // land on a different account — a cold prompt cache each time,
                // and usage smeared across subscriptions for no gain. Hashing
                // the session spreads by conversation instead of by request.
                //
                // From the UUID bytes, not `DefaultHasher`: that hasher's output
                // is explicitly not stable across releases, and a sticky choice
                // that silently moves on a toolchain bump is not sticky.
                (Rotate::RoundRobin, Some(id)) => (id.0.as_u128() % n as u128) as usize,
                (Rotate::RoundRobin, None) => {
                    let mut c = self.cursor.lock().expect("pool cursor");
                    let i = *c % n;
                    *c = c.wrapping_add(1);
                    i
                }
            }
        };
        let mut adapters: Vec<PoolMember> = members
            .iter()
            .map(|m| (m.meta.id.clone(), m.adapter.clone()))
            .collect();
        if rotate == Rotate::MostRemaining {
            // Rank by what is left, fullest first; unmeasured credentials go
            // last. We rank only what we can measure — placing an unknown
            // *between* two known values would mean inventing a number for it,
            // and this pool refuses to invent numbers everywhere else.
            //
            // The consequence, stated plainly: a credential known to be at 2%
            // is still tried before one nobody has measured, and will probably
            // 429 first. Ordering is not the tool for that — the operator's
            // threshold is, and it *removes* the credential rather than
            // reshuffling it. Two mechanisms, one job each.
            //
            // `sort_by` is stable, so unmeasured credentials keep their
            // configured order among themselves.
            let left: std::collections::BTreeMap<String, Option<f64>> = members
                .iter()
                .map(|m| {
                    let ceiling = m.grant.map(|g| g.ceiling);
                    let grant_pct = ceiling
                        .filter(|c| *c > 0)
                        .map(|c| 1.0 - (m.meter.used.min(c) as f64 / c as f64));
                    let hdr = headers.get(&m.meta.id).copied().unwrap_or_default();
                    (m.meta.id.clone(), scarcer(grant_pct, hdr.headroom_pct()))
                })
                .collect();
            adapters.sort_by(|a, b| {
                let (x, y) = (
                    left.get(&a.0).copied().flatten(),
                    left.get(&b.0).copied().flatten(),
                );
                match (x, y) {
                    (Some(x), Some(y)) => y.total_cmp(&x),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            });
        }
        (rotate, start, adapters)
    }

    /// Declare (or clear) a credential's grant. Operator action — console form
    /// or boot from env (docs/25 M25.6).
    pub fn set_grant(&self, id: &str, grant: Option<Grant>) -> bool {
        let mut members = self.members.lock().expect("pool members");
        let Some(m) = members.iter_mut().find(|m| m.meta.id == id) else {
            return false;
        };
        m.grant = grant;
        // A re-declared ceiling starts a fresh window: the operator is telling us
        // the old number was wrong, and carrying its count forward would report a
        // percentage of a ceiling that never applied.
        m.meter = Meter::default();
        true
    }

    pub fn grant(&self, id: &str) -> Option<Grant> {
        let members = self.members.lock().expect("pool members");
        members
            .iter()
            .find(|m| m.meta.id == id)
            .and_then(|m| m.grant)
    }

    /// What every credential has spent against its grant.
    ///
    /// Takes `&self` but *does* mutate: reading rolls any window that has
    /// elapsed. It has to — a pool that goes quiet would otherwise keep
    /// reporting the last window's count until the next call arrived, which is
    /// exactly when an operator is most likely to be looking at it.
    pub fn usage(&self) -> Vec<MemberUsage> {
        // Snapshot the headers first and drop the lock: holding both at once
        // would order two locks that a sink takes in the other direction.
        let headers = self.remaining.lock().expect("pool remaining").clone();
        let mut members = self.members.lock().expect("pool members");
        members
            .iter_mut()
            .map(|m| {
                let hdr = headers.get(&m.meta.id).copied().unwrap_or_default();
                m.meter.roll(m.grant);
                let ceiling = m.grant.map(|g| g.ceiling);
                MemberUsage {
                    id: m.meta.id.clone(),
                    used: m.meter.used,
                    ceiling,
                    window_secs: m.grant.map(|g| g.window.as_secs()),
                    // Saturating at 0: a provider that let us past our own
                    // declared ceiling should read as "none left", not negative.
                    remaining_pct: ceiling
                        .filter(|c| *c > 0)
                        .map(|c| 1.0 - (m.meter.used.min(c) as f64 / c as f64)),
                    exhausted: m.meter.exhausted,
                    headroom_pct: hdr.headroom_pct(),
                    headroom_requests: hdr.requests,
                    headroom_tokens: hdr.tokens,
                }
            })
            .collect()
    }

    /// Count one attempt against a credential, and publish the outcome.
    fn record_call(&self, id: &str, outcome: &'static str, rate_limited: bool) {
        panday_sdk::metrics::metrics()
            .upstream_calls
            .inc(&[self.metric_provider(), outcome]);
        let mut members = self.members.lock().expect("pool members");
        let Some(m) = members.iter_mut().find(|m| m.meta.id == id) else {
            return;
        };
        m.meter.roll(m.grant);
        m.meter.used = m.meter.used.saturating_add(1);
        // "429 with remaining 0 → exhausted until reset" (docs/25). Only a 429
        // proves it: our own count reaching the ceiling means *we* think it is
        // spent, which is a guess until the provider agrees.
        if rate_limited {
            if let Some(g) = m.grant {
                if m.meter.used >= g.ceiling {
                    m.meter.exhausted = true;
                }
            }
        }
    }

    fn metric_provider(&self) -> &str {
        if self.provider.is_empty() {
            self.dialect_name
        } else {
            &self.provider
        }
    }

    /// Every credential known to be at or below the operator's threshold.
    ///
    /// "Known" is load-bearing: a credential nobody has measured is never
    /// counted as spent, so a pool of undeclared OAuth seats can never funnel
    /// itself out of existence. Returns false when the threshold is 0 (the
    /// default), which is what makes the funnel opt-in.
    pub fn all_below_threshold(&self) -> bool {
        self.all_below(remaining_threshold())
    }

    /// The decision, with the threshold passed in.
    ///
    /// Split from the env read so a test never has to mutate process-wide
    /// state: `set_var` races with any concurrent `env::var` in the same test
    /// binary, which is exactly why edition 2024 made it `unsafe`.
    pub fn all_below(&self, threshold: f64) -> bool {
        if threshold <= 0.0 {
            return false;
        }
        let usage = self.usage();
        !usage.is_empty()
            && usage.iter().all(|u| {
                scarcer(u.remaining_pct, u.headroom_pct).is_some_and(|left| left <= threshold)
            })
    }

    /// Breaker state for one credential, for the console and for tests.
    pub fn credential_state(&self, id: &str) -> crate::circuit::State {
        self.breakers.state(id)
    }
}

/// Last four Unicode scalars of a secret, for display. Never the secret.
pub fn last4(secret: &str) -> String {
    let n = secret.chars().count();
    secret.chars().skip(n.saturating_sub(4)).collect()
}

/// Split a pasted or env list of keys. Comma, semicolon, newline. Not colon
/// (`sk-…` keys are colon-free; paths use colon in `PANDAY_GROK_AUTH`).
pub fn split_keys(raw: &str) -> Vec<String> {
    raw.split([',', ';', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn env_keys(primary: &str, extra: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(v) = std::env::var(primary) {
        out.extend(split_keys(&v));
    }
    if let Ok(v) = std::env::var(extra) {
        out.extend(split_keys(&v));
    }
    out
}

pub fn xai_oauth_adapter(token: impl Into<String>) -> Arc<dyn ProviderAdapter> {
    Arc::new(OpenAiCompat::new(
        panday_sdk::oauth::xai_api_base(),
        Some(token.into()),
    )) as _
}

pub fn xai_key_adapter(key: impl Into<String>) -> Arc<dyn ProviderAdapter> {
    xai_oauth_adapter(key)
}

pub fn openai_key_adapter(base: &str, key: impl Into<String>) -> Arc<dyn ProviderAdapter> {
    Arc::new(OpenAiCompat::new(base, Some(key.into()))) as _
}

pub fn anthropic_key_adapter(key: impl Into<String>) -> Arc<dyn ProviderAdapter> {
    Arc::new(Anthropic::new(key)) as _
}

pub fn anthropic_oauth_adapter(token: impl Into<String>) -> Arc<dyn ProviderAdapter> {
    Arc::new(Anthropic::oauth(token)) as _
}

/// 0 tokens → none; 1+ → a pool (one member is still a pool so the console can add more).
pub fn from_xai_tokens(tokens: Vec<String>) -> Option<Arc<PooledAdapter>> {
    let tokens: Vec<String> = tokens
        .into_iter()
        .filter(|t| !t.trim().is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let pool = PooledAdapter::empty("xai", "openai_compat");
    for (i, t) in tokens.into_iter().enumerate() {
        let label = format!("grok-{}", i + 1);
        let tail = last4(&t);
        pool.push("oauth", &label, &tail, xai_oauth_adapter(t));
    }
    Some(pool)
}

#[async_trait::async_trait]
impl ProviderAdapter for PooledAdapter {
    fn name(&self) -> &'static str {
        self.dialect_name
    }

    fn is_exhausted(&self) -> bool {
        self.all_below_threshold()
    }

    fn capabilities(&self, model: &str) -> AdapterCaps {
        let members = self.members.lock().expect("pool members");
        match members.first() {
            Some(m) => m.adapter.capabilities(model),
            None => AdapterCaps::default(),
        }
    }

    async fn chat(&self, req: ChatRequest) -> Result<ItemStream, PandayError> {
        let (_rotate, start, members) = self.snapshot(req.metadata.session);
        if members.is_empty() {
            return Err(PandayError::Provider {
                upstream: if self.provider.is_empty() {
                    self.dialect_name.to_string()
                } else {
                    self.provider.clone()
                },
                message: "no credentials in this pool".into(),
                retryable: true,
            });
        }
        let n = members.len();
        let mut last_retryable: Option<PandayError> = None;
        let mut all_rate_limited = true;
        let mut soonest_retry_after_ms: Option<u64> = None;
        let mut tried = 0usize;
        for i in 0..n {
            let (id, member) = &members[(start + i) % n];
            // A credential whose breaker is open is skipped without being
            // called: that is the whole point of the breaker, and trying it
            // anyway would spend the caller's latency proving what we know.
            if !self.breakers.allow(id) {
                continue;
            }
            tried += 1;
            match member.chat(req.clone()).await {
                Ok(stream) => {
                    self.breakers.record_success(id);
                    self.record_call(id, "ok", false);
                    return Ok(stream);
                }
                Err(e) if e.is_retryable() => {
                    self.breakers.record_failure(id);
                    let limited = matches!(e, PandayError::RateLimited { .. });
                    self.record_call(id, if limited { "rate_limited" } else { "error" }, limited);
                    if let PandayError::RateLimited {
                        retry_after_ms: wait,
                    } = &e
                    {
                        // 0 means the upstream sent no `Retry-After`, not "retry
                        // now" (docs/25 M25.3). A member that did not say must
                        // not out-vote one that did, so unknowns abstain rather
                        // than collapsing the minimum to zero.
                        if *wait > 0 {
                            soonest_retry_after_ms =
                                Some(soonest_retry_after_ms.map_or(*wait, |s| s.min(*wait)));
                        }
                    } else {
                        all_rate_limited = false;
                    }
                    last_retryable = Some(e);
                }
                // A 400 is the request's fault, not the credential's. Counting
                // it would let one malformed caller open every key in the pool —
                // and it must not spend the operator's grant either, for the same
                // reason. It is still published, labelled `rejected`, because an
                // operator watching a provider needs to see it.
                Err(e) => {
                    panday_sdk::metrics::metrics()
                        .upstream_calls
                        .inc(&[self.metric_provider(), "rejected"]);
                    return Err(e);
                }
            }
        }
        if tried == 0 {
            // Every credential is breaker-open. Retryable, so the model chain
            // walks to another provider rather than reporting this as the
            // caller's problem.
            return Err(PandayError::Provider {
                upstream: if self.provider.is_empty() {
                    self.dialect_name.to_string()
                } else {
                    self.provider.clone()
                },
                message: "every credential in this pool is circuit-open".into(),
                retryable: true,
            });
        }
        if all_rate_limited && last_retryable.is_some() {
            return Err(PandayError::RateLimited {
                retry_after_ms: soonest_retry_after_ms.unwrap_or(0),
            });
        }
        Err(last_retryable.unwrap_or_else(|| PandayError::Provider {
            upstream: self.dialect_name.into(),
            message: "pooled adapter has no members".into(),
            retryable: true,
        }))
    }

    async fn list_models(&self) -> Result<Vec<RemoteModel>, PandayError> {
        let adapters: Vec<Arc<dyn ProviderAdapter>> = {
            let members = self.members.lock().expect("pool members");
            members.iter().map(|m| m.adapter.clone()).collect()
        };
        if adapters.is_empty() {
            return Ok(Vec::new());
        }
        let mut last_retryable: Option<PandayError> = None;
        for adapter in adapters {
            match adapter.list_models().await {
                Ok(models) => return Ok(models),
                Err(e) if e.is_retryable() => last_retryable = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last_retryable.unwrap_or_else(|| PandayError::Provider {
            upstream: self.dialect_name.into(),
            message: "pooled adapter has no members".into(),
            retryable: true,
        }))
    }
}

/// The four provider pools the console and boot share.
#[derive(Clone)]
pub struct CredHub {
    pub xai: Arc<PooledAdapter>,
    pub anthropic: Arc<PooledAdapter>,
    pub openai: Arc<PooledAdapter>,
    pub gemini: Arc<PooledAdapter>,
}

impl CredHub {
    pub fn new() -> Self {
        Self {
            xai: PooledAdapter::empty("xai", "openai_compat"),
            anthropic: PooledAdapter::empty("anthropic", "anthropic"),
            openai: PooledAdapter::empty("openai", "openai_compat"),
            gemini: PooledAdapter::empty("gemini", "openai_compat"),
        }
    }

    pub fn set_rotate(&self, policy: Rotate) {
        self.xai.set_rotate(policy);
        self.anthropic.set_rotate(policy);
        self.openai.set_rotate(policy);
        self.gemini.set_rotate(policy);
    }

    pub fn rotate(&self) -> Rotate {
        self.xai.rotate()
    }

    pub fn accounts(&self) -> Vec<MemberMeta> {
        let mut out = self.xai.list();
        out.extend(self.anthropic.list());
        out.extend(self.openai.list());
        out.extend(self.gemini.list());
        out
    }

    /// Live grant counters for every credential in every pool (docs/25 M25.6).
    pub fn usage(&self) -> Vec<MemberUsage> {
        let mut out = Vec::new();
        for p in [&self.xai, &self.anthropic, &self.openai, &self.gemini] {
            out.extend(p.usage());
        }
        out
    }

    /// Declare a grant against whichever pool holds this credential.
    pub fn set_grant(&self, id: &str, grant: Option<Grant>) -> bool {
        [&self.xai, &self.anthropic, &self.openai, &self.gemini]
            .into_iter()
            .any(|p| p.set_grant(id, grant))
    }

    pub fn pool(&self, provider: &str) -> Option<Arc<PooledAdapter>> {
        match provider {
            "xai" => Some(self.xai.clone()),
            "anthropic" => Some(self.anthropic.clone()),
            "openai" => Some(self.openai.clone()),
            "gemini" => Some(self.gemini.clone()),
            _ => None,
        }
    }

    pub fn remove(&self, id: &str) -> bool {
        self.xai.remove(id)
            || self.anthropic.remove(id)
            || self.openai.remove(id)
            || self.gemini.remove(id)
    }

    pub fn contains_last4(&self, provider: &str, last4: &str) -> bool {
        self.accounts()
            .iter()
            .any(|a| a.provider == provider && a.last4 == last4)
    }
}

impl Default for CredHub {
    fn default() -> Self {
        Self::new()
    }
}

/// `PANDAY_<PROVIDER>_CEILING` — calls permitted per window, e.g. `1000`.
/// `PANDAY_<PROVIDER>_WINDOW` — the window, e.g. `5h`, `30d`, `90m`, `3600`.
///
/// Declared in env rather than only in the console because a ceiling is
/// *configuration*: it must survive a restart. Per-credential persistence in
/// the sealed vault belongs to M25.9, which owns making the vault the boot
/// source of truth; until then env is the durable declaration and the console
/// is a runtime override.
pub fn env_grant(provider: &str) -> Option<Grant> {
    let up = provider.to_ascii_uppercase();
    let ceiling: u64 = std::env::var(format!("PANDAY_{up}_CEILING"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let window = std::env::var(format!("PANDAY_{up}_WINDOW"))
        .ok()
        .and_then(|v| parse_window(&v))
        // A ceiling with no window is a monthly grant far more often than it is
        // anything else, and refusing to guess would leave the operator with a
        // declared ceiling that silently does nothing.
        .unwrap_or(std::time::Duration::from_secs(30 * 24 * 3600));
    Some(Grant { ceiling, window })
}

/// `90` (seconds), `90s`, `15m`, `5h`, `30d`.
pub fn parse_window(raw: &str) -> Option<std::time::Duration> {
    let raw = raw.trim();
    let (digits, mult) = match raw.chars().last()? {
        's' => (&raw[..raw.len() - 1], 1),
        'm' => (&raw[..raw.len() - 1], 60),
        'h' => (&raw[..raw.len() - 1], 3600),
        'd' => (&raw[..raw.len() - 1], 86_400),
        _ => (raw, 1),
    };
    let n: u64 = digits.trim().parse().ok()?;
    (n > 0).then(|| std::time::Duration::from_secs(n.saturating_mul(mult)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CacheStyle;
    use futures_util::StreamExt;
    use panday_sdk::providers::transport::{HttpStreamTransport, SseResponse};
    use panday_types::id::{AccountId, RequestId};
    use panday_types::model::{
        CallMeta, ContentBlock, Message, ModelRef, Role, Sampling, StopReason, StreamItem,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct Spy {
        name: &'static str,
        calls: AtomicUsize,
        script: Script,
    }

    enum Script {
        RateLimited(u64),
        BadRequest,
        Retryable,
        Ok(&'static str),
    }

    impl Spy {
        fn new(name: &'static str, script: Script) -> Arc<Self> {
            Arc::new(Self {
                name,
                calls: AtomicUsize::new(0),
                script,
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for Spy {
        fn name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self, _model: &str) -> AdapterCaps {
            AdapterCaps {
                cache_style: CacheStyle::AutomaticPrefix,
                tools: true,
                ..Default::default()
            }
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ItemStream, PandayError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.script {
                Script::RateLimited(retry_after_ms) => {
                    Err(PandayError::RateLimited { retry_after_ms })
                }
                Script::BadRequest => Err(PandayError::Provider {
                    upstream: self.name.into(),
                    message: "HTTP 400: bad request".into(),
                    retryable: false,
                }),
                Script::Retryable => Err(PandayError::Provider {
                    upstream: self.name.into(),
                    message: "HTTP 503: overloaded".into(),
                    retryable: true,
                }),
                Script::Ok(text) => {
                    let items: Vec<Result<StreamItem, PandayError>> = vec![
                        Ok(StreamItem::Delta { text: text.into() }),
                        Ok(StreamItem::Done {
                            reason: StopReason::EndTurn,
                        }),
                    ];
                    Ok(Box::pin(futures_util::stream::iter(items)))
                }
            }
        }
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: ModelRef("xai/grok-4.6".into()),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
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

    async fn drain_text(adapter: &dyn ProviderAdapter) -> Result<String, PandayError> {
        let mut stream = adapter.chat(req()).await?;
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let StreamItem::Delta { text: t } = item? {
                text.push_str(&t);
            }
        }
        Ok(text)
    }

    fn pool(members: Vec<Arc<Spy>>) -> PooledAdapter {
        PooledAdapter::new(
            members
                .into_iter()
                .map(|m| m as Arc<dyn ProviderAdapter>)
                .collect(),
        )
    }

    /// A pool whose credential breakers trip fast, so a test does not need
    /// twenty calls to open one.
    fn pool_with_quick_breakers(members: Vec<Arc<Spy>>) -> PooledAdapter {
        let mut p = pool(members);
        p.breakers = crate::circuit::CredentialBreakers::new(crate::circuit::BreakerConfig {
            window: 4,
            min_samples: 2,
            error_rate: 0.5,
            cooldown: std::time::Duration::from_secs(30),
        });
        p
    }

    async fn drain_session(
        adapter: &dyn ProviderAdapter,
        session: SessionId,
    ) -> Result<String, PandayError> {
        let mut r = req();
        r.metadata.session = Some(session);
        let mut stream = adapter.chat(r).await?;
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let StreamItem::Delta { text: t } = item? {
                text.push_str(&t);
            }
        }
        Ok(text)
    }

    #[tokio::test]
    async fn rate_limited_member_walks_to_the_next_key() {
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        assert_eq!(adapter.name(), "openai_compat");
        let text = drain_text(&adapter).await.expect("B serves the call");
        assert_eq!(text, "from-b");
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
    }

    #[tokio::test]
    async fn every_member_429_returns_rate_limited() {
        let a = Spy::new("openai_compat", Script::RateLimited(8_000));
        let b = Spy::new("openai_compat", Script::RateLimited(3_000));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let err = drain_text(&adapter).await.expect_err("pool exhausted");
        match &err {
            PandayError::RateLimited { retry_after_ms } => {
                assert_eq!(*retry_after_ms, 3_000, "the soonest Retry-After wins")
            }
            other => panic!("expected RateLimited, got {other}"),
        }
        assert!(err.is_retryable());
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
    }

    // ── M25.8: most_remaining and the funnel ─────────────────────────────────

    #[tokio::test]
    async fn most_remaining_tries_the_fullest_credential_first() {
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        adapter.set_rotate(Rotate::MostRemaining);
        let ids: Vec<String> = adapter.list().into_iter().map(|m| m.id).collect();
        // a is nearly spent, b is fresh.
        adapter.set_grant(&ids[0], Some(grant(10, 3600)));
        adapter.set_grant(&ids[1], Some(grant(10, 3600)));
        for _ in 0..9 {
            adapter.record_call(&ids[0], "ok", false);
        }
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-b");
    }

    #[tokio::test]
    async fn the_scarcer_of_grant_and_headroom_decides() {
        use panday_sdk::providers::transport::RemainingSink;
        // a has a fat grant but almost no short-window headroom; b is middling
        // on both. Choosing on the kinder number would pick a, which is the one
        // about to 429.
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        adapter.set_rotate(Rotate::MostRemaining);
        let ids: Vec<String> = adapter.list().into_iter().map(|m| m.id).collect();
        adapter.set_grant(&ids[0], Some(grant(1000, 3600)));
        adapter.set_grant(&ids[1], Some(grant(1000, 3600)));
        for _ in 0..500 {
            adapter.record_call(&ids[1], "ok", false);
        }
        MemberSink {
            map: adapter.remaining.clone(),
            id: ids[0].clone(),
        }
        .observe(RatelimitRemaining {
            requests: Some(2),
            limit_requests: Some(100),
            ..Default::default()
        });
        // a: min(grant 1.0, headroom 0.02) = 0.02. b: min(grant 0.5, ?) = 0.5.
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-b");
    }

    #[tokio::test]
    async fn ranking_uses_only_what_is_measured_and_puts_unknown_last() {
        // Placing an unknown *between* two known values would mean inventing a
        // number for it. So it goes last, and the operator's threshold — not
        // the ordering — is what handles a credential too empty to be worth
        // trying. This test pins the consequence as well as the rule.
        let measured = Spy::new("openai_compat", Script::Ok("measured"));
        let unknown = Spy::new("openai_compat", Script::Ok("unknown"));
        let adapter = pool(vec![measured.clone(), unknown.clone()]);
        adapter.set_rotate(Rotate::MostRemaining);
        let ids: Vec<String> = adapter.list().into_iter().map(|m| m.id).collect();
        adapter.set_grant(&ids[0], Some(grant(10, 3600)));
        assert_eq!(drain_text(&adapter).await.unwrap(), "measured");

        // Still measured-first even when nearly spent. Known-but-low beats
        // no-information, because we at least know it has something left.
        for _ in 0..9 {
            adapter.record_call(&ids[0], "ok", false);
        }
        assert_eq!(
            drain_text(&adapter).await.unwrap(),
            "measured",
            "ordering ranks what it knows; emptiness is the threshold's job"
        );
    }

    #[tokio::test]
    async fn the_fullest_of_several_measured_credentials_wins() {
        let low = Spy::new("openai_compat", Script::Ok("low"));
        let mid = Spy::new("openai_compat", Script::Ok("mid"));
        let high = Spy::new("openai_compat", Script::Ok("high"));
        let adapter = pool(vec![low.clone(), mid.clone(), high.clone()]);
        adapter.set_rotate(Rotate::MostRemaining);
        let ids: Vec<String> = adapter.list().into_iter().map(|m| m.id).collect();
        for id in &ids {
            adapter.set_grant(id, Some(grant(100, 3600)));
        }
        for _ in 0..90 {
            adapter.record_call(&ids[0], "ok", false);
        }
        for _ in 0..50 {
            adapter.record_call(&ids[1], "ok", false);
        }
        assert_eq!(drain_text(&adapter).await.unwrap(), "high");
    }

    #[tokio::test]
    async fn the_funnel_is_off_unless_the_operator_asks_for_it() {
        // A declared ceiling is an estimate; only a 429 proves a credential is
        // spent (M25.6). Omitting a provider on the strength of a guess would
        // turn a wrong estimate into an outage, so the default omits nothing.
        let a = Spy::new("openai_compat", Script::Ok("hi"));
        let adapter = pool(vec![a]);
        let id = adapter.list()[0].id.clone();
        adapter.set_grant(&id, Some(grant(10, 3600)));
        for _ in 0..10 {
            adapter.record_call(&id, "ok", false);
        }
        assert_eq!(adapter.usage()[0].remaining_pct, Some(0.0));
        assert!(
            !adapter.all_below_threshold(),
            "threshold defaults to 0.0 = never omit"
        );
        assert!(
            adapter.all_below(0.05),
            "…but the decision itself sees it as spent once asked"
        );
    }

    #[tokio::test]
    async fn a_pool_all_below_threshold_reports_itself_exhausted() {
        let a = Spy::new("openai_compat", Script::Ok("hi"));
        let b = Spy::new("openai_compat", Script::Ok("hi"));
        let adapter = pool(vec![a, b]);
        let ids: Vec<String> = adapter.list().into_iter().map(|m| m.id).collect();
        for id in &ids {
            adapter.set_grant(id, Some(grant(10, 3600)));
            for _ in 0..10 {
                adapter.record_call(id, "ok", false);
            }
        }
        assert!(adapter.all_below(0.05));
        // The env-driven entry point defaults to 0.0, so it stays false.
        assert!(!ProviderAdapter::is_exhausted(&adapter));
    }

    #[tokio::test]
    async fn one_healthy_credential_keeps_the_whole_provider_in_play() {
        let spent = Spy::new("openai_compat", Script::Ok("hi"));
        let fresh = Spy::new("openai_compat", Script::Ok("hi"));
        let adapter = pool(vec![spent, fresh]);
        let ids: Vec<String> = adapter.list().into_iter().map(|m| m.id).collect();
        adapter.set_grant(&ids[0], Some(grant(10, 3600)));
        adapter.set_grant(&ids[1], Some(grant(10, 3600)));
        for _ in 0..10 {
            adapter.record_call(&ids[0], "ok", false);
        }
        assert!(!adapter.all_below(0.05));
    }

    #[tokio::test]
    async fn unmeasured_credentials_can_never_funnel_themselves_out() {
        // The OAuth-subscription pool: no declared ceiling, no headers. It must
        // stay callable however aggressive the threshold, because "unknown" is
        // not evidence of being spent.
        let a = Spy::new("openai_compat", Script::Ok("hi"));
        let adapter = pool(vec![a]);
        assert!(!adapter.all_below(0.99));
    }

    // ── M25.7: header overlay ────────────────────────────────────────────────

    #[tokio::test]
    async fn a_credential_with_no_headers_reports_no_headroom() {
        // The OAuth-subscription case. Absent headers must leave the local
        // counters in charge, not read as "nothing left".
        let a = Spy::new("openai_compat", Script::Ok("hi"));
        let adapter = pool(vec![a]);
        let id = adapter.list()[0].id.clone();
        adapter.set_grant(&id, Some(grant(10, 3600)));
        drain_text(&adapter).await.unwrap();
        let u = &adapter.usage()[0];
        assert_eq!(u.headroom_pct, None, "no header, no headroom claim");
        assert_eq!(
            u.remaining_pct,
            Some(0.9),
            "the operator's grant still answers"
        );
    }

    #[tokio::test]
    async fn observed_headers_surface_as_headroom_for_that_credential_only() {
        use panday_sdk::providers::transport::RemainingSink;
        let a = Spy::new("openai_compat", Script::Ok("a"));
        let b = Spy::new("openai_compat", Script::Ok("b"));
        let adapter = pool(vec![a, b]);
        let ids: Vec<String> = adapter.list().into_iter().map(|m| m.id).collect();

        // Stand in for the adapter reporting what it saw on the wire.
        let sink = MemberSink {
            map: adapter.remaining.clone(),
            id: ids[0].clone(),
        };
        sink.observe(RatelimitRemaining {
            requests: Some(20),
            limit_requests: Some(100),
            ..Default::default()
        });

        let usage = adapter.usage();
        let first = usage.iter().find(|u| u.id == ids[0]).unwrap();
        let second = usage.iter().find(|u| u.id == ids[1]).unwrap();
        assert_eq!(first.headroom_pct, Some(0.2));
        assert_eq!(first.headroom_requests, Some(20));
        assert_eq!(
            second.headroom_pct, None,
            "one credential's headers must not be attributed to its sibling"
        );
    }

    #[tokio::test]
    async fn revoking_a_credential_forgets_its_headers() {
        use panday_sdk::providers::transport::RemainingSink;
        let a = Spy::new("openai_compat", Script::Ok("a"));
        let adapter = pool(vec![a]);
        let id = adapter.list()[0].id.clone();
        MemberSink {
            map: adapter.remaining.clone(),
            id: id.clone(),
        }
        .observe(RatelimitRemaining {
            requests: Some(1),
            limit_requests: Some(10),
            ..Default::default()
        });
        assert_eq!(adapter.usage()[0].headroom_pct, Some(0.1));
        adapter.remove(&id);
        assert!(
            adapter.remaining.lock().unwrap().is_empty(),
            "a revoked credential must not leave headroom behind for a reused id"
        );
    }

    // ── M25.6: grants, counters, remaining % ─────────────────────────────────

    fn grant(ceiling: u64, secs: u64) -> Grant {
        Grant {
            ceiling,
            window: std::time::Duration::from_secs(secs),
        }
    }

    #[tokio::test]
    async fn an_undeclared_ceiling_reports_unknown_not_full() {
        // The whole point of the number is to inform a decision. Rendering
        // "100% left" for a credential nobody has measured invites exactly the
        // decision it exists to prevent.
        let a = Spy::new("openai_compat", Script::Ok("hi"));
        let adapter = pool(vec![a]);
        drain_text(&adapter).await.unwrap();
        let u = &adapter.usage()[0];
        assert_eq!(u.used, 1, "calls are still counted without a ceiling");
        assert_eq!(u.ceiling, None);
        assert_eq!(u.remaining_pct, None);
    }

    #[tokio::test]
    async fn remaining_falls_as_the_grant_is_spent() {
        let a = Spy::new("openai_compat", Script::Ok("hi"));
        let adapter = pool(vec![a]);
        let id = adapter.list()[0].id.clone();
        assert!(adapter.set_grant(&id, Some(grant(4, 3600))));

        for expected in [0.75, 0.50, 0.25, 0.0] {
            drain_text(&adapter).await.unwrap();
            let got = adapter.usage()[0].remaining_pct.unwrap();
            assert!(
                (got - expected).abs() < f64::EPSILON,
                "expected {expected}, got {got}"
            );
        }
    }

    #[tokio::test]
    async fn overshooting_the_ceiling_reads_as_none_left_not_negative() {
        // A declared ceiling is the operator's estimate; the provider may well
        // let us past it. Reporting -50% would be arithmetic, not information.
        let a = Spy::new("openai_compat", Script::Ok("hi"));
        let adapter = pool(vec![a]);
        let id = adapter.list()[0].id.clone();
        adapter.set_grant(&id, Some(grant(2, 3600)));
        for _ in 0..5 {
            drain_text(&adapter).await.unwrap();
        }
        assert_eq!(adapter.usage()[0].remaining_pct, Some(0.0));
        assert_eq!(adapter.usage()[0].used, 5, "the real count is still shown");
    }

    #[test]
    fn a_meter_rolls_only_after_its_window_elapses() {
        use std::time::Duration;
        // Tested on `Meter` directly rather than through the pool: `usage()`
        // rolls as a side effect of reading, so a pool-level test cannot see
        // the pre-roll state it is trying to assert on.
        let mut m = Meter {
            used: 7,
            window_started: Some(Instant::now()),
            exhausted: true,
        };
        m.roll(Some(grant(10, 3600)));
        assert_eq!(m.used, 7, "an unelapsed window keeps its count");
        assert!(m.exhausted, "and stays exhausted");

        // `sleep` guarantees a *lower* bound, so this cannot fire early — the
        // assertion is that the window elapsed, not that it elapsed on time.
        // A five-fold margin over a 1ms window keeps it off the flake list.
        std::thread::sleep(Duration::from_millis(5));
        m.roll(Some(Grant {
            ceiling: 10,
            window: Duration::from_millis(1),
        }));
        assert_eq!(m.used, 0, "an elapsed window starts clean");
        assert!(
            !m.exhausted,
            "exhausted clears with the window it applied to"
        );
    }

    #[tokio::test]
    async fn a_meter_with_no_grant_never_rolls() {
        // No declared ceiling means no window to roll; the raw count still
        // accumulates so the console can show activity.
        let a = Spy::new("openai_compat", Script::Ok("hi"));
        let adapter = pool(vec![a]);
        for _ in 0..3 {
            drain_text(&adapter).await.unwrap();
        }
        assert_eq!(adapter.usage()[0].used, 3);
        assert_eq!(adapter.usage()[0].remaining_pct, None);
    }

    #[tokio::test]
    async fn redeclaring_a_ceiling_starts_a_fresh_window() {
        // Carrying the old count forward would report a percentage of a ceiling
        // that never applied to it.
        let a = Spy::new("openai_compat", Script::Ok("hi"));
        let adapter = pool(vec![a]);
        let id = adapter.list()[0].id.clone();
        adapter.set_grant(&id, Some(grant(10, 3600)));
        for _ in 0..3 {
            drain_text(&adapter).await.unwrap();
        }
        assert_eq!(adapter.usage()[0].used, 3);
        adapter.set_grant(&id, Some(grant(100, 3600)));
        assert_eq!(adapter.usage()[0].used, 0);
        assert_eq!(adapter.usage()[0].remaining_pct, Some(1.0));
    }

    #[tokio::test]
    async fn only_a_429_marks_a_credential_exhausted() {
        // Our own count reaching the ceiling means *we* think it is spent. That
        // is a guess until the provider agrees.
        let a = Spy::new("openai_compat", Script::Ok("hi"));
        let adapter = pool(vec![a]);
        let id = adapter.list()[0].id.clone();
        adapter.set_grant(&id, Some(grant(1, 3600)));
        for _ in 0..3 {
            drain_text(&adapter).await.unwrap();
        }
        assert!(
            !adapter.usage()[0].exhausted,
            "past our own ceiling is not proof the provider agrees"
        );

        let b = Spy::new("openai_compat", Script::RateLimited(0));
        let limited = pool(vec![b]);
        let bid = limited.list()[0].id.clone();
        limited.set_grant(&bid, Some(grant(1, 3600)));
        let _ = drain_text(&limited).await;
        assert!(
            limited.usage()[0].exhausted,
            "a 429 at the ceiling is proof"
        );
    }

    #[tokio::test]
    async fn a_rejected_request_does_not_spend_the_grant() {
        // Same reason a 400 does not walk the pool or trip a breaker: it is the
        // caller's fault, and one malformed client must not burn an operator's
        // subscription.
        let a = Spy::new("openai_compat", Script::BadRequest);
        let adapter = pool(vec![a]);
        let id = adapter.list()[0].id.clone();
        adapter.set_grant(&id, Some(grant(10, 3600)));
        for _ in 0..4 {
            let _ = drain_text(&adapter).await;
        }
        assert_eq!(adapter.usage()[0].used, 0);
        assert_eq!(adapter.usage()[0].remaining_pct, Some(1.0));
    }

    #[test]
    fn windows_parse_in_the_units_an_operator_types() {
        use std::time::Duration;
        assert_eq!(parse_window("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_window("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_window(" 15m "), Some(Duration::from_secs(900)));
        assert_eq!(parse_window("5h"), Some(Duration::from_secs(18_000)));
        assert_eq!(parse_window("30d"), Some(Duration::from_secs(2_592_000)));
        assert_eq!(parse_window(""), None);
        assert_eq!(parse_window("soon"), None);
        // Zero is not a window; it would make every call its own period.
        assert_eq!(parse_window("0d"), None);
    }

    // ── M25.5: sticky sessions ───────────────────────────────────────────────

    #[tokio::test]
    async fn one_session_lands_on_the_same_credential_every_turn() {
        // Round-robin exists to spread load across accounts. Spreading it
        // *within* one conversation gives every turn a cold prompt cache and
        // smears one user's usage across subscriptions for no gain.
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let c = Spy::new("openai_compat", Script::Ok("from-c"));
        let adapter = pool(vec![a.clone(), b.clone(), c.clone()]);
        adapter.set_rotate(Rotate::RoundRobin);

        let session = SessionId::new();
        let first = drain_session(&adapter, session).await.unwrap();
        for _ in 0..6 {
            assert_eq!(
                drain_session(&adapter, session).await.unwrap(),
                first,
                "a session must not hop credentials between turns"
            );
        }
        // All seven turns went to one member.
        let calls = [a.calls(), b.calls(), c.calls()];
        assert!(
            calls.contains(&7) && calls.iter().filter(|c| **c == 0).count() == 2,
            "expected one member to take all 7 turns, got {calls:?}"
        );
    }

    #[tokio::test]
    async fn different_sessions_still_spread_across_credentials() {
        // Sticky must not collapse into "everyone gets member 0" — that would
        // trade load-spreading away entirely rather than moving it from
        // per-request to per-conversation.
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        adapter.set_rotate(Rotate::RoundRobin);

        // Enough distinct sessions that both members being used is overwhelming
        // (a fair hash misses only 1-in-2^39), and deterministic per session.
        for _ in 0..40 {
            drain_session(&adapter, SessionId::new()).await.unwrap();
        }
        assert!(
            a.calls() > 0 && b.calls() > 0,
            "sessions should distribute: a={} b={}",
            a.calls(),
            b.calls()
        );
    }

    #[tokio::test]
    async fn failover_ignores_the_session_because_it_is_already_sticky() {
        // Failover's contract is "always start at member 0". A session must not
        // quietly redefine that.
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        for _ in 0..5 {
            assert_eq!(
                drain_session(&adapter, SessionId::new()).await.unwrap(),
                "from-a"
            );
        }
        assert_eq!(b.calls(), 0);
    }

    #[tokio::test]
    async fn a_sticky_session_still_walks_when_its_credential_is_rate_limited() {
        // Sticky is a preference, not a pin: a 429 must still fail over.
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        adapter.set_rotate(Rotate::RoundRobin);
        assert_eq!(
            drain_session(&adapter, SessionId::new()).await.unwrap(),
            "from-b"
        );
    }

    // ── M25.5: per-credential breakers ───────────────────────────────────────

    #[tokio::test]
    async fn a_failing_credential_is_dropped_without_being_called_again() {
        let bad = Spy::new("openai_compat", Script::Retryable);
        let good = Spy::new("openai_compat", Script::Ok("from-good"));
        let adapter = pool_with_quick_breakers(vec![bad.clone(), good.clone()]);

        for _ in 0..4 {
            assert_eq!(drain_text(&adapter).await.unwrap(), "from-good");
        }
        let after_open = bad.calls();
        for _ in 0..4 {
            assert_eq!(drain_text(&adapter).await.unwrap(), "from-good");
        }
        assert_eq!(
            bad.calls(),
            after_open,
            "an open credential must stop being dialled, not just fail faster"
        );
        assert_eq!(
            adapter.credential_state(&adapter.list()[0].id),
            crate::circuit::State::Open
        );
    }

    #[tokio::test]
    async fn one_dead_credential_does_not_open_its_healthy_siblings() {
        // The reason per-credential breakers exist: with only the
        // (provider, model) breaker, a dead key's failures accumulate against
        // the whole route and eventually take the healthy keys down with it.
        let bad = Spy::new("openai_compat", Script::Retryable);
        let good = Spy::new("openai_compat", Script::Ok("from-good"));
        let adapter = pool_with_quick_breakers(vec![bad.clone(), good.clone()]);

        for _ in 0..8 {
            assert_eq!(drain_text(&adapter).await.unwrap(), "from-good");
        }
        let ids: Vec<String> = adapter.list().into_iter().map(|m| m.id).collect();
        assert_eq!(
            adapter.credential_state(&ids[0]),
            crate::circuit::State::Open
        );
        assert_eq!(
            adapter.credential_state(&ids[1]),
            crate::circuit::State::Closed,
            "the healthy sibling must stay closed"
        );
    }

    #[tokio::test]
    async fn a_pool_with_every_credential_open_is_retryable_so_the_chain_walks() {
        let a = Spy::new("openai_compat", Script::Retryable);
        let b = Spy::new("openai_compat", Script::Retryable);
        let adapter = pool_with_quick_breakers(vec![a.clone(), b.clone()]);

        for _ in 0..4 {
            let _ = drain_text(&adapter).await;
        }
        let calls_before = a.calls() + b.calls();
        let err = drain_text(&adapter).await.expect_err("all open");
        assert!(
            err.is_retryable(),
            "must stay retryable so the model chain fails over: {err}"
        );
        assert_eq!(
            a.calls() + b.calls(),
            calls_before,
            "nothing should be dialled once every credential is open"
        );
    }

    #[tokio::test]
    async fn a_bad_request_does_not_count_against_the_credential() {
        // A 400 is the caller's fault. Counting it would let one malformed
        // client open every key in the pool for everybody else.
        let a = Spy::new("openai_compat", Script::BadRequest);
        let adapter = pool_with_quick_breakers(vec![a.clone()]);
        for _ in 0..6 {
            let _ = drain_text(&adapter).await;
        }
        assert_eq!(
            adapter.credential_state(&adapter.list()[0].id),
            crate::circuit::State::Closed
        );
        assert_eq!(a.calls(), 6, "every attempt should still have been made");
    }

    #[tokio::test]
    async fn a_member_that_sent_no_retry_after_does_not_erase_one_that_did() {
        // 0 is "the upstream sent no header", not a zero-millisecond wait.
        // Folding it into the minimum would throw away the only real signal.
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::RateLimited(60_000));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let err = drain_text(&adapter).await.expect_err("pool exhausted");
        match &err {
            PandayError::RateLimited { retry_after_ms } => assert_eq!(
                *retry_after_ms, 60_000,
                "the known wait survives an unknown sibling"
            ),
            other => panic!("expected RateLimited, got {other}"),
        }
    }

    #[tokio::test]
    async fn a_pool_where_nobody_stated_a_wait_reports_zero() {
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::RateLimited(0));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let err = drain_text(&adapter).await.expect_err("pool exhausted");
        match &err {
            PandayError::RateLimited { retry_after_ms } => assert_eq!(
                *retry_after_ms, 0,
                "no header anywhere means no wait to report, not an invented one"
            ),
            other => panic!("expected RateLimited, got {other}"),
        }
    }

    #[tokio::test]
    async fn a_400_does_not_walk_keys() {
        let a = Spy::new("openai_compat", Script::BadRequest);
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let err = drain_text(&adapter).await.expect_err("caller error");
        assert!(!err.is_retryable());
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 0, "a 400 must not spend the next credential");
    }

    #[tokio::test]
    async fn retryable_provider_error_walks_like_429() {
        let a = Spy::new("openai_compat", Script::Retryable);
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        let text = drain_text(&adapter).await.expect("B serves");
        assert_eq!(text, "from-b");
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
    }

    #[test]
    fn from_xai_tokens_matches_boot_shape() {
        assert!(from_xai_tokens(vec![]).is_none());
        assert!(from_xai_tokens(vec![String::new()]).is_none());
        let one = from_xai_tokens(vec!["sk-test-aaaa".into()]).expect("one token");
        assert_eq!(one.name(), "openai_compat");
        let two = from_xai_tokens(vec!["sk-test-aaaa".into(), "sk-test-bbbb".into()])
            .expect("two tokens");
        assert_eq!(two.name(), "openai_compat");
    }

    /// Minimal scripted HTTP: fail with an error, or replay one SSE body.
    struct MockTransport {
        fail: Mutex<Option<fn() -> PandayError>>,
        body: Vec<u8>,
        calls: AtomicUsize,
    }

    impl MockTransport {
        fn failing(err: fn() -> PandayError) -> Arc<Self> {
            Arc::new(Self {
                fail: Mutex::new(Some(err)),
                body: Vec::new(),
                calls: AtomicUsize::new(0),
            })
        }
        fn streaming(body: &[u8]) -> Arc<Self> {
            Arc::new(Self {
                fail: Mutex::new(None),
                body: body.to_vec(),
                calls: AtomicUsize::new(0),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl HttpStreamTransport for MockTransport {
        async fn post_sse(
            &self,
            _url: &str,
            _headers: &[(String, String)],
            _body: Vec<u8>,
        ) -> Result<SseResponse, PandayError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(err) = *self.fail.lock().unwrap() {
                return Err(err());
            }
            Ok(SseResponse {
                headers: Default::default(),
                body: Box::pin(futures_util::stream::iter(vec![Ok(self.body.clone())])),
            })
        }
    }

    const FROM_B: &[u8] =
        br#"data: {"choices":[{"index":0,"delta":{"content":"from-b"},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#;

    #[tokio::test]
    async fn mock_transport_429_then_sse_from_the_second_key() {
        let a_http = MockTransport::failing(|| PandayError::RateLimited { retry_after_ms: 0 });
        let b_http = MockTransport::streaming(FROM_B);
        let a = OpenAiCompat::with_transport(
            "https://api.x.ai",
            Some("sk-test-aaaa".into()),
            a_http.clone(),
        );
        let b = OpenAiCompat::with_transport(
            "https://api.x.ai",
            Some("sk-test-bbbb".into()),
            b_http.clone(),
        );
        let adapter = PooledAdapter::new(vec![Arc::new(a) as _, Arc::new(b) as _]);
        let text = drain_text(&adapter).await.expect("B's SSE");
        assert_eq!(text, "from-b");
        assert_eq!(a_http.calls(), 1);
        assert_eq!(b_http.calls(), 1);
    }

    #[test]
    fn split_keys_accepts_comma_semicolon_and_newlines() {
        assert_eq!(
            split_keys("sk-test-aaaa, sk-test-bbbb"),
            vec!["sk-test-aaaa", "sk-test-bbbb"]
        );
        assert_eq!(
            split_keys("sk-test-aaaa;\nsk-test-bbbb\n"),
            vec!["sk-test-aaaa", "sk-test-bbbb"]
        );
        assert!(split_keys("  \n , ; ").is_empty());
        assert_eq!(last4("sk-test-aaaa"), "aaaa");
    }

    #[tokio::test]
    async fn empty_pool_is_retryable_so_the_model_chain_can_walk() {
        let pool = PooledAdapter::empty("xai", "openai_compat");
        let err = drain_text(pool.as_ref()).await.expect_err("empty");
        assert!(err.is_retryable(), "{err}");
        assert!(pool.list_models().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn round_robin_starts_on_the_next_member() {
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        adapter.set_rotate(Rotate::RoundRobin);
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-a");
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-b");
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-a");
        assert_eq!(a.calls(), 2);
        assert_eq!(b.calls(), 1);
    }

    #[tokio::test]
    async fn round_robin_still_walks_on_429() {
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::Ok("from-b"));
        let adapter = pool(vec![a.clone(), b.clone()]);
        adapter.set_rotate(Rotate::RoundRobin);
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-b");
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
    }

    #[tokio::test]
    async fn three_api_keys_failover_skips_two_429s() {
        let a = Spy::new("openai_compat", Script::RateLimited(0));
        let b = Spy::new("openai_compat", Script::RateLimited(0));
        let c = Spy::new("openai_compat", Script::Ok("from-c"));
        let adapter = pool(vec![a.clone(), b.clone(), c.clone()]);
        assert_eq!(drain_text(&adapter).await.unwrap(), "from-c");
        assert_eq!(a.calls(), 1);
        assert_eq!(b.calls(), 1);
        assert_eq!(c.calls(), 1);
    }

    #[tokio::test]
    async fn live_add_and_remove_change_who_serves() {
        let pool = PooledAdapter::empty("openai", "openai_compat");
        let a = Spy::new("openai_compat", Script::Ok("from-a"));
        pool.push(
            "api_key",
            "paid",
            "aaaa",
            a.clone() as Arc<dyn ProviderAdapter>,
        );
        assert_eq!(drain_text(pool.as_ref()).await.unwrap(), "from-a");
        let id = pool.list()[0].id.clone();
        assert!(pool.remove(&id));
        assert!(pool.is_empty());
        let err = drain_text(pool.as_ref()).await.expect_err("removed");
        assert!(err.is_retryable());
    }

    #[test]
    fn rotate_parse() {
        assert_eq!(Rotate::parse("round-robin"), Some(Rotate::RoundRobin));
        assert_eq!(Rotate::parse("failover"), Some(Rotate::Failover));
        assert!(Rotate::parse("random").is_none());
    }
}
