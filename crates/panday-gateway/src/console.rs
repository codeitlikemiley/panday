//! Mounts the Leptos operator console on the gateway process (docs/11).

use crate::adapters::pool::{parse_window, CredHub, Grant, Rotate};
use crate::creds::{persist_revoke, persist_secret};
use crate::gateway::{CollectUsage, Gateway};
use axum::extract::{Form, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use panday_console::snapshot::{AccountRow, CallRow, Cred, ModelRow, PoolRow, Snapshot};
use panday_sdk::oauth::Token;
use panday_sdk::vault::Kind;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct ConsoleState {
    pub gateway: Arc<Gateway>,
    pub usage: Arc<CollectUsage>,
    pub listen: String,
    pub hub: CredHub,
}

pub fn router(state: ConsoleState) -> Router {
    let mut app = Router::new()
        .route("/", get(page))
        .route("/models", get(page))
        .route("/playground", get(page))
        .route("/accounts", get(page))
        .route("/console/forge.css", get(css))
        .route("/console/try", post(try_prompt))
        .route("/console/snapshot", get(snapshot_json))
        .route("/console/accounts/import-grok", post(import_grok))
        .route("/console/accounts/paste-grok", post(paste_grok))
        .route("/console/accounts/api", post(add_api))
        .route("/console/accounts/revoke", post(revoke_account))
        .route("/console/accounts/rotate", post(set_rotate))
        .route("/console/accounts/ceiling", post(set_ceiling))
        .with_state(state);

    if let Some(pkg) = pkg_dir() {
        app = app.nest_service("/pkg", tower_http::services::ServeDir::new(pkg));
    }
    app
}

fn pkg_dir() -> Option<PathBuf> {
    let p = std::env::var("PANDAY_CONSOLE_PKG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target/site/pkg"));
    p.is_dir().then_some(p)
}

async fn page(State(state): State<ConsoleState>, uri: axum::http::Uri) -> Html<String> {
    Html(panday_console::render(uri.path(), snapshot(&state).await))
}

async fn css() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../panday-console/style/forge.css"),
    )
        .into_response()
}

async fn snapshot_json(State(state): State<ConsoleState>) -> axum::Json<Snapshot> {
    axum::Json(snapshot(&state).await)
}

#[derive(Deserialize)]
struct TryForm {
    model: String,
    prompt: String,
}

async fn try_prompt(State(state): State<ConsoleState>, Form(form): Form<TryForm>) -> Html<String> {
    match run_try(&state, &form).await {
        Ok(text) => Html(format!(
            "<!doctype html><meta charset=utf-8><link rel=stylesheet href=/console/forge.css>\
             <body class=shell><main><h1>reply</h1><pre class=reply>{}</pre>\
             <p><a href=/playground>back</a></p></main></body>",
            escape(&text)
        )),
        Err(e) => Html(format!(
            "<!doctype html><meta charset=utf-8><link rel=stylesheet href=/console/forge.css>\
             <body class=shell><main><h1>call failed</h1><pre class='reply error'>{}</pre>\
             <p><a href=/playground>back</a></p></main></body>",
            escape(&e)
        )),
    }
}

async fn run_try(state: &ConsoleState, form: &TryForm) -> Result<String, String> {
    use futures_util::StreamExt;
    use panday_sdk::ModelClient;
    use panday_types::model::{
        CallMeta, ChatRequest, ContentBlock, Message, ModelRef, Role, Sampling, StreamItem,
    };

    let req = ChatRequest {
        model: ModelRef(form.model.clone()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: form.prompt.clone(),
            }],
            call_id: None,
            provider_call_id: None,
        }],
        tools: vec![],
        sampling: Sampling {
            temperature: Some(0.0),
            max_tokens: Some(256),
            ..Default::default()
        },
        cache: Default::default(),
        stream: true,
        metadata: CallMeta {
            account: panday_types::id::AccountId::new(),
            request: panday_types::id::RequestId::new(),
            session: None,
            turn: None,
            task: None,
        },
    };
    let mut stream = state.gateway.chat(req).await.map_err(|e| e.to_string())?;
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let StreamItem::Delta { text: d } = item.map_err(|e| e.to_string())? {
            text.push_str(&d);
        }
    }
    Ok(text)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn accounts_redirect() -> Redirect {
    Redirect::to("/accounts")
}

async fn import_grok(State(state): State<ConsoleState>) -> Redirect {
    let tokens = panday_sdk::oauth::grok_access_all().await;
    for (i, token) in tokens.into_iter().enumerate() {
        let label = format!("grok-cli-{}", i + 1);
        if let Ok(meta) = state.hub.add_secret("xai", "oauth", &label, &token) {
            let _ = persist_secret("xai", Kind::Oauth, &meta.label, &token).await;
        }
    }
    accounts_redirect()
}

#[derive(Deserialize)]
struct PasteGrok {
    #[serde(default)]
    label: String,
    #[serde(default)]
    json: String,
}

async fn paste_grok(State(state): State<ConsoleState>, Form(form): Form<PasteGrok>) -> Redirect {
    let toks = panday_sdk::oauth::grok_cli_all_from_json(&form.json);
    for (i, tok) in toks.into_iter().enumerate() {
        let label = if form.label.trim().is_empty() {
            format!("pasted-{}", i + 1)
        } else if i == 0 {
            form.label.trim().to_string()
        } else {
            format!("{}-{}", form.label.trim(), i + 1)
        };
        if let Ok(meta) = state.hub.add_secret("xai", "oauth", &label, &tok.access) {
            let _ = persist_secret("xai", Kind::Oauth, &meta.label, &tok.access).await;
        }
    }
    accounts_redirect()
}

#[derive(Deserialize)]
struct AddApi {
    provider: String,
    #[serde(default)]
    label: String,
    key: String,
}

async fn add_api(State(state): State<ConsoleState>, Form(form): Form<AddApi>) -> Redirect {
    let provider = form.provider.trim();
    if let Ok(meta) = state
        .hub
        .add_secret(provider, "api_key", form.label.trim(), form.key.trim())
    {
        let _ = persist_secret(provider, Kind::ApiKey, &meta.label, form.key.trim()).await;
    }
    accounts_redirect()
}

#[derive(Deserialize)]
struct RevokeForm {
    id: String,
}

async fn revoke_account(
    State(state): State<ConsoleState>,
    Form(form): Form<RevokeForm>,
) -> Redirect {
    let meta = state.hub.accounts().into_iter().find(|m| m.id == form.id);
    if let Some(m) = meta {
        persist_revoke(&m.provider, &m.last4).await;
        let _ = state.hub.remove(&form.id);
    }
    accounts_redirect()
}

#[derive(Deserialize)]
struct RotateForm {
    policy: String,
}

async fn set_rotate(State(state): State<ConsoleState>, Form(form): Form<RotateForm>) -> Redirect {
    if let Some(p) = Rotate::parse(&form.policy) {
        state.hub.set_rotate(p);
    }
    accounts_redirect()
}

#[derive(Deserialize)]
struct CeilingForm {
    id: String,
    /// Calls per window. Empty clears the grant, which is how an operator says
    /// "I no longer know this number" — distinct from declaring zero.
    ceiling: String,
    /// `5h`, `30d`, `90m`, or bare seconds. Blank defaults to 30 days.
    window: String,
}

async fn set_ceiling(State(state): State<ConsoleState>, Form(form): Form<CeilingForm>) -> Redirect {
    let grant = match form.ceiling.trim().parse::<u64>() {
        Ok(ceiling) => Some(Grant {
            ceiling,
            window: parse_window(&form.window)
                .unwrap_or(std::time::Duration::from_secs(30 * 24 * 3600)),
        }),
        // Unparseable or empty both clear it. Keeping a stale ceiling because a
        // form field had a typo would leave a remaining % nobody declared.
        Err(_) => None,
    };
    state.hub.set_grant(&form.id, grant);
    accounts_redirect()
}

async fn snapshot(state: &ConsoleState) -> Snapshot {
    let grok_toks = panday_sdk::oauth::grok_cli_all();
    let grok = grok_toks.into_iter().next();
    let claude = panday_sdk::oauth::claude_code();
    let catalog = panday_router::ModelCatalog::shipped();
    let providers: Vec<String> = state
        .gateway
        .providers()
        .into_iter()
        .map(str::to_string)
        .collect();
    let live = state.gateway.live_models().await;
    let live_ids: std::collections::HashSet<String> = live.iter().map(|m| m.id.clone()).collect();
    Snapshot {
        listen: state.listen.clone(),
        providers,
        grok: cred(grok.as_ref()),
        claude: cred(claude.as_ref()),
        accounts: {
            let usage = state.hub.usage();
            state
                .hub
                .accounts()
                .into_iter()
                .map(|m| {
                    let u = usage.iter().find(|u| u.id == m.id);
                    AccountRow {
                        id: m.id,
                        provider: m.provider,
                        kind: m.kind,
                        label: m.label,
                        last4: m.last4,
                        used: u.map(|u| u.used).unwrap_or(0),
                        ceiling: u.and_then(|u| u.ceiling),
                        window_secs: u.and_then(|u| u.window_secs),
                        remaining_pct: u.and_then(|u| u.remaining_pct),
                        exhausted: u.map(|u| u.exhausted).unwrap_or(false),
                    }
                })
                .collect()
        },
        rotate: state.hub.rotate().as_str().into(),
        models: overlay_live(&live, &catalog),
        pools: pools_for_live(pools_from_dev(), &live_ids),
        recent: state
            .usage
            .recent()
            .into_iter()
            .map(|r| CallRow {
                model: r.model.0,
                provider: r.provider,
                pool: r.pool,
                input_tokens: r.usage.input_tokens,
                output_tokens: r.usage.output_tokens,
                cache_read_tokens: r.usage.cache_read_tokens,
            })
            .collect(),
    }
}

/// Catalog supplies measured context and prices when we already know the id.
/// Presence on the page comes only from the provider listing.
fn overlay_live(live: &[crate::LiveModel], catalog: &panday_router::ModelCatalog) -> Vec<ModelRow> {
    live.iter()
        .map(|m| {
            if let Some(entry) = catalog.models.iter().find(|e| e.id == m.id) {
                ModelRow {
                    id: m.id.clone(),
                    context: entry.profile.context,
                    provenance: match entry.profile.provenance {
                        panday_types::capability::Provenance::Measured => "measured".into(),
                        panday_types::capability::Provenance::Declared => "declared".into(),
                    },
                }
            } else {
                ModelRow {
                    id: m.id.clone(),
                    context: m.context.unwrap_or(0),
                    provenance: "live".into(),
                }
            }
        })
        .collect()
}

fn pools_for_live(
    pools: Vec<PoolRow>,
    live_ids: &std::collections::HashSet<String>,
) -> Vec<PoolRow> {
    pools
        .into_iter()
        .filter_map(|mut p| {
            p.models.retain(|m| live_ids.contains(m));
            (!p.models.is_empty()).then_some(p)
        })
        .collect()
}

fn cred(tok: Option<&Token>) -> Cred {
    match tok {
        Some(t) => Cred {
            present: true,
            fresh: t.still_fresh(),
        },
        None => Cred {
            present: false,
            fresh: false,
        },
    }
}

fn pools_from_dev() -> Vec<PoolRow> {
    let raw = include_str!("../../panday-router/policy/dev.yaml");
    let v: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(raw).unwrap_or(serde_yaml_ng::Value::Null);
    let Some(map) = v.get("pools").and_then(|p| p.as_mapping()) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(k, val)| {
            let name = k.as_str()?.to_string();
            let models = val
                .as_sequence()?
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
            Some(PoolRow { name, models })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LiveModel;

    fn live(id: &str) -> LiveModel {
        LiveModel {
            id: id.into(),
            context: None,
            display_name: None,
        }
    }

    #[test]
    fn the_page_shows_what_the_provider_listed_not_the_yaml() {
        let catalog = panday_router::ModelCatalog::shipped();
        let listed = vec![
            live("anthropic/claude-fable-5"),
            live("anthropic/claude-opus-5"),
            live("xai/grok-4.6"),
            live("anthropic/claude-some-new-thing"),
        ];
        let rows = overlay_live(&listed, &catalog);
        let ids: Vec<&str> = rows.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "anthropic/claude-fable-5",
                "anthropic/claude-opus-5",
                "xai/grok-4.6",
                "anthropic/claude-some-new-thing"
            ]
        );
        assert_eq!(rows[3].provenance, "live");
        assert_eq!(rows[2].provenance, "measured");
        assert!(
            !ids.contains(&"together/qwen3.5-32b-instruct"),
            "catalog rows the provider did not list stay off the page"
        );
    }

    #[test]
    fn pools_only_name_models_the_provider_listed() {
        let live_ids = ["anthropic/claude-sonnet-5".to_string()]
            .into_iter()
            .collect();
        let pools = pools_for_live(
            vec![
                PoolRow {
                    name: "workhorse".into(),
                    models: vec![
                        "anthropic/claude-sonnet-5".into(),
                        "together/qwen3.5-32b-instruct".into(),
                    ],
                },
                PoolRow {
                    name: "local-only".into(),
                    models: vec!["local/qwen3.5-4b".into()],
                },
            ],
            &live_ids,
        );
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].models, ["anthropic/claude-sonnet-5"]);
    }
}
