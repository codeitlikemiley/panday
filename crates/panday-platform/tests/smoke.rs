//! The deployment smoke suite (docs/22 M22.2).
//!
//! > "smoke suite (create session, run turn, check ledger row)"
//!
//! Three assertions against a *running* deployment, in the order a customer meets them: a key
//! authenticates, a turn runs through the ingress, and the ledger has a row for it. That last one is
//! the point — a deploy where inference works and metering silently does not is the worst possible
//! green tick, because it looks fine until the invoice.
//!
//! It talks HTTP to a base URL and SQL to a database, so it runs against the dev stack, a staging
//! environment, or production, unchanged. `#[ignore]`d because it needs both; the CI deploy job
//! passes `PANDAY_SMOKE_URL` and runs it with `--run-ignored`.

use panday_platform::keys::{self, Environment, Scope};
use panday_platform::pg;

/// Where to point. Absent means "no deployment to smoke", which skips rather than fails: this suite
/// exists to test a deployment, and a machine with none has nothing to say about one.
fn base_url() -> Option<String> {
    std::env::var("PANDAY_SMOKE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
}

async fn database() -> Option<sqlx::PgPool> {
    let url = std::env::var("PANDAY_SMOKE_DATABASE_URL")
        .ok()
        .or_else(pg::test_database_url)?;
    pg::connect(&url).await.ok()
}

#[tokio::test]
#[ignore = "needs a running deployment: PANDAY_SMOKE_URL"]
async fn a_fresh_key_can_run_a_turn_and_the_ledger_records_it() {
    let Some(base) = base_url() else {
        eprintln!("PANDAY_SMOKE_URL unset — nothing to smoke");
        return;
    };
    let pool = database().await.expect("PANDAY_SMOKE_DATABASE_URL");
    pg::migrate_embedded(&pool)
        .await
        .expect("the deployment's schema");

    // 1. An account and a key, exactly as an operator would mint them.
    let account = pg::create_account(&pool, &format!("smoke-{}", uuid::Uuid::new_v4()))
        .await
        .expect("create account");
    let issued = keys::issue(&pool, account, "smoke", Environment::Live, &[Scope::Models])
        .await
        .expect("issue key");

    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", base.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": "auto",
        "messages": [{"role": "user", "content": "say hello"}],
    });

    // 2. The turn. A deployment with no provider configured answers 503, which is a *successful*
    //    smoke of everything up to the provider — auth, routing, the ingress envelope. Anything
    //    else means the deployment is broken in a way worth failing on.
    let response = client
        .post(&url)
        .header("authorization", format!("Bearer {}", issued.plaintext))
        .json(&body)
        .send()
        .await
        .expect("the deployment answered");
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    assert!(
        status == 200 || status == 503,
        "unexpected {status}: {text}"
    );

    // 3. Auth is real: the same request without a key must be refused, or the deployment is an
    //    open proxy to somebody else's paid inference.
    let unauthenticated = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .expect("the deployment answered");
    assert_eq!(unauthenticated.status().as_u16(), 401);

    // 4. The ledger. Only when the call actually reached a provider — a 503 bills nothing, and
    //    asserting a row for it would make the smoke suite demand a charge for work not done.
    if status == 200 {
        let entries: (i64,) = sqlx::query_as(
            "SELECT count(*)::bigint FROM ledger_entries WHERE account_id = $1 AND kind = 'usage.model'",
        )
        .bind(account)
        .fetch_one(&pool)
        .await
        .expect("ledger query");
        assert!(
            entries.0 > 0,
            "the turn ran and the ledger has no row for it — metering is broken, which looks \
             fine until the invoice"
        );
    }

    // Tidy: the key is revoked rather than deleted, exactly as a real one would be.
    keys::revoke(&pool, account, issued.key.key_id).await.ok();
}

#[tokio::test]
#[ignore = "needs a running deployment: PANDAY_SMOKE_URL"]
async fn the_deployment_exposes_metrics_without_a_key() {
    // A metrics endpoint that needs a key is one nobody scrapes (docs/21), and a deploy that lost
    // its metrics is a deploy nobody can watch.
    let Some(base) = base_url() else {
        return;
    };
    let text = reqwest::get(format!("{}/metrics", base.trim_end_matches('/')))
        .await
        .expect("metrics answered")
        .text()
        .await
        .unwrap_or_default();

    assert!(
        text.contains("# HELP"),
        "not Prometheus exposition: {text:.200}"
    );
    // A named series rather than merely "some output": an empty registry also contains no `# HELP`
    // by accident, and this asserts the gateway's own metrics are registered.
    assert!(
        text.contains("panday_"),
        "no panday metrics in the exposition"
    );
}

#[tokio::test]
#[ignore = "needs a running deployment: PANDAY_SMOKE_URL"]
async fn the_schema_is_the_one_this_build_expects() {
    // The failure this catches: a deploy that rolled the binary and not the migrations. Everything
    // else in this suite would still pass while the first query against a new column failed.
    let Some(pool) = database().await else {
        return;
    };
    let applied = pg::migrate_embedded(&pool).await.expect("migrate");
    assert_eq!(
        applied.len(),
        pg::EMBEDDED_MIGRATIONS.len(),
        "the deployment is missing migrations this build carries"
    );
}

#[tokio::test]
#[ignore = "needs a running deployment: PANDAY_SMOKE_URL"]
async fn the_status_endpoint_answers_without_a_key_and_tells_the_truth() {
    // docs/22 M22.3. An uptime checker cannot present a key, and a status page that answers 200
    // with "degraded" in the body is one every checker reports as up.
    let Some(base) = base_url() else {
        return;
    };
    let response = reqwest::get(format!("{}/status", base.trim_end_matches('/')))
        .await
        .expect("status answered");
    let status = response.status().as_u16();
    let body: serde_json::Value = response.json().await.expect("json");

    assert!(status == 200 || status == 503, "unexpected {status}");
    assert_eq!(status == 200, body["status"] == "ok");
    // Content-free: no account counts, no customer names. A status page is a public URL.
    let rendered = body.to_string();
    assert!(!rendered.contains("account_id"), "{rendered}");
    assert!(body["version"].is_string());
    assert!(body["schema"].is_boolean());
}
