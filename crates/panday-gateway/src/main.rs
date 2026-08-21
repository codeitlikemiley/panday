//! panday-gateway service binary.
//!
//! Serves the OpenAI-compatible ingress (docs/11 §also serves) so any existing
//! tool can point at panday with a base-URL change and inherit routing and
//! metering. Thin over the library, per docs/02.

use panday_gateway::adapters::openai_compat::OpenAiCompat;
use panday_gateway::{CollectUsage, Gateway, IngressState, ProviderAdapter};
use panday_router::PolicyRouter;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;

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
        .usage_sink(usage.clone() as Arc<dyn panday_gateway::UsageSink>)
        .costs(Arc::new(prices));

    // Live pools: env (one key or PANDAY_*_API_KEYS list), Grok/Claude OAuth,
    // vault rows, and the operator console can add more without a restart.
    let hub = panday_gateway::adapters::pool::CredHub::seed_from_process().await;
    builder = builder
        .adapter("xai", hub.xai.clone() as Arc<dyn ProviderAdapter>)
        .adapter(
            "anthropic",
            hub.anthropic.clone() as Arc<dyn ProviderAdapter>,
        )
        .adapter("openai", hub.openai.clone() as Arc<dyn ProviderAdapter>)
        .adapter("gemini", hub.gemini.clone() as Arc<dyn ProviderAdapter>);
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
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:8081".to_string());
    // Do not advertise local GGUFs unless something is actually listening.
    // A catalog row is not a running llama-server.
    if endpoint_listening(&local) {
        builder = builder.adapter(
            "local",
            Arc::new(OpenAiCompat::local(local)) as Arc<dyn ProviderAdapter>,
        );
    }

    let gateway = Arc::new(builder.build());
    println!("panday-gateway providers: {:?}", gateway.providers());

    // Open by default: this binary is the dev/solo shape (docs/01), where there are no accounts and
    // no keys. The authenticated shape is `IngressState::open(..).with_auth(..).with_rate_limit(..)`,
    // wired by whatever runs the platform alongside it (M17.3) — an ingress that demanded a key
    // before the platform exists would make `panday-gateway` unusable on a laptop.
    let mut state = IngressState::open(
        Arc::clone(&gateway),
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
    let console = panday_gateway::console::router(panday_gateway::console::ConsoleState {
        gateway: Arc::clone(&gateway),
        usage: Arc::clone(&usage),
        listen: addr.clone(),
        hub,
    });
    let app = panday_gateway::ingress::router(state).merge(console);

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("panday-gateway: cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!("panday-gateway listening on {addr}");
    println!("  POST /v1/chat/completions");
    println!("  POST /v1/messages");
    println!("  POST /v1beta/models/{{model}}:generateContent");
    println!("  GET  /                  operator console");
    println!("  GET  /accounts          grok logins + API keys");
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("panday-gateway: {e}");
        std::process::exit(1);
    }
}

/// TCP probe so we do not register `local/` against a catalog row with nothing behind it.
fn endpoint_listening(base: &str) -> bool {
    let trimmed = base.trim();
    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let hostport = without_scheme.split('/').next().unwrap_or(without_scheme);
    let default_port: u16 = if trimmed.starts_with("https://") {
        443
    } else {
        80
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h, p.parse().unwrap_or(default_port))
        }
        _ => (hostport, default_port),
    };
    let Ok(mut addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}
