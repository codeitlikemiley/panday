//! M17.6 — `panday local` honours a licence, and expires it without breaking.
//!
//! The behaviour worth pinning down is the *degradation*, not the happy path: what a customer
//! experiences on the day their renewal is late decides whether the licensing model is tolerable.

use panday_local::{Local, LocalConfig};
use panday_plugins::entitlement::{issue, Entitlement, Status};
use panday_plugins::signature::SigningKeyPair;
use std::path::{Path, PathBuf};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("panday-entitle-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn licence(dir: &Path, expires_in_days: i64, grace_days: u32) -> (PathBuf, String) {
    let keys = SigningKeyPair::from_bytes(&[5u8; 32]);
    let now = time::OffsetDateTime::now_utc();
    let rfc3339 = time::format_description::well_known::Rfc3339;
    let entitlement = Entitlement {
        version: 1,
        subject: "acme".into(),
        plan: "enterprise".into(),
        seats: 25,
        issued_at: (now - time::Duration::days(90)).format(&rfc3339).unwrap(),
        expires_at: (now + time::Duration::days(expires_in_days))
            .format(&rfc3339)
            .unwrap(),
        grace_days,
        note: None,
    };
    let (document, signature) = issue(&keys, &entitlement).unwrap();
    let path = dir.join("licence.json");
    std::fs::write(&path, document).unwrap();
    std::fs::write(format!("{}.sig", path.display()), signature).unwrap();
    (path, keys.public_key_hex())
}

fn config(workspace: &Path) -> LocalConfig {
    let mut config = LocalConfig::new(workspace);
    config.log = workspace.join("session.jsonl");
    config
}

#[tokio::test]
async fn a_valid_licence_is_reported_and_grants() {
    let dir = scratch("valid");
    let (path, key) = licence(&dir, 60, 30);
    let mut config = config(&dir);
    config.entitlement = Some((path, key));

    let local = Local::boot(config).await.expect("boot");
    let (entitlement, status) = local.entitlement().expect("a licence");
    assert_eq!(entitlement.seats, 25);
    assert!(status.grants());
    assert_eq!(
        local.licence_line().unwrap(),
        "licence: enterprise (25 seats)"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn an_expired_licence_degrades_to_community_and_says_so() {
    // The behaviour the whole design turns on: past grace it stops granting, and the software keeps
    // working. Bricking a paying customer's laptop over a renewal e-mail is an outage you charged
    // for (ADR-011: the offline tier needs no account at all).
    let dir = scratch("expired");
    let (path, key) = licence(&dir, -60, 30);
    let mut config = config(&dir);
    config.entitlement = Some((path, key));

    let local = Local::boot(config)
        .await
        .expect("an expired licence must still boot");
    let (_, status) = local.entitlement().unwrap();
    assert!(matches!(status, Status::Expired { .. }));
    assert!(!status.grants());
    let line = local.licence_line().unwrap();
    assert!(line.contains("community"), "{line}");
    assert!(line.contains("expired"), "{line}");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_licence_inside_grace_still_grants_and_warns_every_run() {
    // Not a one-time notice: a warning that appears once, on the day it expires, is one nobody sees.
    let dir = scratch("grace");
    let (path, key) = licence(&dir, -5, 30);
    let mut config = config(&dir);
    config.entitlement = Some((path, key));

    let local = Local::boot(config).await.expect("boot");
    let (_, status) = local.entitlement().unwrap();
    assert!(matches!(status, Status::Grace { .. }));
    assert!(status.grants());
    assert!(local.licence_line().unwrap().contains("EXPIRED"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn an_edited_licence_is_refused_at_boot() {
    // A text file a customer can read is a text file they can edit. A tampered licence is an
    // operator error worth stopping on, unlike an expired one.
    let dir = scratch("tampered");
    let (path, key) = licence(&dir, 30, 30);
    let document = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, document.replace("\"seats\": 25", "\"seats\": 2500")).unwrap();

    let mut config = config(&dir);
    config.entitlement = Some((path, key));
    let err = Local::boot(config).await.map(|_| ()).unwrap_err();
    assert!(err.to_string().contains("entitlement"), "{err}");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_missing_licence_file_is_an_error_rather_than_a_silent_downgrade() {
    // A typo in a path quietly dropping a paying customer to the community tier is a support ticket
    // that takes a week to reach the truth.
    let dir = scratch("missing");
    let mut config = config(&dir);
    config.entitlement = Some((dir.join("nope.json"), "ab".repeat(32)));
    assert!(Local::boot(config).await.map(|_| ()).is_err());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn no_licence_is_the_normal_case_and_grants_the_whole_product() {
    // The offline tier needs no account and no licence. Anything here that made a licence
    // *required* would break ADR-011.
    let dir = scratch("none");
    let local = Local::boot(config(&dir)).await.expect("boot");
    assert!(local.entitlement().is_none());
    assert!(
        local.licence_line().is_none(),
        "nothing to say, so say nothing"
    );

    std::fs::remove_dir_all(&dir).ok();
}
