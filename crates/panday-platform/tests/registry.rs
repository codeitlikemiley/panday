//! M16.6 — publish, fetch, verify (docs/16 §the package).
//!
//! The tests are mostly about what the registry *refuses*, because a package registry's value
//! is entirely in its refusals: anything it accepts, somebody installs.

use panday_platform::registry::{publish, MemoryRegistry, RegistryError, RegistryStore, Tier};
use panday_plugins::archive::pack;
use panday_plugins::signature::SigningKeyPair;

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
    ])
    .expect("pack")
}

fn signed(name: &str, version: &str) -> (Vec<u8>, String, String, SigningKeyPair) {
    let key = SigningKeyPair::from_bytes(&[7u8; 32]);
    let bytes = archive(name, version);
    let signature = key.sign_archive(&bytes);
    let public = key.public_key_hex();
    (bytes, signature, public, key)
}

#[test]
fn a_signed_archive_publishes_and_fetches_back_byte_identical() {
    let store = MemoryRegistry::new();
    let (bytes, signature, public, _key) = signed("linty", "0.1.0");

    let release =
        publish(&store, "linty", "0.1.0", bytes.clone(), &signature, &public).expect("publish");
    assert_eq!(release.size, bytes.len());
    assert_eq!(release.digest.len(), 64);

    let (fetched, fetched_bytes) = store.get("linty", "0.1.0").expect("fetch");
    assert_eq!(fetched_bytes, bytes, "the bytes must not change in transit");
    assert_eq!(fetched.public_key, public);
    assert_eq!(fetched.signature, signature);
}

#[test]
fn publishing_always_lands_unlisted() {
    // docs/16: `verified` "requires review". A field in a request body is not a review, so
    // `publish` takes no tier at all — there is no code path from a publisher's wish to a tier.
    let store = MemoryRegistry::new();
    let (bytes, signature, public, _key) = signed("linty", "0.1.0");
    let release = publish(&store, "linty", "0.1.0", bytes, &signature, &public).unwrap();
    assert_eq!(release.tier, Tier::Unlisted);

    // Promotion exists, as a store method with no HTTP route: that route needs authentication
    // (M17.3), and a promotion endpoint without it would undo the trust model with one curl.
    store.promote("linty", "0.1.0", Tier::Verified).unwrap();
    assert_eq!(store.get("linty", "0.1.0").unwrap().0.tier, Tier::Verified);
}

#[test]
fn a_tampered_archive_does_not_publish() {
    let store = MemoryRegistry::new();
    let (bytes, signature, public, _key) = signed("linty", "0.1.0");
    let mut tampered = bytes.clone();
    // Flip a byte in the middle — the compressed stream still decompresses in many cases, and
    // the signature is what catches it.
    let middle = tampered.len() / 2;
    tampered[middle] ^= 0xff;

    let err = publish(&store, "linty", "0.1.0", tampered, &signature, &public)
        .expect_err("a tampered archive must not publish");
    assert!(matches!(err, RegistryError::BadSignature(_)), "{err:?}");
    assert!(store.get("linty", "0.1.0").is_none(), "nothing was stored");
}

#[test]
fn a_signature_from_a_different_key_does_not_publish() {
    let store = MemoryRegistry::new();
    let bytes = archive("linty", "0.1.0");
    let author = SigningKeyPair::from_bytes(&[1u8; 32]);
    let impostor = SigningKeyPair::from_bytes(&[2u8; 32]);
    let signature = author.sign_archive(&bytes);

    // Signed by one key, published as another's: the mismatch is the whole point of signing.
    let err = publish(
        &store,
        "linty",
        "0.1.0",
        bytes,
        &signature,
        &impostor.public_key_hex(),
    )
    .expect_err("must not publish");
    assert!(matches!(err, RegistryError::BadSignature(_)), "{err:?}");
}

#[test]
fn a_published_version_is_immutable() {
    // An installed plugin's bytes must not change under someone who already consented to them.
    // "Same version, different code" is the supply-chain attack signing exists to prevent, and
    // a registry that allowed a re-publish would provide it as a feature.
    let store = MemoryRegistry::new();
    let (bytes, signature, public, key) = signed("linty", "0.1.0");
    publish(&store, "linty", "0.1.0", bytes, &signature, &public).unwrap();

    let replacement = pack(&[
        ("plugin.toml", manifest("linty", "0.1.0").as_bytes()),
        ("tools/lint_fast.wasm", b"\0asm\x01\0\0\0different"),
    ])
    .unwrap();
    let replacement_signature = key.sign_archive(&replacement);
    let err = publish(
        &store,
        "linty",
        "0.1.0",
        replacement,
        &replacement_signature,
        &public,
    )
    .expect_err("re-publishing a version must be refused");
    assert!(
        matches!(err, RegistryError::AlreadyPublished(_, _)),
        "{err:?}"
    );

    // A new version is fine, and both are listed.
    let (bytes2, sig2, _pk, _k) = signed("linty", "0.2.0");
    publish(&store, "linty", "0.2.0", bytes2, &sig2, &public).unwrap();
    let versions: Vec<String> = store
        .versions("linty")
        .into_iter()
        .map(|r| r.version)
        .collect();
    assert_eq!(versions, ["0.1.0", "0.2.0"]);
}

#[test]
fn the_archive_must_agree_with_the_name_it_is_published_under() {
    // Otherwise `linty@1.0.0` could ship a manifest calling itself something else, and every
    // consent prompt after that describes a different plugin than the one installed.
    let store = MemoryRegistry::new();
    let key = SigningKeyPair::from_bytes(&[7u8; 32]);
    let bytes = archive("something-else", "9.9.9");
    let signature = key.sign_archive(&bytes);

    let err = publish(
        &store,
        "linty",
        "0.1.0",
        bytes,
        &signature,
        &key.public_key_hex(),
    )
    .expect_err("a mismatched manifest must be refused");
    assert!(err.to_string().contains("declares"), "{err}");
}

#[test]
fn an_archive_with_no_manifest_does_not_publish() {
    let store = MemoryRegistry::new();
    let key = SigningKeyPair::from_bytes(&[7u8; 32]);
    let bytes = pack(&[("tools/lint.wasm", b"\0asm")]).unwrap();
    let signature = key.sign_archive(&bytes);
    assert!(publish(
        &store,
        "linty",
        "0.1.0",
        bytes,
        &signature,
        &key.public_key_hex()
    )
    .is_err());
}

#[test]
fn fetching_something_that_was_never_published_is_none_not_a_panic() {
    let store = MemoryRegistry::new();
    assert!(store.get("ghost", "1.0.0").is_none());
    assert!(store.versions("ghost").is_empty());
    assert!(store.list().is_empty());
    assert!(matches!(
        store.promote("ghost", "1.0.0", Tier::Verified),
        Err(RegistryError::NotFound(_))
    ));
}
