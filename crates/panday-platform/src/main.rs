//! The `panday-platform` service (docs/17, docs/22 shape 2).
//!
//! The composition root. Every other binary in the tree is a *deployment shape* with a piece
//! missing on purpose: `panday-gateway` has no database (it cannot — `panday-platform` depends on
//! it, not the other way round), `panday local` has no accounts (ADR-011). This one has all of it,
//! and that is its whole job: connect the ledger to the gateway's usage sink, the key table to the
//! ingress, the route audit to the router.
//!
//! Configuration is environment variables, because that is what every platform in docs/22 shape 2
//! supplies. A missing `PANDAY_DATABASE_URL` is fatal rather than defaulted: a billing service that
//! silently starts without its ledger is worse than one that does not start.

use panday_gateway::adapters::anthropic::Anthropic;
use panday_gateway::adapters::openai_compat::OpenAiCompat;
use panday_gateway::ingress::{IngressState, RateLimiter};
use panday_gateway::{Gateway, ProviderAdapter};
use panday_platform::entitlements::Plan;
use panday_platform::keys::KeyAuthenticator;
use panday_platform::ledger::{LedgerBudget, LedgerSink, OnWriteFailure};
use panday_platform::pg;
use panday_platform::routes::PgRouteAudit;
use panday_router::{ModelCatalog, PolicyRouter};
use std::sync::Arc;

const POLICY: &str = include_str!("../../panday-router/policy/default.yaml");

const USAGE: &str = "\
panday-platform — the hosted control plane (docs/17)

  panday-platform serve                          serve the API (default)
  panday-platform migrate                        apply migrations and exit
  panday-platform account <name>                 create an account, print its id
  panday-platform issue-key <account-id> <name> [scopes]
                                                 mint an API key; the secret is printed ONCE
  panday-platform keys <account-id>              list an account's keys (never the secret)
  panday-platform revoke-key <account-id> <key-id>
  panday-platform prune-routes <days>            drop route audit rows older than <days>
  panday-platform entitle <subject> <plan> <seats> <days> --key-file <path>
                                                 sign an offline licence (docs/17 M17.6)
  panday-platform drift <provider> <report.csv> <from> <to>
                                                 reconcile the ledger against a usage report
  panday-platform billing apply                  apply pending webhooks (the nightly reconcile)
  panday-platform billing stuck                  webhooks that could not be applied
  panday-platform billing export <hours-ago>     send an hour of usage to the meter (dry run)
  panday-platform suspend <account-id> <reason>  kill switch: refuse this account's keys now
  panday-platform unsuspend <account-id>         reinstate it
  panday-platform watch                          velocity + anomaly report (the nightly look)

Scopes are comma-separated: models,sessions,admin (default: models,sessions).
PANDAY_DATABASE_URL is required by every subcommand.
";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();

    // Bootstrap lives in the same binary as the service on purpose: the first key on a fresh
    // deployment has to come from somewhere, and "somewhere" being a second tool nobody built is
    // how a service ships without a way to use it. M17.7's admin panel replaces the ergonomics,
    // not the need.
    let result = match argv.as_slice() {
        [] | ["serve"] => run().await,
        ["migrate"] => migrate().await,
        ["account", name] => account(name).await,
        ["issue-key", account, name] => issue_key(account, name, "models,sessions").await,
        ["issue-key", account, name, scopes] => issue_key(account, name, scopes).await,
        ["keys", account] => list_keys(account).await,
        ["revoke-key", account, key] => revoke_key(account, key).await,
        ["prune-routes", days] => prune_routes(days).await,
        ["entitle", subject, plan, seats, days, "--key-file", key_file] => {
            entitle(subject, plan, seats, days, key_file)
        }
        ["drift", provider, report, from, to] => drift(provider, report, from, to).await,
        ["billing", "apply"] => billing_apply().await,
        ["billing", "stuck"] => billing_stuck().await,
        ["billing", "export", hours_ago] => billing_export(hours_ago).await,
        ["suspend", account, reason] => suspend(account, reason).await,
        ["unsuspend", account] => unsuspend(account).await,
        ["watch"] => watch().await,
        ["-h" | "--help" | "help"] => {
            print!("{USAGE}");
            return;
        }
        other => {
            eprint!("panday-platform: unknown command {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("panday-platform: {e}");
        std::process::exit(1);
    }
}

/// The pool every subcommand needs, migrated. A CLI that ran against an un-migrated database would
/// fail with a missing-column error instead of doing its job.
async fn admin_pool() -> Result<sqlx::PgPool, String> {
    let url = env("PANDAY_DATABASE_URL").ok_or("PANDAY_DATABASE_URL is required")?;
    let pool = pg::connect(&url).await.map_err(|e| e.to_string())?;
    pg::migrate_embedded(&pool)
        .await
        .map_err(|e| format!("migrate: {e}"))?;
    Ok(pool)
}

/// Migrate and exit — what a deploy step or a `just dev` runs before anything serves.
async fn migrate() -> Result<(), String> {
    let pool = admin_pool().await?;
    // `admin_pool` already migrated; report what the schema is, because "it worked" with no output
    // is indistinguishable from "it did nothing" in a deploy log.
    let names = pg::migrate_embedded(&pool)
        .await
        .map_err(|e| e.to_string())?;
    println!("schema up to date ({} migrations)", names.len());
    Ok(())
}

async fn account(name: &str) -> Result<(), String> {
    let pool = admin_pool().await?;
    let id = pg::create_account(&pool, name)
        .await
        .map_err(|e| e.to_string())?;
    println!("{id}");
    Ok(())
}

async fn issue_key(account: &str, name: &str, scopes: &str) -> Result<(), String> {
    use panday_platform::keys::{self, Environment, Scope};

    let account = parse_uuid(account)?;
    let scopes: Vec<Scope> = scopes
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Scope::parse(s).ok_or_else(|| format!("unknown scope `{s}`")))
        .collect::<Result<_, _>>()?;

    let pool = admin_pool().await?;
    let issued = keys::issue(&pool, account, name, Environment::Live, &scopes)
        .await
        .map_err(|e| e.to_string())?;

    // Printed once, to stdout, with the warning on stderr so a script can capture the key alone.
    eprintln!("This is the only time this key is shown. Store it now.");
    println!("{}", issued.plaintext);
    Ok(())
}

async fn list_keys(account: &str) -> Result<(), String> {
    let pool = admin_pool().await?;
    for key in panday_platform::keys::list(&pool, parse_uuid(account)?)
        .await
        .map_err(|e| e.to_string())?
    {
        let state = if key.revoked { "revoked" } else { "active" };
        let used = key.last_used_at.as_deref().unwrap_or("never used");
        let scopes: Vec<&str> = key.scopes.iter().map(|s| s.as_str()).collect();
        println!(
            "{}  {:<20} {:<8} {:<24} {}",
            key.key_id,
            key.name,
            state,
            used,
            scopes.join(",")
        );
    }
    Ok(())
}

async fn revoke_key(account: &str, key: &str) -> Result<(), String> {
    let pool = admin_pool().await?;
    panday_platform::keys::revoke(&pool, parse_uuid(account)?, parse_uuid(key)?)
        .await
        .map_err(|e| e.to_string())?;
    println!("revoked");
    Ok(())
}

async fn prune_routes(days: &str) -> Result<(), String> {
    let days: i64 = days
        .parse()
        .map_err(|_| format!("`{days}` is not a number"))?;
    let pool = admin_pool().await?;
    let dropped = panday_platform::routes::prune(&pool, days)
        .await
        .map_err(|e| e.to_string())?;
    println!("{dropped} route decisions older than {days} days deleted");
    Ok(())
}

/// Sign an offline entitlement (M17.6).
///
/// Needs no database: a licence is a statement about a contract, and an air-gapped customer may
/// never have had an account at all (docs/18 M18.7). Making it a database operation would tie the
/// one artifact that has to work offline to the one component that cannot.
fn entitle(
    subject: &str,
    plan: &str,
    seats: &str,
    days: &str,
    key_file: &str,
) -> Result<(), String> {
    use panday_plugins::entitlement::{issue, Entitlement};

    let seats: u32 = seats
        .parse()
        .map_err(|_| format!("`{seats}` is not a seat count"))?;
    let days: i64 = days
        .parse()
        .map_err(|_| format!("`{days}` is not a number of days"))?;

    let seed = std::fs::read(key_file).map_err(|e| format!("read {key_file}: {e}"))?;
    let seed: [u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| format!("{key_file} must be exactly 32 bytes of ed25519 seed"))?;
    let keys = panday_plugins::signature::SigningKeyPair::from_bytes(&seed);

    let now = time::OffsetDateTime::now_utc();
    let rfc3339 = time::format_description::well_known::Rfc3339;
    let entitlement = Entitlement {
        version: 1,
        subject: subject.to_string(),
        plan: plan.to_string(),
        seats,
        issued_at: now.format(&rfc3339).map_err(|e| e.to_string())?,
        expires_at: (now + time::Duration::days(days))
            .format(&rfc3339)
            .map_err(|e| e.to_string())?,
        // docs/17: "~90-day expiry + grace". Thirty days of grace, because a renewal that has to
        // land on the day is one that will eventually not.
        grace_days: 30,
        note: None,
    };

    let (document, signature) = issue(&keys, &entitlement)?;
    let path = format!("{subject}.entitlement.json");
    std::fs::write(&path, &document).map_err(|e| format!("write {path}: {e}"))?;
    std::fs::write(format!("{path}.sig"), &signature)
        .map_err(|e| format!("write {path}.sig: {e}"))?;

    println!(
        "wrote {path} and {path}.sig\n  subject: {subject} · plan: {plan} · seats: {seats}\n  \
         expires: {} (+{} days grace)\n\nThe customer runs:\n  \
         PANDAY_ENTITLEMENT_KEY={} panday local --entitlement {path}",
        entitlement.expires_at,
        entitlement.grace_days,
        keys.public_key_hex(),
    );
    Ok(())
}

/// Reconcile a period against a provider's usage report (M21.4).
///
/// Exits non-zero when anything needs a person, so a cron job's own failure handling is the alarm
/// of last resort — a monitor whose only output is a log line is a monitor nobody reads.
async fn drift(provider: &str, report: &str, from: &str, to: &str) -> Result<(), String> {
    use panday_platform::drift;

    let rfc3339 = time::format_description::well_known::Rfc3339;
    let parse_at = |s: &str, what: &str| {
        time::OffsetDateTime::parse(s, &rfc3339)
            .map_err(|_| format!("{what} `{s}` is not an RFC 3339 timestamp"))
    };
    let from = parse_at(from, "from")?;
    let to = parse_at(to, "to")?;

    let csv = std::fs::read_to_string(report).map_err(|e| format!("read {report}: {e}"))?;
    let theirs = drift::parse_report(&csv).map_err(|e| e.to_string())?;

    let pool = admin_pool().await?;
    let ours = drift::recorded_cogs(&pool, from, to)
        .await
        .map_err(|e| e.to_string())?;

    // 1% and one cent: rounding differs on every call because we price from our table and they
    // price from theirs. Both are overridable by an operator who knows their own noise floor.
    let tolerance_bp = env("PANDAY_DRIFT_TOLERANCE_BP")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let floor = env("PANDAY_DRIFT_FLOOR_MICROS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    let report = drift::compare(&ours, &theirs, tolerance_bp, floor);
    print!("{}", report.to_text());
    drift::publish(provider, &report);

    let alarms = report.alarms().len();
    if alarms > 0 {
        return Err(format!(
            "{alarms} model(s) need a look — a difference means either our metering is wrong or \
             the invoice is, and which one is a person's job"
        ));
    }
    Ok(())
}

/// The nightly reconcile (M17.4): apply what arrived, and say what did not.
///
/// docs/17: "never trust webhook delivery; poll-reconcile nightly". This is the poll half.
async fn billing_apply() -> Result<(), String> {
    let pool = admin_pool().await?;
    let report = panday_platform::billing::apply_pending(&pool, 500)
        .await
        .map_err(|e| e.to_string())?;
    println!(
        "applied {} · failed {} · unknown customer {}",
        report.applied, report.failed, report.unknown_customer
    );
    if report.failed > 0 || report.unknown_customer > 0 {
        // Non-zero, because a webhook nobody could apply is a customer on the wrong plan.
        return Err("some events could not be applied — `billing stuck` lists them".into());
    }
    Ok(())
}

async fn billing_stuck() -> Result<(), String> {
    let pool = admin_pool().await?;
    let stuck = panday_platform::billing::stuck(&pool, 100)
        .await
        .map_err(|e| e.to_string())?;
    if stuck.is_empty() {
        println!("nothing stuck");
        return Ok(());
    }
    for (id, error, attempts) in &stuck {
        println!("{id}  attempts {attempts}  {error}");
    }
    Err(format!("{} event(s) stuck", stuck.len()))
}

/// Export one hour of usage (M17.5).
///
/// A recording sink for now: the Stripe transport is a trait, and nothing in this repo has an
/// account to send to. What this proves is our half — the aggregate, the cursor, and the
/// idempotency — which is the half that can be wrong in a way a customer would pay for.
async fn billing_export(hours_ago: &str) -> Result<(), String> {
    let hours: i64 = hours_ago
        .parse()
        .map_err(|_| format!("`{hours_ago}` is not a number of hours"))?;
    let pool = admin_pool().await?;
    let hour = time::OffsetDateTime::now_utc() - time::Duration::hours(hours);

    let sink = panday_platform::billing::RecordingMeter::new();
    let report = panday_platform::billing::export_hour(&pool, &sink, hour)
        .await
        .map_err(|e| e.to_string())?;
    println!(
        "hour {hours}h ago: {} event(s), {} token(s), {} already exported, {} failed",
        report.events_sent, report.tokens, report.already_exported, report.failed
    );
    for (customer, meter, value) in sink.sent() {
        println!("  {customer:<40} {meter} {value}");
    }
    if report.failed > 0 {
        return Err(format!("{} account(s) failed to export", report.failed));
    }
    Ok(())
}

/// The kill switch from a terminal (M20.4).
///
/// Available as a command as well as a page, because the moment you need it most is the moment
/// something else is on fire and a browser is the wrong tool.
async fn suspend(account: &str, reason: &str) -> Result<(), String> {
    let pool = admin_pool().await?;
    panday_platform::abuse::suspend(&pool, parse_uuid(account)?, "cli", reason)
        .await
        .map_err(|e| e.to_string())?;
    println!("suspended — this account's keys are refused from the next request");
    Ok(())
}

async fn unsuspend(account: &str) -> Result<(), String> {
    let pool = admin_pool().await?;
    panday_platform::abuse::unsuspend(&pool, parse_uuid(account)?, "cli")
        .await
        .map_err(|e| e.to_string())?;
    println!("reinstated");
    Ok(())
}

/// The velocity and anomaly report (M20.4).
///
/// Reports; it does not act. A heuristic wired to an irreversible action will eventually be wrong
/// about a real customer on their busiest day — so this prints what a person should look at, and
/// `suspend` is a separate, deliberate command.
async fn watch() -> Result<(), String> {
    use panday_platform::abuse::{accounts_created, keys_issued, spend_micros, Velocity};

    let pool = admin_pool().await?;
    let signups = Velocity {
        window_hours: 1,
        limit: 50,
    };
    let seen = accounts_created(&pool, signups.window_hours)
        .await
        .map_err(|e| e.to_string())?;
    let verdict = signups.judge(seen);
    println!(
        "signups (1h): {seen} — {}",
        if verdict.is_suspicious() {
            "OVER LIMIT"
        } else {
            "ok"
        }
    );

    // The accounts worth a look: the biggest spenders in the last day, with their key-minting rate
    // beside them. Two ordinary numbers whose *combination* is the signal.
    let accounts: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "-- tenant-scoping: cross-tenant — the abuse watch is a population view by construction.
         SELECT account_id, name FROM accounts WHERE suspended_at IS NULL ORDER BY created_at DESC LIMIT 200",
    )
    .fetch_all(&pool)
    .await
    .map_err(|e| e.to_string())?;

    let keys = Velocity {
        window_hours: 1,
        limit: 10,
    };
    let mut flagged = 0;
    for (account_id, name) in accounts {
        let spent = spend_micros(&pool, account_id, 24)
            .await
            .map_err(|e| e.to_string())?;
        let minted = keys_issued(&pool, account_id, keys.window_hours)
            .await
            .map_err(|e| e.to_string())?;
        let key_verdict = keys.judge(minted);
        if spent == 0 && !key_verdict.is_suspicious() {
            continue;
        }
        if key_verdict.is_suspicious() {
            flagged += 1;
        }
        println!(
            "  {:<24} spent ${:>8.2}/24h · {minted} key(s)/1h{}",
            name,
            spent as f64 / 1e6,
            if key_verdict.is_suspicious() {
                "  ← LOOK"
            } else {
                ""
            }
        );
    }

    if verdict.is_suspicious() || flagged > 0 {
        // Non-zero so a cron job surfaces it, and nothing is suspended automatically.
        return Err("something is worth a look — nothing was suspended automatically".into());
    }
    Ok(())
}

fn parse_uuid(s: &str) -> Result<uuid::Uuid, String> {
    s.parse().map_err(|_| format!("`{s}` is not a uuid"))
}

async fn run() -> Result<(), String> {
    // Content-free telemetry (docs/20 T5): a platform log with a prompt in it is a data-retention
    // problem nobody chose.
    panday_sdk::telemetry::init("panday-platform").map_err(|e| e.to_string())?;

    let database_url = env("PANDAY_DATABASE_URL")
        .ok_or("PANDAY_DATABASE_URL is required — the platform is the service that has a ledger")?;
    let addr = env("PANDAY_PLATFORM_ADDR").unwrap_or_else(|| "0.0.0.0:8080".to_string());

    let pool = pg::connect(&database_url)
        .await
        .map_err(|e| format!("database: {e}"))?;
    // Migrate-then-serve, with the advisory lock making a rolling deploy safe (docs/22 §release
    // engineering). Compiled-in migrations, because a container has no source tree.
    let applied = pg::migrate_embedded(&pool)
        .await
        .map_err(|e| format!("migrate: {e}"))?;
    tracing::info!(count = applied.len(), "schema up to date");

    // The catalog resolves the policy's pool patterns and supplies the prices the ledger bills at.
    // A pool pattern that resolves to nothing is a rule that routes nowhere, so this is checked
    // before anything is served rather than discovered per request.
    let catalog = ModelCatalog::shipped();
    let prices = Arc::new(catalog.price_table());
    let router = PolicyRouter::from_yaml(POLICY)
        .map_err(|e| format!("policy: {e}"))?
        .with_catalog(catalog);

    let mut builder = Gateway::builder(Arc::new(router))
        .costs(prices.clone())
        // Fail-closed: this surface is the API product, and docs/17 is explicit that an API key's
        // usage must be billed or refused — never served for free because a write failed.
        .usage_sink(Arc::new(LedgerSink::new(
            pool.clone(),
            prices.clone(),
            OnWriteFailure::FailClosed,
        )))
        .budget(Arc::new(LedgerBudget::new(pool.clone(), Plan::free())))
        .route_audit(Arc::new(PgRouteAudit::new(pool.clone())));

    for (provider, adapter) in adapters() {
        builder = builder.adapter(provider, adapter);
    }
    let gateway = Arc::new(builder.build());
    tracing::info!(providers = ?gateway.providers(), "model plane ready");

    // Every request carries a key; the account comes from the key, so the fallback account here is
    // only ever used by code paths that cannot reach the network.
    let mut state = IngressState::open(gateway, panday_types::id::AccountId::new())
        .with_auth(Arc::new(KeyAuthenticator::new(pool.clone())));
    if let Some(limit) = env("PANDAY_RATE_LIMIT_PER_MIN").and_then(|v| v.parse().ok()) {
        state = state.with_rate_limit(Arc::new(RateLimiter::per_minute(limit)));
    }

    // Two routers on one port: the model plane and the sync endpoint. A customer who has a key
    // should not need a second host to push the sessions that key already paid for (M18.6).
    let mut app = panday_gateway::ingress::router(state).merge(
        panday_platform::sync::http::router(panday_platform::sync::http::SyncState {
            pool: pool.clone(),
            prices: prices.clone(),
            auth: Arc::new(KeyAuthenticator::new(pool.clone())),
        }),
    );

    // The admin surface (M17.7). Always mounted, always behind an `admin`-scoped key: an admin page
    // that appears only when a flag is set is one that is off in the deployment where it is needed.
    app = app.merge(panday_platform::admin::router(
        panday_platform::admin::AdminState {
            pool: pool.clone(),
            auth: Arc::new(KeyAuthenticator::new(pool.clone())),
        },
    ));

    // Mounted only when a secret is configured. An unauthenticated webhook endpoint is an
    // open write into the billing inbox, and defaulting the secret to something would make that
    // the out-of-the-box state.
    match env("PANDAY_BILLING_WEBHOOK_SECRET") {
        Some(secret) => {
            app = app.merge(panday_platform::billing::http::router(
                panday_platform::billing::http::WebhookState {
                    pool: pool.clone(),
                    secret,
                },
            ));
            tracing::info!("billing webhook mounted");
        }
        None => tracing::info!("no PANDAY_BILLING_WEBHOOK_SECRET — billing webhook not mounted"),
    }
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    println!("panday-platform listening on {addr}");
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("serve: {e}"))
}

/// Adapters the environment actually configured.
///
/// The local tier is always registered: it needs no credentials, and a llama-server that is not
/// running fails at connect with a clear error rather than being invisible here.
fn adapters() -> Vec<(&'static str, Arc<dyn ProviderAdapter>)> {
    let mut out: Vec<(&'static str, Arc<dyn ProviderAdapter>)> = Vec::new();
    if let Some(key) = env("ANTHROPIC_API_KEY") {
        out.push(("anthropic", Arc::new(Anthropic::new(key))));
    }
    if let Some(base) = env("PANDAY_COMPAT_BASE_URL") {
        out.push((
            "together",
            Arc::new(OpenAiCompat::new(base, env("PANDAY_COMPAT_API_KEY"))),
        ));
    }
    let local = env("PANDAY_LOCAL_BASE_URL").unwrap_or_else(|| "http://127.0.0.1:8081".to_string());
    out.push(("local", Arc::new(OpenAiCompat::local(local))));
    out
}

/// An environment variable that is set *and* has content. An empty string is how a compose file
/// spells "unset", and treating it as configured is how a service starts with an empty API key.
fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
