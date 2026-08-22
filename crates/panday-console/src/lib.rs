//! Operator console: Leptos islands on the gateway (docs/11).
//!
//! The page is HTML first. WASM hydrates islands only (`hydrate_islands`).
//! Status still renders if the browser never loads `/pkg`.

mod app;
pub mod snapshot;

pub use app::App;
pub use snapshot::Snapshot;

/// Server-render a full HTML document for `path` (`/`, `/models`, `/playground`).
#[cfg(feature = "ssr")]
pub fn render(path: &str, snapshot: Snapshot) -> String {
    use leptos::prelude::*;

    let path = path.to_string();
    let owner = Owner::new();
    let body = owner.with(|| view! { <App path=path snapshot=snapshot /> }.to_html());
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>panday console</title>\n\
         <link rel=\"stylesheet\" href=\"/console/forge.css\">\n\
         <script type=\"module\">\n\
         import init, {{ hydrate }} from '/pkg/panday_console.js';\n\
         init().then(() => hydrate());\n\
         </script>\n\
         </head>\n\
         <body>{body}</body>\n\
         </html>"
    )
}

#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_islands();
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;
    use crate::snapshot::*;

    fn snap() -> Snapshot {
        Snapshot {
            listen: "127.0.0.1:8088".into(),
            providers: vec!["xai".into()],
            grok: Cred {
                present: true,
                fresh: true,
            },
            claude: Cred {
                present: false,
                fresh: false,
            },
            accounts: vec![AccountRow {
                id: "00000000-0000-4000-8000-000000000001".into(),
                provider: "xai".into(),
                kind: "oauth".into(),
                label: "grok-cli-1".into(),
                last4: "aaaa".into(),
                used: 12,
                ceiling: Some(100),
                window_secs: Some(18_000),
                remaining_pct: Some(0.88),
                exhausted: false,
                headroom_pct: Some(0.42),
            }],
            rotate: "failover".into(),
            models: vec![ModelRow {
                id: "xai/grok-4.6".into(),
                context: 64528,
                provenance: "measured".into(),
            }],
            pools: vec![],
            recent: vec![],
        }
    }

    #[test]
    fn overview_names_the_gateway_and_the_model() {
        let html = render("/", snap());
        assert!(html.contains("xai/grok-4.6"), "{html}");
        assert!(html.contains("signed in"), "{html}");
        assert!(html.contains("/console/forge.css"));
        assert!(html.contains("/accounts"), "{html}");
    }

    #[test]
    fn accounts_page_lists_last4_and_not_a_secret() {
        let html = render("/accounts", snap());
        assert!(html.contains("grok-cli-1"), "{html}");
        assert!(html.contains("aaaa"), "{html}");
        assert!(!html.contains("sk-test-aaaa"), "{html}");
        assert!(html.contains("Import Grok CLI"), "{html}");
        assert!(
            html.contains("round_robin")
                || html.contains("round-robin")
                || html.contains("Round-robin"),
            "{html}"
        );
        assert!(
            html.contains("ANTHROPIC") || html.contains("anthropic") || html.contains("API key"),
            "{html}"
        );
    }
}
