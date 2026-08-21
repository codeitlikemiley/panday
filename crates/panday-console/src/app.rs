use crate::snapshot::{CallRow, Cred, ModelRow, PoolRow, Snapshot};
use leptos::prelude::*;

#[component]
pub fn App(path: String, snapshot: Snapshot) -> impl IntoView {
    view! {
        <div class="shell">
            <header class="top">
                <a class="mark" href="/">panday</a>
                <nav>
                    <a href="/" class=active(&path, "/")>console</a>
                    <a href="/models" class=active(&path, "/models")>models</a>
                    <a href="/playground" class=active(&path, "/playground")>playground</a>
                    <a href="/metrics">metrics</a>
                </nav>
                <span class="listen">{snapshot.listen.clone()}</span>
            </header>
            <main>
                {match path.as_str() {
                    "/models" => view! { <ModelsPage snapshot=snapshot /> }.into_any(),
                    "/playground" => view! { <PlaygroundPage snapshot=snapshot /> }.into_any(),
                    _ => view! { <OverviewPage snapshot=snapshot /> }.into_any(),
                }}
            </main>
        </div>
    }
}

fn active(path: &str, href: &str) -> &'static str {
    if path == href {
        "on"
    } else {
        ""
    }
}

#[component]
fn OverviewPage(snapshot: Snapshot) -> impl IntoView {
    view! {
        <section class="hero">
            <h1>Gateway</h1>
            <p>"OpenAI-compatible ingress. Subscription OAuth is read from the official CLIs, then the call goes to the provider. Not a third-party proxy."</p>
        </section>
        <div class="grid">
            <CredCard name="Grok CLI" id="xai/grok-4.6" cred=snapshot.grok />
            <CredCard name="Claude Code" id="anthropic/*" cred=snapshot.claude />
        </div>
        <h2>Adapters up</h2>
        <ul class="chips">
            {snapshot.providers.into_iter().map(|p| view! { <li>{p}</li> }).collect_view()}
        </ul>
        <h2>Recent calls</h2>
        <RecentTable rows=snapshot.recent />
    }
}

#[component]
fn CredCard(name: &'static str, id: &'static str, cred: Cred) -> impl IntoView {
    let state = if cred.present && cred.fresh {
        "live"
    } else if cred.present {
        "stale"
    } else {
        "off"
    };
    let label = if cred.present && cred.fresh {
        "signed in"
    } else if cred.present {
        "expired"
    } else {
        "missing"
    };
    view! {
        <article class="card">
            <p class="kicker">{id}</p>
            <h3>{name}</h3>
            <p class=format!("lamp {state}")>{label}</p>
        </article>
    }
}

#[component]
fn RecentTable(rows: Vec<CallRow>) -> impl IntoView {
    if rows.is_empty() {
        return view! { <p class="empty">"No calls this process. Send one from the playground."</p> }
            .into_any();
    }
    view! {
        <table>
            <thead>
                <tr>
                    <th>model</th>
                    <th>via</th>
                    <th>pool</th>
                    <th>in</th>
                    <th>out</th>
                    <th>cache</th>
                </tr>
            </thead>
            <tbody>
                {rows
                    .into_iter()
                    .rev()
                    .take(24)
                    .map(|r| {
                        view! {
                            <tr>
                                <td class="mono">{r.model}</td>
                                <td>{r.provider}</td>
                                <td>{r.pool}</td>
                                <td class="num">{r.input_tokens}</td>
                                <td class="num">{r.output_tokens}</td>
                                <td class="num">{r.cache_read_tokens}</td>
                            </tr>
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    }
    .into_any()
}

#[component]
fn ModelsPage(snapshot: Snapshot) -> impl IntoView {
    view! {
        <h1>Models this process can call</h1>
        <p>"Intersection of the catalog and the adapters that booted. Together and local GGUFs stay in the catalog for deployments that have them — they do not show up here unless that backend is up."</p>
        <h2>Pools</h2>
        <div class="grid">
            {snapshot
                .pools
                .into_iter()
                .map(|p| view! { <PoolCard pool=p /> })
                .collect_view()}
        </div>
        <h2>Models</h2>
        <table>
            <thead>
                <tr><th>id</th><th>context</th><th>provenance</th></tr>
            </thead>
            <tbody>
                {snapshot
                    .models
                    .into_iter()
                    .map(|m| view! { <ModelRowView row=m /> })
                    .collect_view()}
            </tbody>
        </table>
    }
}

#[component]
fn PoolCard(pool: PoolRow) -> impl IntoView {
    view! {
        <article class="card">
            <h3>{pool.name.clone()}</h3>
            <ul class="stack">
                {pool.models.into_iter().map(|m| view! { <li class="mono">{m}</li> }).collect_view()}
            </ul>
        </article>
    }
}

#[component]
fn ModelRowView(row: ModelRow) -> impl IntoView {
    view! {
        <tr>
            <td class="mono">{row.id}</td>
            <td class="num">{row.context}</td>
            <td>{row.provenance}</td>
        </tr>
    }
}

#[component]
fn PlaygroundPage(snapshot: Snapshot) -> impl IntoView {
    let default_model = snapshot
        .models
        .iter()
        .find(|m| m.id.starts_with("xai/"))
        .or_else(|| snapshot.models.first())
        .map(|m| m.id.as_str())
        .unwrap_or("auto");
    view! {
        <h1>Playground</h1>
        <p>"Posts to this process " <code>"/v1/chat/completions"</code> ". Same path curl uses."</p>
        <Playground default_model=default_model.to_string() />
    }
}

/// The only hydrated component. Status pages stay HTML.
#[island]
fn Playground(default_model: String) -> impl IntoView {
    let model = RwSignal::new(default_model);
    let prompt = RwSignal::new("reply with the single word pong".to_string());
    let reply = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let err = RwSignal::new(String::new());

    let send = move |_| {
        busy.set(true);
        err.set(String::new());
        reply.set(String::new());
        let model = model.get();
        let prompt = prompt.get();
        leptos::task::spawn_local(async move {
            match call_chat(model, prompt).await {
                Ok(text) => reply.set(text),
                Err(e) => err.set(e),
            }
            busy.set(false);
        });
    };

    view! {
        <form class="play" on:submit=move |ev| {
            ev.prevent_default();
            send(());
        }>
            <label>
                "model"
                <input type="text" prop:value=move || model.get()
                    on:input=move |ev| model.set(event_target_value(&ev)) />
            </label>
            <label>
                "prompt"
                <textarea prop:value=move || prompt.get()
                    on:input=move |ev| prompt.set(event_target_value(&ev))></textarea>
            </label>
            <button type="submit" disabled=move || busy.get()>
                {move || if busy.get() { "running" } else { "send" }}
            </button>
        </form>
        <pre class="reply" class:error=move || !err.get().is_empty()>
            {move || {
                let e = err.get();
                if e.is_empty() { reply.get() } else { e }
            }}
        </pre>
        <noscript>
            <form class="play" method="post" action="/console/try">
                <label>"model" <input type="text" name="model" value="xai/grok-4.6" /></label>
                <label>"prompt" <textarea name="prompt">"reply with the single word pong"</textarea></label>
                <button type="submit">send without wasm</button>
            </form>
        </noscript>
    }
}

#[cfg(feature = "hydrate")]
async fn call_chat(model: String, prompt: String) -> Result<String, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Request, RequestInit, RequestMode, Response};

    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 256,
        "temperature": 0.0,
        "stream": false
    });
    let opts = RequestInit::new();
    opts.set_method("POST");
    opts.set_mode(RequestMode::SameOrigin);
    opts.set_body(&wasm_bindgen::JsValue::from_str(&body.to_string()));
    let headers = web_sys::Headers::new().map_err(|e| format!("{e:?}"))?;
    headers
        .set("content-type", "application/json")
        .map_err(|e| format!("{e:?}"))?;
    opts.set_headers(&headers);
    let req = Request::new_with_str_and_init("/v1/chat/completions", &opts)
        .map_err(|e| format!("{e:?}"))?;
    let window = web_sys::window().ok_or("no window")?;
    let resp = JsFuture::from(window.fetch_with_request(&req))
        .await
        .map_err(|e| format!("{e:?}"))?;
    let resp: Response = resp.dyn_into().map_err(|e| format!("{e:?}"))?;
    let text = JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("{e:?}"))?
        .as_string()
        .unwrap_or_default();
    if !resp.ok() {
        return Err(text);
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse: {e} / {text}"))?;
    Ok(v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("(empty)")
        .to_string())
}

#[cfg(not(feature = "hydrate"))]
async fn call_chat(_model: String, _prompt: String) -> Result<String, String> {
    Ok(String::new())
}
