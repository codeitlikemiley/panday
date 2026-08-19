//! Circuit breakers per (provider, model) (M11.6, docs/11).
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

/// Breaker state for every (provider, model) the gateway has called.
pub struct Breakers {
    config: BreakerConfig,
    routes: Mutex<BTreeMap<(String, String), Route>>,
}

impl Default for Breakers {
    fn default() -> Self {
        Self::new(BreakerConfig::default())
    }
}

impl Breakers {
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            config,
            routes: Mutex::new(BTreeMap::new()),
        }
    }

    /// The state of a route right now, without reserving a probe.
    pub fn state(&self, provider: &str, model: &str) -> State {
        let routes = self.routes.lock().unwrap();
        let Some(route) = routes.get(&key(provider, model)) else {
            return State::Closed;
        };
        match route.opened_at {
            None => State::Closed,
            Some(at) if at.elapsed() >= self.config.cooldown => State::HalfOpen,
            Some(_) => State::Open,
        }
    }

    /// May this call proceed? Reserves the half-open probe if it takes one.
    ///
    /// One function rather than "check then mark" so two concurrent callers
    /// cannot both believe they are the probe.
    pub fn allow(&self, provider: &str, model: &str) -> bool {
        let mut routes = self.routes.lock().unwrap();
        let route = routes.entry(key(provider, model)).or_default();
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

    pub fn record_success(&self, provider: &str, model: &str) {
        self.record(provider, model, true)
    }

    pub fn record_failure(&self, provider: &str, model: &str) {
        self.record(provider, model, false)
    }

    fn record(&self, provider: &str, model: &str, ok: bool) {
        let mut routes = self.routes.lock().unwrap();
        let route = routes.entry(key(provider, model)).or_default();

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
            report(provider, route.opened_at.is_some());
            return;
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
            report(provider, true);
        }
    }

    /// Forget a route's history. Exposed for an operator's kill-switch reset and
    /// for tests; not called on the request path.
    pub fn reset(&self, provider: &str, model: &str) {
        self.routes.lock().unwrap().remove(&key(provider, model));
        report(provider, false);
    }
}

fn key(provider: &str, model: &str) -> (String, String) {
    (provider.to_string(), model.to_string())
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
