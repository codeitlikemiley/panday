//! Mounts the Leptos operator console on the gateway process (docs/11).

use crate::gateway::{CollectUsage, Gateway};
use axum::extract::{Form, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use panday_console::snapshot::{CallRow, Cred, ModelRow, PoolRow, Snapshot};
use panday_sdk::oauth::Token;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct ConsoleState {
    pub gateway: Arc<Gateway>,
    pub usage: Arc<CollectUsage>,
    pub listen: String,
}

pub fn router(state: ConsoleState) -> Router {
    let mut app = Router::new()
        .route("/", get(page))
        .route("/models", get(page))
        .route("/playground", get(page))
        .route("/console/forge.css", get(css))
        .route("/console/try", post(try_prompt))
        .route("/console/snapshot", get(snapshot_json))
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
    Html(panday_console::render(uri.path(), snapshot(&state)))
}

async fn css() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../panday-console/style/forge.css"),
    )
        .into_response()
}

async fn snapshot_json(State(state): State<ConsoleState>) -> axum::Json<Snapshot> {
    axum::Json(snapshot(&state))
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

fn snapshot(state: &ConsoleState) -> Snapshot {
    let grok = panday_sdk::oauth::grok_cli();
    let claude = panday_sdk::oauth::claude_code();
    let catalog = panday_router::ModelCatalog::shipped();
    let providers: Vec<String> = state
        .gateway
        .providers()
        .into_iter()
        .map(str::to_string)
        .collect();
    Snapshot {
        listen: state.listen.clone(),
        providers: providers.clone(),
        grok: cred(grok.as_ref()),
        claude: cred(claude.as_ref()),
        models: callable_models(&catalog, &providers),
        pools: callable_pools(pools_from_dev(), &providers),
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

/// The `/models` page lists what this process can call, not the whole shipped catalog.
fn adapter_up(providers: &[String], id: &str) -> bool {
    id.split_once('/')
        .is_some_and(|(p, _)| providers.iter().any(|x| x == p))
}

fn callable_models(catalog: &panday_router::ModelCatalog, providers: &[String]) -> Vec<ModelRow> {
    catalog
        .models
        .iter()
        .filter(|m| adapter_up(providers, &m.id))
        .map(|m| ModelRow {
            id: m.id.clone(),
            context: m.profile.context,
            provenance: match m.profile.provenance {
                panday_types::capability::Provenance::Measured => "measured".into(),
                panday_types::capability::Provenance::Declared => "declared".into(),
            },
        })
        .collect()
}

fn callable_pools(pools: Vec<PoolRow>, providers: &[String]) -> Vec<PoolRow> {
    pools
        .into_iter()
        .filter_map(|mut p| {
            p.models.retain(|m| adapter_up(providers, m));
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

    #[test]
    fn the_models_page_hides_backends_this_process_cannot_call() {
        let catalog = panday_router::ModelCatalog::shipped();
        let rows = callable_models(&catalog, &["anthropic".into(), "xai".into()]);
        let ids: Vec<&str> = rows.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"anthropic/claude-fable-5"), "{ids:?}");
        assert!(ids.contains(&"anthropic/claude-opus-5"), "{ids:?}");
        assert!(ids.contains(&"anthropic/claude-sonnet-5"), "{ids:?}");
        assert!(ids.contains(&"xai/grok-4.6"), "{ids:?}");
        assert!(
            !ids.iter().any(|id| id.starts_with("together/")),
            "together is not registered: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id.starts_with("local/")),
            "local llama-server is not up: {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id.starts_with("openai/")),
            "no OpenAI key: {ids:?}"
        );
        assert!(
            !ids.contains(&"anthropic/claude-opus-4-1"),
            "retired ids must not appear: {ids:?}"
        );
    }

    #[test]
    fn empty_pools_are_dropped_not_shown_as_fiction() {
        let pools = callable_pools(
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
            &["anthropic".into()],
        );
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].name, "workhorse");
        assert_eq!(pools[0].models, ["anthropic/claude-sonnet-5"]);
    }
}
