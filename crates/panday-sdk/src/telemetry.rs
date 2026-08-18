//! Tracing and OTLP export (docs/21, M21.1).
//!
//! docs/21 opens with the thing that shapes this module: "The platform is
//! event-sourced; observability is mostly *projection*, not instrumentation."
//! The event log already answers *what happened* — so traces are for latency
//! and fan-out, and they carry **counts and decisions, never content**.
//!
//! ## The id scheme
//!
//! > "`account_id / session_id / turn_id / seq / request_id / call_id` — UUIDv7,
//! > propagated: AEP envelope → tracing span fields → gateway request → ledger
//! > `source`. One id in hand reaches everything else."
//!
//! [`ids`] is the single place those field names are written down, so a span in
//! the harness and a span in the gateway join on the same keys instead of on
//! two spellings of the same idea.

/// Canonical span field names. Everything joins on these.
pub mod ids {
    pub const ACCOUNT: &str = "account_id";
    pub const SESSION: &str = "session_id";
    pub const TURN: &str = "turn_id";
    pub const SEQ: &str = "seq";
    pub const REQUEST: &str = "request_id";
    pub const CALL: &str = "call_id";
}

/// Field names that carry measurements rather than content.
pub mod fields {
    pub const MODEL: &str = "model";
    pub const PROVIDER: &str = "provider";
    pub const TOOL: &str = "tool";
    pub const INPUT_TOKENS: &str = "input_tokens";
    pub const OUTPUT_TOKENS: &str = "output_tokens";
    pub const CACHE_READ_TOKENS: &str = "cache_read_tokens";
    pub const TOKENS_RAW: &str = "tokens_raw";
    pub const TOKENS_KEPT: &str = "tokens_kept";
    pub const STRATEGY: &str = "strategy";
    pub const GATE: &str = "gate";
    pub const STOP_REASON: &str = "stop_reason";
    pub const MATCHED_RULE: &str = "matched_rule";
    pub const IS_ERROR: &str = "is_error";
}

/// Field names that MUST NOT appear on a span or log at default levels.
///
/// docs/21 §logs: "Content-free by default". This list is what
/// [`content_is_scrubbed`] and the M21.5 audit check against — naming the
/// forbidden keys once means a new span cannot accidentally invent one.
/// Note the absence of `message`: that is **tracing's own** key for an event's
/// static description, not a field of ours. Forbidding it would fail on every
/// log line. Content smuggled *into* a message — `info!("user said {prompt}")` —
/// is caught instead by the value-level check in the telemetry suite, which
/// greps for known secrets. Keys are checked structurally, values by content;
/// neither alone is enough.
pub const FORBIDDEN_CONTENT_FIELDS: &[&str] = &[
    "content",
    "text",
    "prompt",
    "messages",
    "args",
    "arguments",
    "output",
    "raw",
    "body",
    "summary",
    "brief",
    "cmd",
    "path",
];

/// True when `PANDAY_DEBUG_CONTENT` is set — local dev only.
pub fn content_debug_enabled() -> bool {
    std::env::var_os("PANDAY_DEBUG_CONTENT").is_some()
}

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("refusing to start: PANDAY_DEBUG_CONTENT is set but env is production")]
    ContentDebugInProduction,
    #[error("telemetry init: {0}")]
    Init(String),
}

/// Where the process thinks it is running.
fn environment() -> String {
    std::env::var("PANDAY_ENV").unwrap_or_else(|_| "development".to_string())
}

/// Set up JSON-to-stdout tracing, and OTLP export when configured.
///
/// docs/21 §logs: "`tracing` JSON to stdout, shipped by the platform ...
/// Content-free by default; `PANDAY_DEBUG_CONTENT=1` per-service for local dev
/// only (**refuses to start** with it set in `env=production`)."
///
/// That refusal is the whole reason this returns a `Result`: a service that
/// logs prompt content in production is a data incident, and the safe failure
/// is to not boot.
pub fn init(service: &str) -> Result<(), TelemetryError> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;

    if content_debug_enabled() && environment() == "production" {
        return Err(TelemetryError::ContentDebugInProduction);
    }

    let filter = EnvFilter::try_from_env("PANDAY_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,panday=debug"));

    let json = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(true)
        .with_target(true);

    let registry = tracing_subscriber::registry().with(filter).with(json);

    // OTLP is opt-in by endpoint: a service with no collector configured must
    // not spend startup time failing to reach one.
    match std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        Ok(endpoint) if !endpoint.trim().is_empty() => {
            let otlp = otlp_layer(service, &endpoint)?;
            registry
                .with(otlp)
                .try_init()
                .map_err(|e| TelemetryError::Init(e.to_string()))
        }
        _ => registry
            .try_init()
            .map_err(|e| TelemetryError::Init(e.to_string())),
    }
}

fn otlp_layer<S>(
    service: &str,
    endpoint: &str,
) -> Result<impl tracing_subscriber::Layer<S>, TelemetryError>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()
        .map_err(|e| TelemetryError::Init(format!("otlp exporter: {e}")))?;

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name(service.to_string())
                .build(),
        )
        .build();

    let tracer = provider.tracer(service.to_string());
    // Held for the process lifetime; dropping the provider would stop exports.
    Box::leak(Box::new(provider));

    Ok(tracing_opentelemetry::layer().with_tracer(tracer))
}

/// Assert a rendered span or log line carries no content fields.
///
/// Exposed rather than kept in a test so every crate's suite can use the same
/// check — docs/21 M21.5 wants a "grep-proof that no content fields leak into
/// spans/logs at default levels", and one shared predicate is how that stays
/// true as spans are added.
pub fn content_is_scrubbed(rendered: &str) -> Result<(), String> {
    for field in FORBIDDEN_CONTENT_FIELDS {
        // Match the JSON key form, so a *value* that happens to contain the
        // word "text" is not a false positive.
        let key = format!("\"{field}\":");
        if rendered.contains(&key) {
            return Err(format!(
                "span/log carries a content field `{field}`: {rendered}"
            ));
        }
    }
    Ok(())
}
