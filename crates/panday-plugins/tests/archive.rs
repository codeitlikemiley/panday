//! `.plugin` archive safety (M16.6, docs/16 §the package, docs/20 T3).
//!
//! An archive from a registry is attacker-controlled input, so most of these tests are
//! rejections. The one thing they all have in common: the rejection happens before anything is
//! written, and it names the entry — "the archive was refused" is not actionable.

use panday_plugins::archive::{
    extract_to, list, pack, read_manifest, ArchiveError, MAX_ENTRY_BYTES,
};
use std::path::PathBuf;

const MANIFEST: &str = r#"
name = "linty"
version = "0.1.0"
description = "A fast linter"

[capabilities]
fs = "workspace-ro"
"#;

fn dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("panday-archive-{tag}-{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Pack with a raw name, bypassing `tar`'s own validation.
///
/// The builder refuses to *write* an absolute or `..` path — which is a good default and
/// exactly why the hostile fixtures have to be built by hand. A real attacker is not using our
/// packer.
fn pack_raw(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut builder = tar::Builder::new(&mut gz);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            {
                let gnu = header.as_gnu_mut().expect("gnu header");
                gnu.name[..name.len()].copy_from_slice(name);
            }
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.finish().unwrap();
    }
    gz.finish().unwrap()
}

fn good_archive() -> Vec<u8> {
    pack(&[
        ("plugin.toml", MANIFEST.as_bytes()),
        ("tools/lint_fast.wasm", b"\0asm\x01\0\0\0"),
        (
            "skills/lint/SKILL.md",
            b"---\nname: lint\ndescription: x\n---\n# Lint\n",
        ),
    ])
    .expect("pack")
}

#[test]
fn the_manifest_is_readable_without_unpacking_anything() {
    // Consent comes before extraction (docs/16 §install-time consent), so the prompt has to be
    // buildable from an archive we have not committed to yet.
    let manifest = read_manifest(&good_archive()).expect("manifest");
    assert_eq!(manifest.name, "linty");
    assert_eq!(manifest.version, "0.1.0");
}

#[test]
fn an_archive_with_no_manifest_is_refused() {
    let bytes = pack(&[("tools/lint.wasm", b"\0asm")]).unwrap();
    assert!(matches!(
        read_manifest(&bytes),
        Err(ArchiveError::NoManifest)
    ));
}

#[test]
fn a_good_archive_extracts_exactly_what_it_contains() {
    let dest = dir("good");
    let written = extract_to(&good_archive(), &dest).expect("extract");
    assert_eq!(written.len(), 3);
    assert!(dest.join("plugin.toml").exists());
    assert!(dest.join("tools/lint_fast.wasm").exists());
    assert!(dest.join("skills/lint/SKILL.md").exists());
    // And nothing else.
    assert_eq!(
        std::fs::read_dir(&dest).unwrap().count(),
        3,
        "extra entries appeared"
    );
    std::fs::remove_dir_all(&dest).ok();
}

#[test]
fn a_traversal_path_is_refused_and_writes_nothing() {
    // The oldest archive bug there is.
    let dest = dir("traversal");
    let bytes = pack_raw(&[
        (b"plugin.toml", MANIFEST.as_bytes()),
        (b"../../../../tmp/panday-escaped", b"gotcha"),
    ]);

    let err = extract_to(&bytes, &dest).expect_err("traversal must be refused");
    match err {
        ArchiveError::UnsafeEntry { path, reason } => {
            assert!(path.contains(".."), "{path}");
            assert!(reason.contains("escapes"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!PathBuf::from("/tmp/panday-escaped").exists());
    std::fs::remove_dir_all(&dest).ok();
}

#[test]
fn an_absolute_path_is_refused() {
    let dest = dir("absolute");
    let bytes = pack_raw(&[
        (b"plugin.toml", MANIFEST.as_bytes()),
        (b"/etc/cron.d/panday", b"* * * * * root sh"),
    ]);
    assert!(matches!(
        extract_to(&bytes, &dest),
        Err(ArchiveError::UnsafeEntry { .. })
    ));
    std::fs::remove_dir_all(&dest).ok();
}

#[test]
fn a_symlink_entry_is_refused() {
    // The subtle escape: the *link* stays inside the destination while its target does not, so
    // a later write through it lands wherever the attacker chose. `tar`'s own unpack would
    // create it.
    let dest = dir("symlink");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut builder = tar::Builder::new(&mut gz);
        let mut header = tar::Header::new_gnu();
        header.set_path("plugin.toml").unwrap();
        header.set_size(MANIFEST.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, MANIFEST.as_bytes()).unwrap();

        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_path("keys").unwrap();
        link.set_link_name("/Users/someone/.ssh").unwrap();
        link.set_size(0);
        link.set_cksum();
        builder.append(&link, std::io::empty()).unwrap();
        builder.finish().unwrap();
    }
    let bytes = gz.finish().unwrap();

    let err = extract_to(&bytes, &dest).expect_err("a symlink must be refused");
    match err {
        ArchiveError::UnsafeEntry { path, reason } => {
            assert_eq!(path, "keys");
            assert!(reason.contains("Symlink"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!dest.join("keys").exists());
    std::fs::remove_dir_all(&dest).ok();
}

#[test]
fn an_oversized_entry_is_refused_by_its_header() {
    let dest = dir("bomb");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut builder = tar::Builder::new(&mut gz);
        let mut header = tar::Header::new_gnu();
        header.set_path("plugin.toml").unwrap();
        header.set_size(MANIFEST.len() as u64);
        header.set_cksum();
        builder.append(&header, MANIFEST.as_bytes()).unwrap();

        // A header claiming more than the cap. Refused without reading it — which is the point:
        // a 4GB claim should cost nothing to reject.
        let mut big = tar::Header::new_gnu();
        big.set_path("huge.bin").unwrap();
        big.set_size(MAX_ENTRY_BYTES + 1);
        big.set_cksum();
        builder.append(&big, std::io::empty()).unwrap_or(());
        let _ = builder.finish();
    }
    let bytes = gz.finish().unwrap();
    assert!(
        matches!(extract_to(&bytes, &dest), Err(ArchiveError::TooLarge(_))),
        "an oversized entry must be refused"
    );
    std::fs::remove_dir_all(&dest).ok();
}

#[test]
fn extraction_refuses_to_overwrite() {
    // An install that silently replaced a file is an install that can be used to replace a
    // file.
    let dest = dir("overwrite");
    std::fs::write(dest.join("plugin.toml"), "already here").unwrap();
    let err = extract_to(&good_archive(), &dest).expect_err("must refuse");
    assert!(err.to_string().contains("refusing to overwrite"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dest.join("plugin.toml")).unwrap(),
        "already here"
    );
    std::fs::remove_dir_all(&dest).ok();
}

#[test]
fn listing_an_archive_reads_paths_and_not_contents() {
    let names = list(&good_archive()).expect("list");
    assert_eq!(
        names,
        [
            "plugin.toml",
            "tools/lint_fast.wasm",
            "skills/lint/SKILL.md"
        ]
    );
}

#[test]
fn garbage_is_not_an_archive() {
    assert!(read_manifest(b"this is not gzip").is_err());
    let dest = dir("garbage");
    assert!(extract_to(b"this is not gzip", &dest).is_err());
    std::fs::remove_dir_all(&dest).ok();
}
