//! M21.2 — the Prometheus endpoint every service exposes (docs/21 §Metrics).
//!
//! Hand-rolled, and deliberately so. The exposition format is a dozen lines of
//! text and the metric set is fixed by docs/21's table; a metrics facade plus a
//! Prometheus backend is two crates outside the docs/02 dependency table, added
//! to save code we would still have to write (the *choice* of what to measure).
//! If the metric set later outgrows this — exemplars, native histograms, a push
//! gateway — that is the moment to ask for the dependency, with a reason.
//!
//! ## Cardinality is the design constraint
//!
//! docs/21's table asks for "cache-read ratio **per session**" and "$ COGS per
//! session". Those cannot be Prometheus labels: sessions are unbounded, and one
//! series per session is how monitoring systems fall over. So the split is:
//!
//! - **Per-session numbers live in the ledger and the event log**, which is
//!   where a question about one session belongs anyway (`panday replay`).
//! - **Prometheus gets the distribution** — a histogram over sessions — which
//!   is what a dashboard and an alert can actually use.
//!
//! Everything here is labelled only by bounded dimensions: provider, model,
//! pool, rule, tier, stop reason, outcome. Labels are still capped at runtime
//! (`MAX_SERIES_PER_FAMILY`) because a bug that puts an id in a label should
//! degrade the metric, not the process.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Past this many label combinations a family stops creating series and counts
/// the drops instead. A metric that silently stops working is bad; a process
/// that OOMs because a `session_id` reached a label is worse.
pub const MAX_SERIES_PER_FAMILY: usize = 512;

// ── Instruments ──────────────────────────────────────────────────────────────

/// A monotonic counter. Integer-valued (`inc`, `add`) and float-valued
/// (`add_f64`) share one representation: f64 bits, so dollars and counts do not
/// need two types.
#[derive(Default, Debug)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.add_f64(1.0);
    }
    pub fn add(&self, n: u64) {
        self.add_f64(n as f64);
    }
    pub fn add_f64(&self, v: f64) {
        if v <= 0.0 {
            // A counter that went backwards is a bug at the call site, and
            // Prometheus would read the reset as a wrap. Drop it loudly in
            // debug, ignore it in release.
            debug_assert!(v >= 0.0, "counters only go up (got {v})");
            return;
        }
        let mut cur = self.0.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(cur) + v).to_bits();
            match self
                .0
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(observed) => cur = observed,
            }
        }
    }
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// A value that goes up and down: queue depth, circuit state, in-flight calls.
#[derive(Default, Debug)]
pub struct Gauge(AtomicU64);

impl Gauge {
    pub fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// Fixed-bucket cumulative histogram (the classic Prometheus shape).
#[derive(Debug)]
pub struct Histogram {
    bounds: &'static [f64],
    /// One more than `bounds` — the last is `+Inf`.
    counts: Vec<AtomicU64>,
    sum: Counter,
}

impl Histogram {
    pub fn new(bounds: &'static [f64]) -> Self {
        debug_assert!(
            bounds.windows(2).all(|w| w[0] < w[1]),
            "histogram bounds must be sorted"
        );
        Self {
            bounds,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum: Counter::default(),
        }
    }

    pub fn observe(&self, v: f64) {
        // NaN in, nothing out: it would poison `_sum` for the life of the
        // process and there is no bucket it belongs in.
        if v.is_nan() {
            return;
        }
        let idx = self.bounds.partition_point(|b| *b < v);
        self.counts[idx].fetch_add(1, Ordering::Relaxed);
        if v > 0.0 {
            self.sum.add_f64(v);
        }
    }

    pub fn count(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }

    pub fn sum(&self) -> f64 {
        self.sum.get()
    }

    /// Cumulative counts, aligned with `bounds` plus a final `+Inf`.
    fn cumulative(&self) -> Vec<u64> {
        let mut acc = 0;
        self.counts
            .iter()
            .map(|c| {
                acc += c.load(Ordering::Relaxed);
                acc
            })
            .collect()
    }
}

pub const LATENCY_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0];
pub const RATIO_BUCKETS: &[f64] = &[0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 0.99];
pub const DOLLAR_BUCKETS: &[f64] = &[0.001, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0, 25.0];

// ── Families ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
}

/// One metric name, many label combinations.
pub struct Family<M> {
    name: &'static str,
    help: &'static str,
    kind: Kind,
    labels: &'static [&'static str],
    #[allow(clippy::type_complexity)]
    make: Box<dyn Fn() -> M + Send + Sync>,
    series: Mutex<BTreeMap<Vec<String>, std::sync::Arc<M>>>,
    dropped: AtomicU64,
}

impl<M> Family<M> {
    fn new(
        name: &'static str,
        help: &'static str,
        kind: Kind,
        labels: &'static [&'static str],
        make: Box<dyn Fn() -> M + Send + Sync>,
    ) -> Self {
        Self {
            name,
            help,
            kind,
            labels,
            make,
            series: Mutex::new(BTreeMap::new()),
            dropped: AtomicU64::new(0),
        }
    }

    /// The series for these label values, creating it on first use.
    ///
    /// Wrong arity is a programming error, not a runtime condition — but it
    /// must not take the process down in the middle of a turn either, so the
    /// sample is dropped and counted.
    pub fn with(&self, values: &[&str]) -> Option<std::sync::Arc<M>> {
        if values.len() != self.labels.len() {
            // Not an assert: this is instrumentation, and a mislabelled sample
            // must not be able to abort a turn even in a debug build. The drop
            // is counted and shows up in the scrape as
            // `panday_metrics_series_dropped_total`, which is the signal.
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let key: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        let mut series = self.series.lock().unwrap();
        if let Some(m) = series.get(&key) {
            return Some(m.clone());
        }
        if series.len() >= MAX_SERIES_PER_FAMILY {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let m = std::sync::Arc::new((self.make)());
        series.insert(key, m.clone());
        Some(m)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Family<Counter> {
    pub fn counter(
        name: &'static str,
        help: &'static str,
        labels: &'static [&'static str],
    ) -> Self {
        Self::new(
            name,
            help,
            Kind::Counter,
            labels,
            Box::new(Counter::default),
        )
    }
    /// `add`, but a dropped series is not an error the caller has to handle —
    /// instrumentation must never change control flow.
    pub fn inc(&self, values: &[&str]) {
        if let Some(c) = self.with(values) {
            c.inc();
        }
    }
    pub fn add(&self, values: &[&str], n: f64) {
        if let Some(c) = self.with(values) {
            c.add_f64(n);
        }
    }
}

impl Family<Gauge> {
    pub fn gauge(name: &'static str, help: &'static str, labels: &'static [&'static str]) -> Self {
        Self::new(name, help, Kind::Gauge, labels, Box::new(Gauge::default))
    }
    pub fn set(&self, values: &[&str], v: f64) {
        if let Some(g) = self.with(values) {
            g.set(v);
        }
    }
}

impl Family<Histogram> {
    pub fn histogram(
        name: &'static str,
        help: &'static str,
        labels: &'static [&'static str],
        bounds: &'static [f64],
    ) -> Self {
        Self::new(
            name,
            help,
            Kind::Histogram,
            labels,
            Box::new(move || Histogram::new(bounds)),
        )
    }
    pub fn observe(&self, values: &[&str], v: f64) {
        if let Some(h) = self.with(values) {
            h.observe(v);
        }
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

trait Render: Send + Sync {
    fn render(&self, out: &mut String);
    fn dropped(&self) -> u64;
    fn name(&self) -> &'static str;
}

fn header(out: &mut String, name: &str, help: &str, kind: Kind) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(
        out,
        "# TYPE {name} {}",
        match kind {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    );
}

/// Prometheus label values are quoted; a backslash, quote or newline inside one
/// would produce a document no scraper can parse.
fn escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn label_set(labels: &[&str], values: &[String], extra: Option<(&str, String)>) -> String {
    let mut parts: Vec<String> = labels
        .iter()
        .zip(values)
        .map(|(l, v)| format!("{l}=\"{}\"", escape(v)))
        .collect();
    if let Some((k, v)) = extra {
        parts.push(format!("{k}=\"{}\"", escape(&v)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", parts.join(","))
    }
}

/// Prometheus wants `1` and `1.5`, never `inf` for a bucket bound other than
/// `+Inf`, and `NaN` never.
fn num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

impl Render for Family<Counter> {
    fn render(&self, out: &mut String) {
        let series = self.series.lock().unwrap();
        if series.is_empty() {
            return;
        }
        header(out, self.name, self.help, self.kind);
        for (values, c) in series.iter() {
            let _ = writeln!(
                out,
                "{}{} {}",
                self.name,
                label_set(self.labels, values, None),
                num(c.get())
            );
        }
    }
    fn dropped(&self) -> u64 {
        self.dropped()
    }
    fn name(&self) -> &'static str {
        self.name
    }
}

impl Render for Family<Gauge> {
    fn render(&self, out: &mut String) {
        let series = self.series.lock().unwrap();
        if series.is_empty() {
            return;
        }
        header(out, self.name, self.help, self.kind);
        for (values, g) in series.iter() {
            let _ = writeln!(
                out,
                "{}{} {}",
                self.name,
                label_set(self.labels, values, None),
                num(g.get())
            );
        }
    }
    fn dropped(&self) -> u64 {
        self.dropped()
    }
    fn name(&self) -> &'static str {
        self.name
    }
}

impl Render for Family<Histogram> {
    fn render(&self, out: &mut String) {
        let series = self.series.lock().unwrap();
        if series.is_empty() {
            return;
        }
        header(out, self.name, self.help, self.kind);
        for (values, h) in series.iter() {
            let cum = h.cumulative();
            for (i, bound) in h.bounds.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "{}_bucket{} {}",
                    self.name,
                    label_set(self.labels, values, Some(("le", num(*bound)))),
                    cum[i]
                );
            }
            let total = *cum.last().unwrap_or(&0);
            let _ = writeln!(
                out,
                "{}_bucket{} {total}",
                self.name,
                label_set(self.labels, values, Some(("le", "+Inf".into())))
            );
            let _ = writeln!(
                out,
                "{}_sum{} {}",
                self.name,
                label_set(self.labels, values, None),
                num(h.sum())
            );
            let _ = writeln!(
                out,
                "{}_count{} {total}",
                self.name,
                label_set(self.labels, values, None)
            );
        }
    }
    fn dropped(&self) -> u64 {
        self.dropped()
    }
    fn name(&self) -> &'static str {
        self.name
    }
}

// ── The metric set (docs/21 §Metrics) ────────────────────────────────────────

/// Every metric docs/21's table names, plus what it takes to alert on them.
///
/// The table's row order is preserved in the field order, and every row is
/// either a field here or a comment saying which milestone owns it — so a
/// reader can check the spec against the code by scrolling.
pub struct Metrics {
    // "cache-read ratio per session" — the distribution, not one series per
    // session. `pool` is the routing pool, which is what a dashboard slices by.
    pub cache_read_ratio: Family<Histogram>,
    // "$ saved by reducer (est) per session" (docs/15's dollar mandate).
    //
    // The dollar figure is the headline (ADR-007) and it needs a price for the
    // model the tokens would have been sent to. Until the harness has one
    // (M11.4 brings the price table to the loop), `observe_reduction` is called
    // with no dollars and only the token volume lands — which is why the token
    // counter exists next to it rather than instead of it. A token count is not
    // a percentage, and it is not being passed off as the saving.
    pub reducer_saved_usd: Family<Counter>,
    pub reducer_tokens_removed: Family<Counter>,
    // "$ COGS per session / per turn, by pool". Per *call*, not per turn: the
    // gateway sees calls, and a turn is a fold over the log (the ledger and
    // `panday replay` answer per-turn and per-session questions). Naming it
    // `_per_turn` here would put a wrong denominator on a money dashboard.
    pub cost_usd: Family<Counter>,
    pub cost_usd_per_call: Family<Histogram>,
    /// Calls whose model had no configured price. A cost dashboard is only
    /// trustworthy next to this being zero — see `pricing::CostModel`.
    pub unpriced_calls: Family<Counter>,
    // "route decisions by rule + counterfactuals"
    pub route_decisions: Family<Counter>,
    pub route_counterfactual_usd: Family<Counter>,
    // "provider error/latency by (provider, model)" + circuit state (M11.6)
    pub model_calls: Family<Counter>,
    pub model_errors: Family<Counter>,
    pub model_latency_seconds: Family<Histogram>,
    pub circuit_open: Family<Gauge>,
    // "sandbox-seconds by tier" — the second metered good (docs/17, M14.7)
    pub sandbox_seconds: Family<Counter>,
    // "turn stop-reasons distribution"
    pub turn_stop_reasons: Family<Counter>,
    pub turns: Family<Counter>,
    // "ledger vs provider-invoice drift" is M21.4 — it needs the provider
    // usage report to compare against, and there is no ledger yet (M11.4).
    pub tokens: Family<Counter>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            cache_read_ratio: Family::histogram(
                "panday_cache_read_ratio",
                "cache-read tokens as a fraction of input tokens, per turn (ADR-008)",
                &["pool"],
                RATIO_BUCKETS,
            ),
            reducer_saved_usd: Family::counter(
                "panday_reducer_saved_usd_total",
                "estimated dollars not spent because of reduction (docs/15)",
                &["strategy"],
            ),
            reducer_tokens_removed: Family::counter(
                "panday_reducer_tokens_removed_total",
                "tokens the reducer removed, by strategy — volume, not the saving (ADR-007)",
                &["strategy"],
            ),
            cost_usd: Family::counter(
                "panday_cost_usd_total",
                "provider cost of goods sold",
                &["pool", "provider", "model"],
            ),
            cost_usd_per_call: Family::histogram(
                "panday_cost_usd_per_call",
                "distribution of provider cost per model call",
                &["pool"],
                DOLLAR_BUCKETS,
            ),
            unpriced_calls: Family::counter(
                "panday_unpriced_calls_total",
                "model calls with no configured price — COGS is understated by these",
                &["provider", "model"],
            ),
            route_decisions: Family::counter(
                "panday_route_decisions_total",
                "routing decisions by the rule that matched and the pool chosen",
                &["rule", "pool", "task"],
            ),
            route_counterfactual_usd: Family::counter(
                "panday_route_counterfactual_usd_total",
                "what the same traffic would have cost on the default pool — is the router earning its keep",
                &["pool"],
            ),
            model_calls: Family::counter(
                "panday_model_calls_total",
                "model calls by outcome",
                &["provider", "model", "outcome"],
            ),
            model_errors: Family::counter(
                "panday_model_errors_total",
                "model call failures by error kind",
                &["provider", "model", "code"],
            ),
            model_latency_seconds: Family::histogram(
                "panday_model_latency_seconds",
                "time to establish a model stream (not to finish it)",
                &["provider", "model"],
                LATENCY_BUCKETS,
            ),
            circuit_open: Family::gauge(
                "panday_circuit_open",
                "1 while a provider's circuit is open (M11.6)",
                &["provider"],
            ),
            sandbox_seconds: Family::counter(
                "panday_sandbox_seconds_total",
                "wall-clock seconds spent executing in the sandbox, by tier — the second metered good",
                &["tier"],
            ),
            turn_stop_reasons: Family::counter(
                "panday_turn_stop_reasons_total",
                "how turns ended; budget stops climbing is a UX problem brewing",
                &["reason"],
            ),
            turns: Family::counter("panday_turns_total", "turns started", &[]),
            tokens: Family::counter(
                "panday_tokens_total",
                "tokens by direction and cache disposition; cache counts are subsets of input",
                &["provider", "model", "kind"],
            ),
        }
    }

    fn families(&self) -> Vec<&dyn Render> {
        vec![
            &self.cache_read_ratio,
            &self.reducer_saved_usd,
            &self.reducer_tokens_removed,
            &self.cost_usd,
            &self.cost_usd_per_call,
            &self.unpriced_calls,
            &self.route_decisions,
            &self.route_counterfactual_usd,
            &self.model_calls,
            &self.model_errors,
            &self.model_latency_seconds,
            &self.circuit_open,
            &self.sandbox_seconds,
            &self.turn_stop_reasons,
            &self.turns,
            &self.tokens,
        ]
    }

    /// Every metric name this process can emit — including families that are
    /// currently empty, which `render()` deliberately omits.
    ///
    /// This exists so a checked-in dashboard can be tested against the code
    /// (`deploy/grafana-panday.json`): a renamed metric otherwise leaves a panel
    /// silently reading "No data", which is indistinguishable from a healthy
    /// system with nothing happening.
    pub fn names(&self) -> Vec<&'static str> {
        self.families().into_iter().map(|f| f.name()).collect()
    }

    /// The body of `GET /metrics`, in Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut dropped = 0;
        for f in self.families() {
            f.render(&mut out);
            dropped += f.dropped();
        }
        // Self-reporting: a dashboard that shows this above zero is telling you
        // an id reached a label, which is a bug worth an alert.
        header(
            &mut out,
            "panday_metrics_series_dropped_total",
            "samples dropped because a family hit its series cap or got wrong label arity",
            Kind::Counter,
        );
        let _ = writeln!(out, "panday_metrics_series_dropped_total {dropped}");
        out
    }

    /// Record everything one model call implies. One call site so a new metric
    /// cannot be wired into the gateway and forgotten in the harness.
    pub fn observe_call(
        &self,
        provider: &str,
        model: &str,
        pool: &str,
        usage: &panday_types::model::Usage,
        cost_usd: Option<f64>,
        latency: std::time::Duration,
    ) {
        self.model_calls.inc(&[provider, model, "ok"]);
        self.model_latency_seconds
            .observe(&[provider, model], latency.as_secs_f64());
        self.tokens
            .add(&[provider, model, "input"], usage.input_tokens as f64);
        self.tokens
            .add(&[provider, model, "output"], usage.output_tokens as f64);
        self.tokens.add(
            &[provider, model, "cache_read"],
            usage.cache_read_tokens as f64,
        );
        self.tokens.add(
            &[provider, model, "cache_write"],
            (usage.cache_write_tokens + usage.cache_write_1h_tokens) as f64,
        );
        match cost_usd {
            Some(usd) => {
                // A priced-at-zero call (a local model) is still priced: it
                // belongs in the distribution, and only `add` needs the guard
                // because counters reject non-positive deltas.
                if usd > 0.0 {
                    self.cost_usd.add(&[pool, provider, model], usd);
                }
                self.cost_usd_per_call.observe(&[pool], usd);
            }
            None => self.unpriced_calls.inc(&[provider, model]),
        }
        // The ratio is only meaningful when there were input tokens at all; a
        // zero-token call would otherwise pull the distribution toward 0 and
        // make an ADR-008 regression look like normal noise.
        if usage.input_tokens > 0 {
            self.cache_read_ratio.observe(
                &[pool],
                usage.cache_read_tokens as f64 / usage.input_tokens as f64,
            );
        }
    }
}

impl Metrics {
    /// Record what one reduction did. `saved_usd` is `None` when no price is
    /// known for the model the tokens would have gone to — reporting $0 there
    /// would claim the reduction was worthless, which is a different statement.
    pub fn observe_reduction(&self, strategy: &str, removed_tokens: u64, saved_usd: Option<f64>) {
        if removed_tokens > 0 {
            self.reducer_tokens_removed
                .add(&[strategy], removed_tokens as f64);
        }
        // A reduction can be worth *negative* dollars (docs/15: generic elision
        // over error output). A counter cannot go down, so a negative saving is
        // not recorded here — it is a `Savings.net_micros` in the log, and the
        // replay is where you see it. Silently clamping it to zero would erase
        // the one case ADR-007 was written about.
        if let Some(usd) = saved_usd.filter(|u| *u > 0.0) {
            self.reducer_saved_usd.add(&[strategy], usd);
        }
    }

    /// Record one sandbox execution — the second metered good (docs/17).
    pub fn observe_sandbox_exec(&self, tier: &str, duration: std::time::Duration) {
        self.sandbox_seconds.add(&[tier], duration.as_secs_f64());
    }
}

static GLOBAL: OnceLock<Metrics> = OnceLock::new();

/// The process-wide metric set. Free to call from anywhere, including hot paths.
pub fn metrics() -> &'static Metrics {
    GLOBAL.get_or_init(Metrics::new)
}

/// `GET /metrics` body for the global set.
pub fn render() -> String {
    metrics().render()
}

/// Content type Prometheus expects for text exposition.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_only_goes_up() {
        let c = Counter::default();
        c.inc();
        c.add(4);
        c.add_f64(0.5);
        assert_eq!(c.get(), 5.5);
    }

    #[test]
    fn a_histogram_puts_a_value_in_the_first_bucket_that_contains_it() {
        let h = Histogram::new(&[1.0, 2.0, 5.0]);
        for v in [0.5, 1.0, 1.5, 7.0] {
            h.observe(v);
        }
        // le=1 holds {0.5, 1.0} — Prometheus buckets are inclusive of the bound.
        assert_eq!(h.cumulative(), vec![2, 3, 3, 4]);
        assert_eq!(h.count(), 4);
        assert_eq!(h.sum(), 10.0);
    }

    #[test]
    fn a_nan_observation_does_not_poison_the_sum() {
        let h = Histogram::new(&[1.0]);
        h.observe(f64::NAN);
        h.observe(1.0);
        assert_eq!(h.sum(), 1.0);
        assert_eq!(h.count(), 1);
    }

    #[test]
    fn exposition_is_parseable_prometheus_text() {
        let m = Metrics::new();
        m.model_calls.inc(&["anthropic", "claude-sonnet-4-5", "ok"]);
        m.model_latency_seconds
            .observe(&["anthropic", "claude-sonnet-4-5"], 0.42);
        let text = m.render();

        assert!(text.contains("# TYPE panday_model_calls_total counter"));
        assert!(text.contains(
            "panday_model_calls_total{provider=\"anthropic\",model=\"claude-sonnet-4-5\",outcome=\"ok\"} 1"
        ));
        assert!(text.contains("# TYPE panday_model_latency_seconds histogram"));
        assert!(text.contains("le=\"+Inf\""));
        assert!(text.contains("panday_model_latency_seconds_count"));
        // Every line is either a comment or `name[{labels}] value`.
        for line in text.lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let (_, value) = line.rsplit_once(' ').unwrap_or_else(|| panic!("{line:?}"));
            assert!(value.parse::<f64>().is_ok(), "not a sample value: {line:?}");
        }
    }

    #[test]
    fn a_family_with_no_series_emits_nothing() {
        // A scrape full of empty families is a scrape nobody reads, and an
        // unset counter is not the same claim as a zero one.
        let text = Metrics::new().render();
        assert!(!text.contains("panday_model_calls_total{"));
        assert!(text.contains("panday_metrics_series_dropped_total 0"));
    }

    #[test]
    fn label_values_are_escaped() {
        let m = Metrics::new();
        m.model_errors
            .inc(&["local", "q\"3\\b", "connect: \"refused\"\nretry"]);
        let text = m.render();
        assert!(
            !text.contains("q\"3"),
            "an unescaped quote breaks the scrape"
        );
        assert!(text.contains("q\\\"3\\\\b"), "{text}");
        // A newline in a label value would end the sample line early.
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("panday_model_errors_total"))
                .count(),
            1
        );
    }

    #[test]
    fn an_unbounded_label_degrades_the_metric_instead_of_the_process() {
        let m = Metrics::new();
        for i in 0..MAX_SERIES_PER_FAMILY + 50 {
            // Pretend someone put a session id in a label.
            m.model_calls.inc(&["p", &format!("model-{i}"), "ok"]);
        }
        assert_eq!(m.model_calls.dropped(), 50);
        assert!(m
            .render()
            .contains("panday_metrics_series_dropped_total 50"));
    }

    #[test]
    fn wrong_label_arity_is_dropped_not_rendered_as_a_broken_sample() {
        // A sample with two values for a three-label family would render as
        // `name{provider="p",model="q"} 1` — valid text, wrong series, and the
        // silent kind of monitoring bug. It is dropped and counted instead.
        let m = Metrics::new();
        m.model_calls.inc(&["p"]);
        assert_eq!(m.model_calls.dropped(), 1);
        assert!(!m.render().contains("panday_model_calls_total{"));
        assert!(m.render().contains("panday_metrics_series_dropped_total 1"));
    }

    #[test]
    fn each_histogram_family_keeps_its_own_buckets() {
        // The first cut of this shared one thread-local for bucket bounds
        // between families, so whichever family was created last decided the
        // buckets for every family created before it: a cache-read *ratio* was
        // being bucketed on latency bounds, and every value landed in `le=1`.
        let m = Metrics::new();
        m.cache_read_ratio.observe(&["p"], 0.9);
        m.model_latency_seconds.observe(&["p", "q"], 0.9);
        let text = m.render();
        assert!(
            text.contains("panday_cache_read_ratio_bucket{pool=\"p\",le=\"0.9\"} 1"),
            "ratio buckets: {text}"
        );
        assert!(
            text.contains(
                "panday_model_latency_seconds_bucket{provider=\"p\",model=\"q\",le=\"1\"} 1"
            ),
            "latency buckets: {text}"
        );
    }

    #[test]
    fn observe_call_records_every_number_one_call_produces() {
        use panday_types::model::Usage;
        let m = Metrics::new();
        m.observe_call(
            "anthropic",
            "claude-sonnet-4-5",
            "frontier",
            &Usage {
                input_tokens: 1000,
                output_tokens: 100,
                cache_read_tokens: 900,
                cache_write_tokens: 50,
                cache_write_1h_tokens: 0,
            },
            Some(0.012),
            std::time::Duration::from_millis(300),
        );
        let text = m.render();
        assert!(text.contains("kind=\"input\"} 1000"), "{text}");
        assert!(text.contains("kind=\"cache_read\"} 900"), "{text}");
        assert!(text.contains("kind=\"cache_write\"} 50"), "{text}");
        assert!(
            text.contains("panday_cost_usd_total{pool=\"frontier\""),
            "{text}"
        );
        // 900/1000 = 0.9, which is at the le="0.9" bound.
        assert!(
            text.contains("panday_cache_read_ratio_bucket{pool=\"frontier\",le=\"0.9\"} 1"),
            "{text}"
        );
    }

    #[test]
    fn a_zero_token_call_is_left_out_of_the_cache_ratio() {
        use panday_types::model::Usage;
        let m = Metrics::new();
        m.observe_call(
            "local",
            "q",
            "local",
            &Usage::default(),
            Some(0.0),
            std::time::Duration::from_millis(1),
        );
        assert!(
            !m.render().contains("panday_cache_read_ratio"),
            "a call with no input tokens has no ratio to report"
        );
    }

    #[test]
    fn an_unpriced_call_is_counted_rather_than_billed_as_zero() {
        use panday_types::model::Usage;
        let m = Metrics::new();
        m.observe_call(
            "together",
            "some-new-model",
            "cheap",
            &Usage {
                input_tokens: 10,
                ..Usage::default()
            },
            None,
            std::time::Duration::from_millis(1),
        );
        let text = m.render();
        assert!(
            text.contains(
                "panday_unpriced_calls_total{provider=\"together\",model=\"some-new-model\"} 1"
            ),
            "{text}"
        );
        // And it did NOT land in the cost total as a free call.
        assert!(!text.contains("panday_cost_usd_total"), "{text}");
    }

    #[test]
    fn a_reduction_with_no_price_reports_volume_but_not_dollars() {
        let m = Metrics::new();
        m.observe_reduction("cargo_test_v1", 4_000, None);
        let text = m.render();
        assert!(
            text.contains("panday_reducer_tokens_removed_total{strategy=\"cargo_test_v1\"} 4000"),
            "{text}"
        );
        assert!(
            !text.contains("panday_reducer_saved_usd_total"),
            "an unpriced reduction has no dollar figure to report: {text}"
        );
    }

    #[test]
    fn a_negative_saving_is_not_reported_as_zero_dollars() {
        // docs/15: generic elision over error output can cost more than it
        // saves. Clamping that to 0 would erase the case ADR-007 exists for.
        let m = Metrics::new();
        m.observe_reduction("generic_elision", 100, Some(-0.02));
        assert!(!m.render().contains("panday_reducer_saved_usd_total"));
    }

    #[test]
    fn sandbox_seconds_accumulate_by_tier() {
        let m = Metrics::new();
        m.observe_sandbox_exec("t2_os_jail", std::time::Duration::from_millis(1500));
        m.observe_sandbox_exec("t2_os_jail", std::time::Duration::from_millis(500));
        m.observe_sandbox_exec("t0_in_process", std::time::Duration::from_millis(10));
        let text = m.render();
        assert!(
            text.contains("panday_sandbox_seconds_total{tier=\"t2_os_jail\"} 2"),
            "{text}"
        );
        assert!(text.contains("tier=\"t0_in_process\"} 0.01"), "{text}");
    }

    #[test]
    fn the_global_set_is_one_set() {
        metrics().turns.inc(&[]);
        let before = metrics().turns.with(&[]).unwrap().get();
        metrics().turns.inc(&[]);
        assert_eq!(metrics().turns.with(&[]).unwrap().get(), before + 1.0);
    }
}
