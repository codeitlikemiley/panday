//! No source file in this repository may carry a command that destroys a machine.
//!
//! **This lint exists because of a specific incident.** While authoring an agent benchmark, a test
//! fixture was written whose "broken" program was `rm -rf "$DIR/"*`, and its verifier ran that
//! program with `DIR` unset to prove the bug. With `DIR` empty the shell expanded it to
//! `rm -rf /*` — every top-level directory — and the suite executed it on the developer's machine.
//! Roughly eighteen applications were deleted before macOS's own protections stopped it.
//!
//! Three things failed at once, and only the first is lintable: a dangerous string existed in the
//! tree, something executed it, and nothing between the two objected. So this checks the first,
//! because it is the one that can be checked *at authoring time* — `cargo test` fails the moment
//! such a string appears, before anyone runs the suite that would execute it.
//!
//! The rule: a destructive command targeting the filesystem root, or targeting a *variable* (which
//! may be empty and therefore be the root), may not appear in any `.rs`, `.sh` or `.yml` file
//! unless that file declares [`DATA_ONLY_MARKER`] with a reason. The exemption is for files that
//! hold such strings as **data to be matched, never executed** — the prompt-injection canaries
//! (docs/20 M20.1) are the legitimate case, and their payloads only ever reach a filter's
//! `pre_tool` or a scripted model, never a shell.
//!
//! Unlike most lints here, this one is deliberately blunt. A false positive costs somebody a
//! comment explaining themselves; a false negative costs somebody their machine.

use std::path::{Path, PathBuf};

/// What a file must say to hold a destructive string, and why.
///
/// Per-file, greppable, and requires a reason — the same shape as the tenancy lint's exemption
/// (docs/20 T5), for the same reason: an exemption nobody can find is an exemption nobody reviews.
pub const DATA_ONLY_MARKER: &str = "dangerous-strings: data-only";

/// A destructive command found in a file.
#[derive(Debug, PartialEq, Eq)]
pub struct Finding {
    pub matched: String,
    pub why: &'static str,
}

/// Find commands that end a *machine*, as distinct from commands that delete a directory.
///
/// The distinction is the whole design. `rm -rf /tmp/panday-test-abc` is how every test in this
/// repository cleans up after itself; flagging it would mean twenty exemption markers within a week
/// and a lint nobody reads. What is forbidden is narrower and never legitimate in a fixture:
///
/// - a delete whose target is the root itself, or `/*`
/// - a delete whose target is a **variable**, which may be empty and therefore *be* the root — this
///   is the exact shape that caused the incident
/// - a recursive permission or ownership change from the root
/// - formatting a filesystem, overwriting a block device, or a fork bomb
pub fn destructive(text: &str) -> Vec<Finding> {
    let mut out = Vec::new();

    for verb in ["rm -rf ", "rm -fr ", "rm -Rf ", "rm -fR "] {
        for (at, _) in text.match_indices(verb) {
            let target = text[at + verb.len()..].trim_start();
            let matched = format!("{verb}{}", first_word(target));

            // A variable target: `$DIR`, `"$DIR"`, `${DIR}` — empty expands to nothing, and the
            // command becomes a delete of whatever preceded it, up to and including `/`.
            //
            // `${DIR:?}` is the exception, and it is the *fix* rather than an exemption: the shell
            // aborts with an error when the variable is unset **or empty**, so the empty case can
            // never reach `rm`. A lint that rejected the safe form too would push people toward
            // markers instead of toward correctness.
            let guarded = first_word(target).contains(":?");
            if !guarded
                && (target.starts_with('$')
                    || target.starts_with("\"$")
                    || target.starts_with("'$"))
            {
                out.push(Finding {
                    matched,
                    why: "the target is a variable, which may be empty and become `/`",
                });
                continue;
            }
            // Root itself, or a glob directly under it. `/tmp/x` and `/var/lib/apt/lists/*` are
            // ordinary and are not matched.
            let rooted = target
                .strip_prefix('"')
                .or_else(|| target.strip_prefix('\''))
                .unwrap_or(target);
            if let Some(rest) = rooted.strip_prefix('/') {
                if rest.is_empty()
                    || rest.starts_with('*')
                    || rest.starts_with('"')
                    || rest.starts_with(char::is_whitespace)
                {
                    out.push(Finding {
                        matched,
                        why: "the target is the filesystem root",
                    });
                }
            }
        }
    }

    for (pattern, why) in [
        (
            "chmod -R 777 /",
            "a world-writable permission change from the root",
        ),
        (
            "chmod -Rf 777 /",
            "a world-writable permission change from the root",
        ),
        ("chown -R root /", "an ownership change from the root"),
        ("mkfs", "a filesystem format"),
        ("dd if=/dev/zero of=/dev/", "overwriting a block device"),
        (":(){ :|:& };:", "a fork bomb"),
    ] {
        if text.contains(pattern) {
            // `chmod -R 777 /workspace` is not the root; require the same end-of-target check.
            if let Some(rest) = pattern.strip_suffix('/') {
                let _ = rest;
                let ok_at_root = text.match_indices(pattern).any(|(at, _)| {
                    let after = &text[at + pattern.len()..];
                    after.is_empty()
                        || after.starts_with('*')
                        || after.starts_with('"')
                        || after.starts_with(char::is_whitespace)
                });
                if !ok_at_root {
                    continue;
                }
            }
            out.push(Finding {
                matched: pattern.to_string(),
                why,
            });
        }
    }
    out
}

fn first_word(s: &str) -> String {
    s.chars()
        .take_while(|c| !c.is_whitespace())
        .take(40)
        .collect()
}

#[test]
fn no_source_file_carries_a_machine_ending_command() {
    let root = workspace_root();
    let mut files = Vec::new();
    for dir in ["crates", "xtask", "scripts", ".github", "deploy"] {
        collect(&root.join(dir), &mut files);
    }
    assert!(
        files.len() > 50,
        "the walker found almost nothing: {}",
        files.len()
    );

    let mut findings = Vec::new();
    for path in &files {
        // The lint's own source names every pattern it forbids; a scan that read itself could never
        // be armed.
        if path.ends_with("no_destructive_fixtures.rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let hits = destructive(&text);
        if hits.is_empty() || claims_data_only(&text).is_some() {
            continue;
        }
        for hit in hits {
            findings.push(format!(
                "{}: `{}` — {}",
                path.strip_prefix(&root).unwrap_or(path).display(),
                hit.matched,
                hit.why
            ));
        }
    }

    assert!(
        findings.is_empty(),
        "a destructive command is present in the tree:\n  {}\n\n\
         If the string is data that is matched and never executed, declare it at the top of the \
         file:\n  // {DATA_ONLY_MARKER} — <why this is never executed>\n\n\
         If something in the tree *runs* it, that is the bug this lint exists for. Fixtures must \
         target a path inside their own workspace, and a verifier that has to demonstrate an \
         unguarded variable should inspect the expansion (`echo`) rather than execute it.",
        findings.join("\n  ")
    );
}

#[test]
fn the_exemption_needs_a_reason() {
    // A bare marker is not an exemption: the reason is the whole point, and one that needs no
    // argument is one that spreads.
    assert!(
        claims_data_only("// dangerous-strings: data-only — matched by a filter, never run")
            .is_some()
    );
    assert!(claims_data_only("// dangerous-strings: data-only").is_none());
    assert!(claims_data_only("// dangerous-strings: data-only    ").is_none());
    assert!(claims_data_only("nothing here").is_none());
}

#[test]
fn the_lint_catches_the_fixture_that_caused_the_incident() {
    // The exact shape, verified as a string rather than by planting a file — this test must not
    // create anything that another process could execute.
    let fixture = "#!/bin/sh\nrm -rf \"$DIR/\"*\n";
    let hits = destructive(fixture);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].why.contains("may be empty"), "{:?}", hits[0]);

    // Root and root-glob, the other two shapes.
    assert_eq!(destructive("rm -rf /*").len(), 1);
    assert_eq!(destructive("rm -rf / ").len(), 1);

    // The guarded form is the fix, and the lint has to accept it — otherwise it teaches people to
    // add exemption markers rather than to write the safe thing.
    assert!(destructive("rm -rf \"${WORK:?}\"").is_empty());
    assert!(destructive("rm -rf \"${DIR:?not set}\"/*").is_empty());
    // Without the guard, the same line is flagged.
    assert_eq!(destructive("rm -rf \"$WORK\"").len(), 1);

    // And the forms that must NOT be flagged, or the lint would forbid every test that cleans up
    // after itself and would be exempted into uselessness within a week.
    for ordinary in [
        "rm -rf /tmp/panday-test-abc123",
        "rm -rf /var/lib/apt/lists/*",
        "std::fs::remove_dir_all(&dir)",
        "rm -rf target/airgap",
        "chmod -R 777 /workspace/scratch",
    ] {
        assert!(
            destructive(ordinary).is_empty(),
            "flagged ordinary cleanup: {ordinary} -> {:?}",
            destructive(ordinary)
        );
    }
}

/// Whether a file declares the exemption, with a reason.
fn claims_data_only(text: &str) -> Option<String> {
    for line in text.lines().take(40) {
        if let Some(at) = line.find(DATA_ONLY_MARKER) {
            let reason = line[at + DATA_ONLY_MARKER.len()..]
                .trim_start_matches([':', '—', '-', ' '])
                .trim();
            return (!reason.is_empty()).then(|| reason.to_string());
        }
    }
    None
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n == "target" || n == ".git")
            {
                continue;
            }
            collect(&path, out);
        } else if path
            .extension()
            .is_some_and(|e| e == "rs" || e == "sh" || e == "yml" || e == "yaml")
        {
            out.push(path);
        }
    }
}
