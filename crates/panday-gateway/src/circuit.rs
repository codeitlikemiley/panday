//! Circuit breakers per (provider, model) (M11.6, docs/11) and per credential
//! (M25.5, docs/25).
//!
//! > "Circuit breaker per (provider, model): trip on error-rate, half-open
//! > probes."
//!
//! Per (provider, model) rather than per provider: one model being overloaded is
//! the common case, and tripping the whole provider would take a healthy pool
//! down with it.
//!
//! ## Why error *rate* and not a failure count
//!
//! A count trips on absolute volume, so a busy route with a 1% error rate opens
//! before a quiet route that is failing every call. The rate is measured over a
//! sliding window of recent outcomes with a minimum sample size — without the
//! minimum, the first failure on a cold route is a 100% error rate and the
//! breaker opens on one bad request.
//!
//! ## Two breakers, one state machine
//!
//! [`Breakers`] guards routes; [`CredentialBreakers`] guards the individual keys
//! and tokens inside one provider's pool. They are separate maps over the *same*
//! [`Core`], because the half-open reservation and the probe-decides-alone rule
//! are subtle enough that a second copy would drift. The route breaker's job is
//! "this model is sick"; the credential breaker's is "this one key is sick, use
//! a sibling" — without which a single dead key's failures accumulate against
//! the whole route and eventually open it for every healthy key too.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Calls flow.
    Closed,
    /// Calls are refused without being attempted.
    Open,
    /// One probe is allowed through to see whether the route recovered.
    HalfOpen,
}

#[derive(Debug, Clone, Copy)]
pub struct BreakerConfig {
    /// Outcomes remembered per route.
    pub window: usize,
    /// Minimum outcomes before the rate is trusted.
    pub min_samples: usize,
    /// Error fraction at or above which the breaker opens.
    pub error_rate: f64,
    /// How long a breaker stays open before a probe is allowed.
    pub cooldown: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            window: 20,
            min_samples: 5,
            error_rate: 0.5,
            cooldown: Duration::from_secs(10),
        }
    }
}

#[derive(Default)]
struct Route {
    outcomes: VecDeque<bool>,
    opened_at: Option<Instant>,
    /// True while a half-open probe is in flight, so a burst of concurrent
    /// requests sends *one* probe rather than all of them.
    probing: bool,
}

/// The breaker state machine. Keyed by an opaque two-part key: what the parts
/// mean is the wrapper's business, and the two wrappers keep separate maps so
/// they cannot collide.
struct Core {
    config: BreakerConfig,
    routes: Mutex<BTreeMap<(String, String), Route>>,
}

impl Core {
    fn new(config: BreakerConfig) -> Self {
        Self {
            config,
            routes: Mutex::new(BTreeMap::new()),
        }
    }

    fn state(&self, k: (String, String)) -> State {
        let routes = self.routes.lock().unwrap();
        let Some(route) = routes.get(&k) else {
            return State::Closed;
        };
        match route.opened_at {
            None => State::Closed,
            Some(at) if at.elapsed() >= self.config.cooldown => State::HalfOpen,
            Some(_) => State::Open,
        }
    }

    fn allow(&self, k: (String, String)) -> bool {
        let mut routes = self.routes.lock().unwrap();
        let route = routes.entry(k).or_default();
        match route.opened_at {
            None => true,
            Some(at) if at.elapsed() >= self.config.cooldown => {
                if route.probing {
                    false
                } else {
                    route.probing = true;
                    true
                }
            }
            Some(_) => false,
        }
    }

    /// Records an outcome. Returns `Some(is_open)` when the caller should
    /// publish the new state, so the metric stays the wrapper's decision.
    fn record(&self, k: (String, String), ok: bool) -> Option<bool> {
        let mut routes = self.routes.lock().unwrap();
        let route = routes.entry(k).or_default();

        let was_probing = route.probing;
        route.probing = false;

        if was_probing {
            // A probe decides on its own, not by rate: the window is full of the
            // failures that opened the breaker, so a rate test would keep it open
            // forever however healthy the route now is.
            if ok {
                route.opened_at = None;
                route.outcomes.clear();
            } else {
                route.opened_at = Some(Instant::now());
            }
            return Some(route.opened_at.is_some());
        }

        route.outcomes.push_back(ok);
        while route.outcomes.len() > self.config.window {
            route.outcomes.pop_front();
        }

        if route.opened_at.is_none()
            && route.outcomes.len() >= self.config.min_samples
            && error_rate(&route.outcomes) >= self.config.error_rate
        {
            route.opened_at = Some(Instant::now());
            return Some(true);
        }
        None
    }

    fn reset(&self, k: (String, String)) {
        self.routes.lock().unwrap().remove(&k);
    }
}

/// Breaker state for every (provider, model) the gateway has called.
pub struct Breakers {
    core: Core,
}

impl Default for Breakers {
    fn default() -> Self {
        Self::new(BreakerConfig::default())
    }
}

impl Breakers {
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            core: Core::new(config),
        }
    }

    /// The state of a route right now, without reserving a probe.
    pub fn state(&self, provider: &str, model: &str) -> State {
        self.core.state(key(provider, model))
    }

    /// May this call proceed? Reserves the half-open probe if it takes one.
    ///
    /// One function rather than "check then mark" so two concurrent callers
    /// cannot both believe they are the probe.
    pub fn allow(&self, provider: &str, model: &str) -> bool {
        self.core.allow(key(provider, model))
    }

    pub fn record_success(&self, provider: &str, model: &str) {
        if let Some(open) = self.core.record(key(provider, model), true) {
            report(provider, open);
        }
    }

    pub fn record_failure(&self, provider: &str, model: &str) {
        if let Some(open) = self.core.record(key(provider, model), false) {
            report(provider, open);
        }
    }

    /// Forget a route's history. Exposed for an operator's kill-switch reset and
    /// for tests; not called on the request path.
    pub fn reset(&self, provider: &str, model: &str) {
        self.core.reset(key(provider, model));
        report(provider, false);
    }
}

/// Breaker state per credential inside one provider's pool (docs/25 M25.5).
///
/// Keyed by the pool's `MemberMeta::id`, which is stable for the life of a
/// credential and is not the secret.
///
/// Deliberately does **not** touch docs/21's `circuit_open` gauge: that series is
/// labelled by provider, and a credential breaker writing to it would fight the
/// route breaker for the same label — one dead key out of four would report the
/// whole provider as open. Operator visibility for these is the console work in
/// M25.6.
pub struct CredentialBreakers {
    core: Core,
}

impl Default for CredentialBreakers {
    fn default() -> Self {
        Self::new(BreakerConfig::default())
    }
}

impl CredentialBreakers {
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            core: Core::new(config),
        }
    }

    pub fn state(&self, credential_id: &str) -> State {
        self.core.state(cred_key(credential_id))
    }

    /// May this credential be tried? Reserves the half-open probe if it takes
    /// one, exactly as the route breaker does.
    pub fn allow(&self, credential_id: &str) -> bool {
        self.core.allow(cred_key(credential_id))
    }

    pub fn record_success(&self, credential_id: &str) {
        self.core.record(cred_key(credential_id), true);
    }

    pub fn record_failure(&self, credential_id: &str) {
        self.core.record(cred_key(credential_id), false);
    }

    /// Forget a credential's history — used when the operator revokes or
    /// replaces it, so a new secret does not inherit the old one's failures.
    pub fn reset(&self, credential_id: &str) {
        self.core.reset(cred_key(credential_id));
    }
}

fn key(provider: &str, model: &str) -> (String, String) {
    (provider.to_string(), model.to_string())
}

fn cred_key(credential_id: &str) -> (String, String) {
    (String::new(), credential_id.to_string())
}

fn error_rate(outcomes: &VecDeque<bool>) -> f64 {
    if outcomes.is_empty() {
        return 0.0;
    }
    outcomes.iter().filter(|ok| !**ok).count() as f64 / outcomes.len() as f64
}

/// docs/21's "circuit state" row. Labelled by provider only — the metric is a
/// health signal an operator watches, and per-model series would multiply it by
/// the catalog.
fn report(provider: &str, open: bool) {
    panday_sdk::metrics::metrics()
        .circuit_open
        .set(&[provider], if open { 1.0 } else { 0.0 });
}
