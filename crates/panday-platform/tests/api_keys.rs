//! M17.3 — API keys end to end, and the ingress they secure (docs/17 §API keys for machines).

use panday_gateway::ingress::{Authenticator, RateLimiter};
use panday_platform::keys::{self, Environment, KeyAuthenticator, KeyError, Scope};
use panday_platform::pg;
use panday_sdk::PandayError;

async fn database() -> sqlx::PgPool {
    let url = pg::test_database_url()
        .expect("PANDAY_TEST_DATABASE_URL is unset — start deploy/integration-compose.yml first");
    let pool = pg::connect(&url).await.expect("connect");
    pg::migrate(&pool, &pg::migrations_dir())
        .await
        .expect("migrate");
    pool
}

#[test]
fn a_key_is_typed_by_its_prefix() {
    // Identifiable on sight — in a log, a support ticket, a screenshot — and a test key used against
    // production fails on shape rather than by spending real money.
    assert_eq!(Environment::of("pnd_live_abc"), Some(Environment::Live));
    assert_eq!(Environment::of("pnd_test_abc"), Some(Environment::Test));
    assert_eq!(Environment::of("sk-ant-api03-xyz"), None);
    assert_eq!(Environment::of(""), None);
}

#[test]
fn the_hash_is_not_the_key() {
    let key = "pnd_live_0123456789abcdef";
    let hashed = keys::hash(key);
    assert_ne!(hashed, key);
    assert_eq!(hashed.len(), 64);
    // Deterministic, because the auth path is a lookup by hash.
    assert_eq!(keys::hash(key), hashed);
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_key_is_issued_once_and_never_retrievable() {
    // A key you can retrieve is a key an attacker can retrieve. There is no "show it again" path,
    // and this test is what keeps it that way.
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();

    let issued = keys::issue(&pool, account, "ci", Environment::Live, &[Scope::Models])
        .await
        .expect("issue");
    assert!(issued.plaintext.starts_with("pnd_live_"));
    assert!(issued.plaintext.len() > 40, "256-bit-class secret");

    let listed = keys::list(&pool, account).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "ci");
    assert_eq!(listed[0].scopes, vec![Scope::Models]);
    // The listing carries no key material at all — not even a truncated form, which would make every
    // audit log a partial leak.
    let rendered = format!("{listed:?}");
    assert!(!rendered.contains(&issued.plaintext));
    assert!(!rendered.contains(&keys::hash(&issued.plaintext)));
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn a_key_authenticates_to_its_own_account_and_scopes() {
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let issued = keys::issue(
        &pool,
        account,
        "ci",
        Environment::Live,
        &[Scope::Models, Scope::Sessions],
    )
    .await
    .unwrap();

    let resolved = keys::authenticate(&pool, &issued.plaintext).await.unwrap();
    assert_eq!(resolved.account_id, account);
    assert!(resolved.allows(Scope::Models));
    assert!(resolved.allows(Scope::Sessions));
    assert!(!resolved.allows(Scope::Admin));
    assert!(resolved.require(Scope::Admin).is_err());

    // Last-used is evidence for a rotation review: "not used in 90 days" is the argument for
    // revoking, and "used an hour ago" is the argument against.
    let listed = keys::list(&pool, account).await.unwrap();
    assert!(listed[0].last_used_at.is_some());
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn an_unknown_or_malformed_key_is_refused() {
    let pool = database().await;
    assert!(matches!(
        keys::authenticate(&pool, "sk-ant-api03-not-ours").await,
        Err(KeyError::Malformed)
    ));
    assert!(matches!(
        keys::authenticate(&pool, "pnd_live_never_issued_0000").await,
        Err(KeyError::NotFound)
    ));
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn revocation_is_instant_and_leaves_a_record() {
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let issued = keys::issue(&pool, account, "ci", Environment::Live, &[Scope::Models])
        .await
        .unwrap();
    assert!(keys::authenticate(&pool, &issued.plaintext).await.is_ok());

    keys::revoke(&pool, account, issued.key.key_id)
        .await
        .unwrap();
    assert!(matches!(
        keys::authenticate(&pool, &issued.plaintext).await,
        Err(KeyError::Revoked)
    ));

    // The record survives: an audit that cannot show a key *was* revoked cannot show when.
    let listed = keys::list(&pool, account).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].revoked);

    // Revoking twice is not an error to hide, but it is not a success either: the second call found
    // nothing to do.
    assert!(matches!(
        keys::revoke(&pool, account, issued.key.key_id).await,
        Err(KeyError::NotFound)
    ));
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn one_account_cannot_revoke_another_accounts_key() {
    // Every query here is tenant-scoped (docs/20 T5), and this is what that buys.
    let pool = database().await;
    let mine = pg::create_account(&pool, "mine").await.unwrap();
    let theirs = pg::create_account(&pool, "theirs").await.unwrap();
    let issued = keys::issue(&pool, theirs, "theirs", Environment::Live, &[Scope::Models])
        .await
        .unwrap();

    assert!(matches!(
        keys::revoke(&pool, mine, issued.key.key_id).await,
        Err(KeyError::NotFound)
    ));
    // Still works, because nothing happened to it.
    assert!(keys::authenticate(&pool, &issued.plaintext).await.is_ok());
    // And it is not in the other account's listing.
    assert!(keys::list(&pool, mine).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "needs the integration lane"]
async fn the_ingress_authenticator_maps_a_key_to_a_caller() {
    let pool = database().await;
    let account = pg::create_account(&pool, "acme").await.unwrap();
    let issued = keys::issue(&pool, account, "ci", Environment::Live, &[Scope::Models])
        .await
        .unwrap();
    let auth = KeyAuthenticator::new(pool.clone());

    let caller = auth.authenticate(&issued.plaintext).await.expect("caller");
    assert_eq!(caller.account.0, account);
    assert!(caller.allows("models"));
    assert!(!caller.allows("admin"));
    // The key id identifies the key for rate limiting and logs, and is not the key.
    assert_ne!(caller.key_id, issued.plaintext);

    // Empty, unknown and revoked all refuse.
    assert!(auth.authenticate("").await.is_err());
    assert!(auth.authenticate("pnd_live_nope").await.is_err());
    keys::revoke(&pool, account, issued.key.key_id)
        .await
        .unwrap();
    assert!(auth.authenticate(&issued.plaintext).await.is_err());
}

#[test]
fn the_rate_limiter_counts_per_key_and_reports_a_retry_hint() {
    let limiter = RateLimiter::per_minute(3);
    for _ in 0..3 {
        assert!(limiter.check("key-a").is_ok());
    }
    match limiter.check("key-a") {
        Err(PandayError::RateLimited { retry_after_ms }) => {
            // The rest of the window, so a client that honours it stops hammering.
            assert!(retry_after_ms > 0 && retry_after_ms <= 60_000);
        }
        other => panic!("{other:?}"),
    }
    // Another key is unaffected: the limit is per key, which is what makes one noisy customer not
    // everyone's problem.
    assert!(limiter.check("key-b").is_ok());
}
