//! M16.1 — manifest, capability model, and the SKILL.md loader.
//!
//! docs/16's stated goal for skills is that "the existing ecosystem should port
//! with **zero edits**", so several tests here feed the loader things a skill
//! written for another runtime would contain and assert it copes.
//!
//! The manifest tests are mostly about consent: a `plugin.toml` is what a human
//! agrees to at install time, so anything ambiguous in it becomes a permission
//! the user did not knowingly grant.

use panday_plugins::skill::{discover, index, Skill, DEFAULT_BODY_TOKEN_BUDGET};
use panday_plugins::{Capabilities, FsCapability, ManifestError, PluginManifest, SkillError};
use std::path::{Path, PathBuf};

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[test]
fn the_example_manifest_parses_with_typed_capabilities() {
    let m = PluginManifest::load(&fixture("example-plugin/plugin.toml")).unwrap();
    assert_eq!(m.name, "example-plugin");
    assert_eq!(m.capabilities.fs, FsCapability::WorkspaceRo);
    assert_eq!(m.capabilities.net, ["api.github.com"]);
    assert_eq!(m.capabilities.secrets, ["GITHUB_TOKEN"]);
    assert!(!m.requested_fs_writable());
    assert!(m.requests_network());
}

#[test]
fn a_typo_in_the_fs_capability_is_rejected_not_downgraded() {
    // The seed used a free string, where `workspac-ro` parses fine and then
    // reads as "no access requested" — i.e. the manifest would look LESS
    // dangerous than it is at the consent prompt.
    let err = PluginManifest::parse(
        r#"
name = "p"
version = "1"
description = "d"
[capabilities]
fs = "workspac-ro"
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ManifestError::Toml(_)), "{err}");
}

#[test]
fn a_name_that_could_traverse_the_filesystem_is_refused() {
    // The name reaches an install directory and a registry path.
    // Single-quoted TOML strings, so a backslash stays a backslash: with basic
    // strings `"a\\b"` becomes a backspace escape and the test would assert
    // nothing about separators.
    for bad in ["../escape", "a/b", r"a\b", ""] {
        let err = PluginManifest::parse(&format!(
            "name = '{bad}'\nversion = '1'\ndescription = 'd'\n"
        ))
        .unwrap_err();
        assert!(matches!(err, ManifestError::BadName(_)), "{bad}: {err}");
    }
}

#[test]
fn a_misplaced_key_is_an_error_rather_than_a_silently_dropped_request() {
    // `hooks` written after `[capabilities]` becomes a key of that table. Left
    // permissive, the plugin would install with no hooks and no complaint —
    // a manifest that does not say what its author believes it says.
    let err = PluginManifest::parse(
        r#"
name = "p"
version = "1"
description = "d"
[capabilities]
fs = "none"
hooks = ["pre_tool"]
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ManifestError::Toml(_)), "{err}");
    assert!(
        err.to_string().contains("hooks"),
        "the error should name the key: {err}"
    );
}

#[test]
fn a_control_character_in_the_name_is_refused() {
    let err = PluginManifest::parse("name = \"a\\u0008b\"\nversion = '1'\ndescription = 'd'\n")
        .unwrap_err();
    assert!(matches!(err, ManifestError::BadName(_)), "{err}");
}

#[test]
fn a_wildcard_domain_is_refused_rather_than_expanded() {
    // `*.example.com` reads as a narrow grant and is in fact a grant to
    // anything anyone can register under that domain — not something a user can
    // meaningfully consent to.
    let err = PluginManifest::parse(
        r#"
name = "p"
version = "1"
description = "d"
[capabilities]
net = ["*.example.com"]
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ManifestError::BadDomain(_)), "{err}");
}

#[test]
fn a_url_in_the_net_capability_is_refused() {
    for bad in [
        "https://api.github.com",
        "api.github.com:443",
        "api.github.com/x",
    ] {
        let err = PluginManifest::parse(&format!(
            "name = \"p\"\nversion = \"1\"\ndescription = \"d\"\n[capabilities]\nnet = [\"{bad}\"]\n"
        ))
        .unwrap_err();
        assert!(matches!(err, ManifestError::BadDomain(_)), "{bad}: {err}");
    }
}

#[test]
fn a_secret_name_that_is_not_an_env_var_is_refused() {
    let err = PluginManifest::parse(
        r#"
name = "p"
version = "1"
description = "d"
[capabilities]
secrets = ["github token"]
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ManifestError::BadSecretName(_)), "{err}");
}

#[test]
fn a_manifest_without_a_description_is_refused() {
    // The description is what a user consents against; an empty one makes the
    // prompt meaningless.
    let err =
        PluginManifest::parse("name = \"p\"\nversion = \"1\"\ndescription = \"\"\n").unwrap_err();
    assert!(matches!(err, ManifestError::MissingDescription));
}

#[test]
fn the_consent_summary_names_every_grant_individually() {
    let m = PluginManifest::load(&fixture("example-plugin/plugin.toml")).unwrap();
    let text = m.consent_summary();

    // A user refusing a plugin needs specifics, not "requests network access".
    assert!(text.contains("api.github.com"), "{text}");
    assert!(text.contains("GITHUB_TOKEN"), "{text}");
    assert!(text.contains("READ the workspace"), "{text}");
    assert!(text.contains("pre_tool"), "{text}");
    assert!(
        text.contains("runs inside every turn"),
        "a hook's reach should be stated: {text}"
    );
}

#[test]
fn a_requested_network_capability_is_not_presented_as_a_grant() {
    // The consent prompt is the only place a person decides. It used to render
    // `network: api.github.com`, which reads as "this plugin may reach that host" — and nothing
    // granted it. No tier can: the egress proxy docs/14 describes is unbuilt, and since M14.8 a
    // `NetPolicy` naming a host is refused rather than approximated.
    //
    // The hosts stay in the text (a user refusing a plugin needs specifics), but the line has to
    // say the request is not honoured, or the prompt is asking consent for something that will
    // not happen.
    let m = PluginManifest::load(&fixture("example-plugin/plugin.toml")).unwrap();
    let text = m.consent_summary();
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("network:"))
        .expect("a network line");

    assert!(
        line.contains("api.github.com"),
        "the requested host must still be named: {line}"
    );
    assert!(
        line.contains("NONE"),
        "the line must say the request is not granted, not merely list it: {line}"
    );
}

#[test]
fn a_manifest_requesting_nothing_says_so_explicitly() {
    // Silence in a consent prompt reads as "unknown", which is worse than
    // "none".
    let m = PluginManifest::parse("name = \"p\"\nversion = \"1\"\ndescription = \"d\"\n").unwrap();
    let text = m.consent_summary();
    assert!(text.contains("filesystem: no access"), "{text}");
    assert!(text.contains("network: none"), "{text}");
    assert!(text.contains("secrets: none"), "{text}");
    assert_eq!(
        Capabilities::default().fs,
        FsCapability::None,
        "the default must be the least privileged"
    );
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

#[test]
fn a_skill_loads_with_frontmatter_body_and_references() {
    let s = Skill::load(&fixture("example-plugin/skills/deploy-check/SKILL.md")).unwrap();

    assert_eq!(s.frontmatter.name, "deploy-check");
    assert!(s
        .frontmatter
        .description
        .starts_with("Verify a deploy is safe"));
    assert_eq!(s.frontmatter.triggers, ["deploy", "release"]);
    assert!(s.body.starts_with("# Deploy check"));
    assert_eq!(s.references.len(), 1, "references/ should be discovered");
    assert!(s.references[0].ends_with("checklist.md"));
}

#[test]
fn unknown_frontmatter_keys_are_kept_not_rejected() {
    // "Port with zero edits": a skill written for another runtime carries keys
    // we have never heard of, and refusing it would break the compatibility
    // this whole format exists for.
    let s = Skill::load(&fixture("example-plugin/skills/deploy-check/SKILL.md")).unwrap();
    assert!(s.frontmatter.extra.contains_key("license"));
    assert!(s.frontmatter.extra.contains_key("author"));
}

#[test]
fn the_index_entry_is_one_line_because_it_lives_in_the_cached_prefix() {
    let s = Skill::load(&fixture("example-plugin/skills/deploy-check/SKILL.md")).unwrap();
    let entry = s.index_entry();
    assert!(
        !entry.contains('\n'),
        "index entries must be one line: {entry}"
    );
    assert!(entry.starts_with("deploy-check:"));
    // The body must NOT be in the index — that is the entire point of lazy
    // loading (docs/16: index in stable, body on trigger).
    assert!(!entry.contains("backwards compatible"), "{entry}");
}

#[test]
fn a_missing_name_or_description_is_a_clear_error() {
    let path = Path::new("x/SKILL.md");

    let err = Skill::parse(path, "---\ndescription: d\n---\nbody").unwrap_err();
    assert!(
        matches!(
            err,
            SkillError::BadYaml { .. } | SkillError::MissingName { .. }
        ),
        "{err}"
    );

    let err = Skill::parse(path, "---\nname: n\ndescription: \"\"\n---\nbody").unwrap_err();
    assert!(
        matches!(err, SkillError::MissingDescription { .. }),
        "{err}"
    );
}

#[test]
fn a_file_without_frontmatter_is_refused_with_a_reason() {
    let err = Skill::parse(Path::new("x/SKILL.md"), "# Just markdown\n").unwrap_err();
    assert!(matches!(err, SkillError::NoFrontmatter { .. }), "{err}");
    assert!(err.to_string().contains("must open with"), "{err}");
}

#[test]
fn unterminated_frontmatter_is_refused() {
    let err = Skill::parse(
        Path::new("x/SKILL.md"),
        "---\nname: n\ndescription: d\nbody",
    )
    .unwrap_err();
    assert!(
        matches!(err, SkillError::UnterminatedFrontmatter { .. }),
        "{err}"
    );
}

#[test]
fn a_horizontal_rule_in_the_body_does_not_end_the_frontmatter_early() {
    // `---` is also valid markdown. Matching it anywhere would truncate the
    // frontmatter of any skill whose body uses a rule.
    let s = Skill::parse(
        Path::new("x/SKILL.md"),
        "---\nname: n\ndescription: d\n---\nintro\n\n---\n\nmore body\n",
    )
    .unwrap();
    assert_eq!(s.frontmatter.name, "n");
    assert!(s.body.contains("more body"), "{}", s.body);
}

#[test]
fn a_leading_byte_order_mark_is_tolerated() {
    // Files exported from an editor carry one, and it is not a reason to reject
    // a skill.
    let s = Skill::parse(
        Path::new("x/SKILL.md"),
        "\u{feff}---\nname: n\ndescription: d\n---\nbody",
    )
    .unwrap();
    assert_eq!(s.frontmatter.name, "n");
}

#[test]
fn an_oversized_body_is_reported_not_silently_truncated() {
    // docs/16 spills the tail to artifacts with expand-on-demand; the decision
    // belongs to whoever owns an artifact store, so this only reports.
    let big = "x".repeat(20_000);
    let s = Skill::parse(
        Path::new("x/SKILL.md"),
        &format!("---\nname: n\ndescription: d\n---\n{big}"),
    )
    .unwrap();

    assert!(s.exceeds_budget(DEFAULT_BODY_TOKEN_BUDGET));
    assert_eq!(s.body.len(), 20_000, "the body must not be truncated here");
}

#[test]
fn discovery_finds_skills_and_skips_reference_material() {
    let skills = discover(&fixture("example-plugin/skills")).unwrap();
    assert_eq!(
        skills.len(),
        1,
        "found: {:?}",
        skills
            .iter()
            .map(|s| &s.frontmatter.name)
            .collect::<Vec<_>>()
    );
    assert_eq!(skills[0].frontmatter.name, "deploy-check");
}

#[test]
fn discovery_of_a_missing_directory_is_empty_not_an_error() {
    // A plugin with no skills is normal.
    assert!(discover(&fixture("example-plugin/nope"))
        .unwrap()
        .is_empty());
}

#[test]
fn the_index_lists_every_skill_and_nothing_else() {
    let skills = discover(&fixture("example-plugin/skills")).unwrap();
    let idx = index(&skills);
    assert!(idx.contains("deploy-check"));
    assert!(idx.contains("Verify a deploy is safe"));
    assert!(
        !idx.contains("backwards compatible"),
        "bodies must stay out: {idx}"
    );
}

#[test]
fn the_consent_prompt_echoes_the_authors_own_spelling() {
    // Showing `PreTool` where the manifest says `pre_tool` makes a user check
    // whether they are looking at the same thing.
    use panday_plugins::HookPoint;
    assert_eq!(HookPoint::PreTool.wire_name(), "pre_tool");
    assert_eq!(HookPoint::OnCompaction.to_string(), "on_compaction");

    let m = PluginManifest::load(&fixture("example-plugin/plugin.toml")).unwrap();
    let text = m.consent_summary();
    assert!(text.contains("pre_tool"), "{text}");
    assert!(
        !text.contains("PreTool"),
        "Rust variant names must not leak: {text}"
    );
}
