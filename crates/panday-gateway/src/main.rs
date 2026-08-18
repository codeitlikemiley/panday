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

    let router = match PolicyRouter::from_yaml(DEV_POLICY) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("panday-gateway: bundled policy is invalid: {e}");
            std::process::exit(1);
        }
    };

    let usage = Arc::new(CollectUsage::new());
    let mut builder = Gateway::builder(Arc::new(router)).usage_sink(usage);

    // Same environment contract as the CLI, so one set of variables configures
    // either entry point.
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.trim().is_empty() {
            builder = builder.adapter(
                "anthropic",
                Arc::new(Anthropic::new(key)) as Arc<dyn ProviderAdapter>,
            );
        }
    }
    if let Ok(base) = std::env::var("PANDAY_COMPAT_BASE_URL") {
        if !base.trim().is_empty() {
            let key = std::env::var("PANDAY_COMPAT_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty());
            builder = builder.adapter(
                "together",
                Arc::new(OpenAiCompat::new(base, key)) as Arc<dyn ProviderAdapter>,
            );
        }
    }
    let local = std::env::var("PANDAY_LOCAL_BASE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());
    builder = builder.adapter(
        "local",
        Arc::new(OpenAiCompat::local(local.clone())) as Arc<dyn ProviderAdapter>,
    );

    let gateway = builder.build();
    println!("panday-gateway providers: {:?}", gateway.providers());

    let app = panday_gateway::ingress::router(IngressState {
        gateway: Arc::new(gateway),
        // Accounts arrive with the platform (M17.1); the record shape is
        // already correct so the ledger can adopt it unchanged.
        account: panday_types::id::AccountId::new(),
    });

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
