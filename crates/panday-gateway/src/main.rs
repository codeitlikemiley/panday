//! panday-gateway service binary.
//!
//! Serves the OpenAI-compatible ingress (docs/11 §also serves) so any existing
//! tool can point at panday with a base-URL change and inherit routing and
//! metering. Thin over the library, per docs/02.

use panday_gateway::adapters::anthropic::Anthropic;
use panday_gateway::adapters::openai_compat::OpenAiCompat;
use panday_gateway::{CollectUsage, Gateway, IngressState, ProviderAdapter};
use panday_router::PolicyRouter;
use std::sync::Arc;

const DEV_POLICY: &str = include_str!("../../panday-router/policy/dev.yaml");

#[tokio::main]
async fn main() {
    // docs/21: JSON tracing to stdout, OTLP when an endpoint is configured.
    // A failure here is fatal on purpose — the one error it returns is
    // "PANDAY_DEBUG_CONTENT is set in production", and booting anyway would
    // mean logging prompt content into a production log.
    if let Err(e) = panday_sdk::telemetry::init("panday-gateway") {
        eprintln!("panday-gateway: {e}");
        std::process::exit(1);
    }

    let addr =
        std::env::var("PANDAY_GATEWAY_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    // Pool patterns resolve against the shipped catalog, which also supplies the prices the COGS
    // metric needs (M12.2). Without it a glob is uncallable and every priced call is unpriced.
    let catalog = panday_router::ModelCatalog::shipped();
    let prices = catalog.price_table();
    let router = match PolicyRouter::from_yaml(DEV_POLICY) {
        Ok(r) => r.with_catalog(catalog),
        Err(e) => {
            eprintln!("panday-gateway: bundled policy is invalid: {e}");
            std::process::exit(1);
        }
    };

    let usage = Arc::new(CollectUsage::new());
    let mut builder = Gateway::builder(Arc::new(router))
        .usage_sink(usage)
        .costs(Arc::new(prices));

    // Same environment contract as the CLI, so one set of variables configures
    // either entry point.
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.trim().is_empty() {
            builder = builder.adapter(
                "anthropic",
                Arc::new(Anthropic::new(key)) as Arc<dyn ProviderAdapter>,
            );
        }
    } else if let Some(tok) = panday_sdk::oauth::claude_code() {
        if tok.still_fresh() {
            builder = builder.adapter(
                "anthropic",
                Arc::new(Anthropic::oauth(tok.access)) as Arc<dyn ProviderAdapter>,
            );
        }
    }
    if let Some(token) = panday_sdk::oauth::grok_access().await {
        builder = builder.adapter(
            "xai",
            Arc::new(OpenAiCompat::new(
                panday_sdk::oauth::xai_api_base(),
                Some(token),
            )) as Arc<dyn ProviderAdapter>,
        );
    }
    let base = std::env::var("PANDAY_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("PANDAY_COMPAT_BASE_URL")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });
    if let Some(base) = base {
        let key = std::env::var("PANDAY_COMPAT_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty());
        builder = builder.adapter(
            "together",
            Arc::new(OpenAiCompat::new(base, key)) as Arc<dyn ProviderAdapter>,
        );
    }
    let local = std::env::var("PANDAY_LOCAL_BASE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());
    builder = builder.adapter(
        "local",
        Arc::new(OpenAiCompat::local(local.clone())) as Arc<dyn ProviderAdapter>,
    );

    let gateway = builder.build();
    println!("panday-gateway providers: {:?}", gateway.providers());

    // Open by default: this binary is the dev/solo shape (docs/01), where there are no accounts and
    // no keys. The authenticated shape is `IngressState::open(..).with_auth(..).with_rate_limit(..)`,
    // wired by whatever runs the platform alongside it (M17.3) — an ingress that demanded a key
    // before the platform exists would make `panday-gateway` unusable on a laptop.
    let mut state = IngressState::open(
        Arc::new(gateway),
        // Accounts arrive with the platform (M17.1); the record shape is
        // already correct so the ledger can adopt it unchanged.
        panday_types::id::AccountId::new(),
    );
    if let Ok(limit) = std::env::var("PANDAY_RATE_LIMIT_PER_MIN") {
        match limit.parse::<u32>() {
            Ok(limit) => {
                state = state.with_rate_limit(Arc::new(
                    panday_gateway::ingress::RateLimiter::per_minute(limit),
                ));
                println!("panday-gateway rate limit: {limit}/min per key");
            }
            Err(_) => {
                eprintln!(
                    "panday-gateway: PANDAY_RATE_LIMIT_PER_MIN wants a number, got `{limit}`"
                );
                std::process::exit(1);
            }
        }
    }
    let app = panday_gateway::ingress::router(state);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("panday-gateway: cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!("panday-gateway listening on {addr} (POST /v1/chat/completions)");
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("panday-gateway: {e}");
        std::process::exit(1);
    }
}
