//! Repo automation. docs/02 §Toolchain: "`cargo xtask` pattern for codegen
//! (event-schema export, OpenAPI generation) instead of build.rs cleverness."
//!
//! ```text
//! cargo xtask schemas          # regenerate proto/
//! cargo xtask schemas --check  # fail if proto/ is stale (CI)
//! cargo xtask reduce-bench     # reduce-then-solve scorecard + regression gate
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            usage();
            return ExitCode::FAILURE;
        }
    };

    match cmd {
        "schemas" => {
            let check = rest.iter().any(|a| a == "--check");
            match schemas(check) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("xtask schemas: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // M15.5. A task rather than only a test so the nightly job has one thing
        // to run and one artifact to keep, and so a human can read the scorecard
        // without decoding `cargo test` output.
        "reduce-bench" => reduce_bench(rest.iter().any(|a| a == "--quiet")),
        "help" | "--help" | "-h" => {
            usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown task: {other}\n");
            usage();
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!(
        "usage: cargo xtask <task>\n\n\
         tasks:\n  \
         schemas [--check]   export JSON Schema for the event protocol to proto/\n  \
         reduce-bench        run the reduce-then-solve eval; non-zero on regression\n"
    );
}

/// docs/15 M15.5: "nightly job + regression gate". The gate is the exit code.
fn reduce_bench(quiet: bool) -> ExitCode {
    // Delegating to the test binary rather than reimplementing the corpus here:
    // two copies of an eval corpus drift, and the copy the gate runs would be
    // the one nobody looks at.
    let status = std::process::Command::new(env!("CARGO"))
        .current_dir(repo_root())
        .args([
            "test",
            "-p",
            "panday-harness",
            "--test",
            "reduce_then_solve",
            "--",
            "--nocapture",
        ])
        .args(if quiet { vec!["--quiet"] } else { vec![] })
        .status();

    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(_) => {
            eprintln!(
                "\nreduce-bench FAILED — reduction made a recorded task unsolvable.\n\
                 docs/15: \"a reducer that loses the plot is negative value at any \
                 compression ratio\". This blocks release."
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("reduce-bench: cannot run cargo test: {e}");
            ExitCode::FAILURE
        }
    }
}

fn repo_root() -> PathBuf {
    // xtask/ lives one level below the workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live under the workspace root")
        .to_path_buf()
}

/// The schemas we publish, and where. Adding an entry here is all it takes to
/// export another type.
fn targets() -> Vec<(&'static str, schemars::Schema)> {
    vec![(
        "aep-envelope.schema.json",
        schemars::schema_for!(panday_types::Envelope),
    )]
}

fn render(schema: &schemars::Schema) -> Result<String, String> {
    let mut s = serde_json::to_string_pretty(schema).map_err(|e| e.to_string())?;
    s.push('\n');
    Ok(s)
}

fn schemas(check: bool) -> Result<ExitCode, String> {
    let dir = repo_root().join("proto");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create proto/: {e}"))?;

    let mut stale = Vec::new();
    for (name, schema) in targets() {
        let path = dir.join(name);
        let generated = render(&schema)?;

        if check {
            let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
            if on_disk != generated {
                stale.push(name);
            }
        } else {
            std::fs::write(&path, &generated).map_err(|e| format!("write {name}: {e}"))?;
            println!("wrote proto/{name}");
        }
    }

    if check && !stale.is_empty() {
        eprintln!(
            "proto/ is out of date: {stale:?}\n\n\
             The event protocol changed without regenerating its schema. Run:\n  \
             cargo xtask schemas\n\n\
             and include a version note per docs/03 §Versioning discipline — a\n\
             schema change without one is a breaking change nobody reviewed."
        );
        return Ok(ExitCode::FAILURE);
    }

    if check {
        println!("proto/ is up to date");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exported schema must describe every event kind this build knows.
    /// Without this, adding an `Event` variant that schemars silently skips
    /// would ship a schema that rejects valid traffic.
    #[test]
    fn schema_describes_every_known_event_tag() {
        let schema = serde_json::to_string(&schemars::schema_for!(panday_types::Envelope))
            .expect("schema serializes");

        let missing: Vec<&str> = panday_types::event::KNOWN_EVENT_TAGS
            .iter()
            .copied()
            .filter(|tag| !schema.contains(&format!("\"const\":\"{tag}\"")))
            .collect();

        assert!(
            missing.is_empty(),
            "these event kinds are missing from the exported schema: {missing:?}"
        );
    }

    /// Schema generation must be deterministic, or `--check` would flap in CI
    /// and every run would look like a protocol change.
    #[test]
    fn schema_generation_is_stable() {
        let a = render(&schemars::schema_for!(panday_types::Envelope)).unwrap();
        let b = render(&schemars::schema_for!(panday_types::Envelope)).unwrap();
        assert_eq!(a, b, "schema generation is not deterministic");
    }
}
