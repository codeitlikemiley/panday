//! M16.6 end to end: publish to a real registry over HTTP, then install from it (docs/16).
//!
//! The order under test is the security property: verify, then consent, then extract. Each test
//! below breaks one link and checks that nothing lands on disk.

use panday_cli::plugin_install::{install, InstallRequest};
use panday_cli::Output;
use panday_platform::registry::http::{router, RegistryState};
use panday_platform::registry::{MemoryRegistry, RegistryStore, Tier};
use panday_plugins::archive::pack;
use panday_plugins::signature::SigningKeyPair;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Default)]
struct Captured {
    text: String,
}

impl Output for Captured {
    fn text(&mut self, s: &str) {
        self.text.push_str(s);
    }
    fn line(&mut self, s: &str) {
        self.text.push_str(s);
        self.text.push('\n');
    }
}

fn manifest(name: &str, version: &str) -> String {
    format!(
        r#"
name = "{name}"
version = "{version}"
description = "A fast linter"

[capabilities]
fs = "workspace-ro"
net = ["api.github.com"]
secrets = ["GITHUB_TOKEN"]
"#
    )
}

fn archive(name: &str, version: &str) -> Vec<u8> {
    pack(&[
        ("plugin.toml", manifest(name, version).as_bytes()),
        ("tools/lint_fast.wasm", b"\0asm\x01\0\0\0"),
        (
            "skills/lint/SKILL.md",
            b"---\nname: lint\ndescription: x\n---\n#Lint\n",
        ),
    ])
    .expect("pack")
}

fn scratch(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("panday-install-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A registry serving one signed release, plus the publisher's key.
async fn serve() -> (String, SigningKeyPair, Arc<MemoryRegistry>) {
    let store = Arc::new(MemoryRegistry::new());
    let key = SigningKeyPair::from_bytes(&[9u8; 32]);
    let bytes = archive("linty", "0.1.0");
    let signature = key.sign_archive(&bytes);
    panday_platform::registry::publish(
        store.as_ref(),
        "linty",
        "0.1.0",
        bytes,
        &signature,
        &key.public_key_hex(),
    )
    .expect("publish");

    let app = router(RegistryState::new(store.clone() as Arc<dyn RegistryStore>));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{}", addr.port()), key, store)
}

#[tokio::test(flavor = "multi_thread")]
async fn install_shows_the_grants_and_writes_nothing_without_consent() {
    // docs/16's "install-time consent". A prompt shown after the files are on disk is a
    // notification, so the default path stops before writing.
    let (url, _key, _store) = serve().await;
    let root = scratch("consent");
    let request = InstallRequest::parse("linty@0.1.0", url, root.clone()).unwrap();

    let mut out = Captured::default();
    let installed = install(request, &mut out).await.expect("install ran");

    assert!(!installed.committed, "nothing should be written");
    assert!(!installed.dir.exists(), "{}", installed.dir.display());
    // Every grant named individually (M16.1's consent summary), in the words a human reads
    // rather than the manifest's tokens — `fs = "workspace-ro"` is a config value, "READ the
    // workspace" is a consent question.
    assert!(out.text.contains("READ the workspace"), "{}", out.text);
    assert!(out.text.contains("api.github.com"), "{}", out.text);
    assert!(out.text.contains("GITHUB_TOKEN"), "{}", out.text);
    // And the two things a user needs to judge it by.
    assert!(out.text.contains("signed by:"), "{}", out.text);
    assert!(out.text.contains("unlisted"), "{}", out.text);
    assert!(out.text.contains("nobody reviewed"), "{}", out.text);
    assert!(out.text.contains("--yes"), "{}", out.text);
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn with_consent_it_extracts_the_plugin_and_records_the_key() {
    let (url, key, _store) = serve().await;
    let root = scratch("yes");
    let mut request = InstallRequest::parse("linty@0.1.0", url, root.clone()).unwrap();
    request.yes = true;

    let installed = install(request, &mut Captured::default())
        .await
        .expect("install");
    assert!(installed.committed);
    assert_eq!(installed.files.len(), 3);
    assert!(installed.dir.join("plugin.toml").exists());
    assert!(installed.dir.join("tools/lint_fast.wasm").exists());
    // The publisher key lands next to the plugin, so `--trust` on the next version has
    // something to point at and a reviewer can see who signed what is on disk.
    assert_eq!(
        std::fs::read_to_string(installed.dir.join(".publisher-key")).unwrap(),
        key.public_key_hex()
    );
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn trusting_the_wrong_key_refuses_and_writes_nothing() {
    // The locked-down path: `--trust` means "this publisher or nobody".
    let (url, _key, _store) = serve().await;
    let root = scratch("trust");
    let impostor = SigningKeyPair::from_bytes(&[1u8; 32]);
    let mut request = InstallRequest::parse("linty@0.1.0", url, root.clone()).unwrap();
    request.trust = Some(impostor.public_key_hex());
    request.yes = true;

    let err = install(request, &mut Captured::default())
        .await
        .expect_err("a different key must be refused");
    assert!(err.contains("does not match the key you trusted"), "{err}");
    assert!(!root.join("linty").exists());
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn trusting_the_right_key_installs() {
    let (url, key, _store) = serve().await;
    let root = scratch("trust-ok");
    let mut request = InstallRequest::parse("linty@0.1.0", url, root.clone()).unwrap();
    request.trust = Some(key.public_key_hex());
    request.yes = true;

    let mut out = Captured::default();
    let installed = install(request, &mut out).await.expect("install");
    assert!(installed.committed);
    // No TOFU line: this install was verified against something.
    assert!(!out.text.contains("accepted on trust"), "{}", out.text);
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_install_says_it_is_trusting_on_first_use() {
    let (url, _key, _store) = serve().await;
    let root = scratch("tofu");
    let request = InstallRequest::parse("linty@0.1.0", url, root.clone()).unwrap();
    let mut out = Captured::default();
    install(request, &mut out).await.unwrap();
    // Silence here would let the user believe the signature was checked against something.
    assert!(out.text.contains("accepted on trust"), "{}", out.text);
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tampered_download_is_refused() {
    // The registry verifies on publish, and the client verifies on install: the bytes could be
    // changed in between by anything between here and there.
    let store = Arc::new(MemoryRegistry::new());
    let key = SigningKeyPair::from_bytes(&[9u8; 32]);
    let good = archive("linty", "0.1.0");
    let signature = key.sign_archive(&good);
    panday_platform::registry::publish(
        store.as_ref(),
        "linty",
        "0.1.0",
        good,
        &signature,
        &key.public_key_hex(),
    )
    .unwrap();

    // A proxy that serves a different archive with the real signature attached.
    let tampered = archive("linty", "0.1.0");
    let mut tampered = tampered.clone();
    let middle = tampered.len() / 2;
    tampered[middle] ^= 0xff;
    let public = key.public_key_hex();
    let app = axum::Router::new().route(
        "/v1/plugins/{name}/{version}",
        axum::routing::get(move || {
            let body = tampered.clone();
            let signature = signature.clone();
            let public = public.clone();
            async move {
                let headers = [
                    ("x-panday-signature", signature),
                    ("x-panday-public-key", public),
                    ("x-panday-tier", "unlisted".to_string()),
                ];
                axum::response::IntoResponse::into_response((headers, body))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let root = scratch("tampered");
    let mut request = InstallRequest::parse(
        "linty@0.1.0",
        format!("http://127.0.0.1:{}", addr.port()),
        root.clone(),
    )
    .unwrap();
    request.yes = true;
    let err = install(request, &mut Captured::default())
        .await
        .expect_err("a tampered archive must be refused");
    assert!(err.contains("does not verify"), "{err}");
    assert!(!root.join("linty").exists());
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsigned_archive_is_refused() {
    let app = axum::Router::new().route(
        "/v1/plugins/{name}/{version}",
        axum::routing::get(|| async {
            axum::response::IntoResponse::into_response(archive("linty", "0.1.0"))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let root = scratch("unsigned");
    let mut request = InstallRequest::parse(
        "linty@0.1.0",
        format!("http://127.0.0.1:{}", addr.port()),
        root.clone(),
    )
    .unwrap();
    request.yes = true;
    let err = install(request, &mut Captured::default())
        .await
        .expect_err("unsigned code must be refused");
    assert!(err.contains("unsigned"), "{err}");
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_release_is_a_clear_error() {
    let (url, _key, _store) = serve().await;
    let root = scratch("missing");
    let mut request = InstallRequest::parse("linty@9.9.9", url, root.clone()).unwrap();
    request.yes = true;
    let err = install(request, &mut Captured::default())
        .await
        .unwrap_err();
    assert!(
        err.contains("404") || err.contains("no such release"),
        "{err}"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn reinstalling_over_an_existing_version_is_refused() {
    // A published version is immutable, so the bytes on disk are the bytes that were consented
    // to; silently replacing them would make that guarantee local-only.
    let (url, _key, _store) = serve().await;
    let root = scratch("again");
    let mut request = InstallRequest::parse("linty@0.1.0", url.clone(), root.clone()).unwrap();
    request.yes = true;
    install(request, &mut Captured::default()).await.unwrap();

    let mut again = InstallRequest::parse("linty@0.1.0", url, root.clone()).unwrap();
    again.yes = true;
    let err = install(again, &mut Captured::default()).await.unwrap_err();
    assert!(err.contains("already exists"), "{err}");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_floating_version_is_refused_at_parse_time() {
    // `install linty` would mean "whatever is newest", which is a different plugin tomorrow.
    let err = InstallRequest::parse("linty", "http://x".into(), PathBuf::from("/tmp")).unwrap_err();
    assert!(err.contains("needs a version"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_registry_tier_is_reported_but_grants_nothing() {
    // A `verified` tier changes what the prompt says and nothing about what is enforced: the
    // sandbox tiers and the manifest are what constrain a plugin (docs/16).
    let (url, _key, store) = serve().await;
    store.promote("linty", "0.1.0", Tier::Verified).unwrap();

    let root = scratch("tier");
    let request = InstallRequest::parse("linty@0.1.0", url, root.clone()).unwrap();
    let mut out = Captured::default();
    let installed = install(request, &mut out).await.unwrap();
    assert_eq!(installed.tier, "verified");
    assert!(out.text.contains("verified"), "{}", out.text);
    // The grants are still every grant.
    assert!(out.text.contains("GITHUB_TOKEN"), "{}", out.text);
    assert!(!out.text.contains("nobody reviewed"), "{}", out.text);
    std::fs::remove_dir_all(&root).ok();
}
