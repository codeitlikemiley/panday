//! M22.5's other half: the install has to *succeed* with no network.
//!
//! `xtask::airgap`'s unit tests are all string properties of two generated files — the installer
//! contains no `curl`, the README mentions every component, the env-var names match what the
//! binary reads. Every one of them is negative and checked at build time, and none of them runs
//! anything. A kit can satisfy all of them and still fail on the customer's box, which is the
//! failure this file exists to catch.
//!
//! `docker run --network none` is a real air gap for every property this kit claims: no interface
//! but loopback, no DNS, no route off the machine. `deploy/airgap-test.Dockerfile` builds the kit
//! on a connected stage and copies it into a bare `debian:bookworm-slim` that installs nothing —
//! the two machines M22.5 describes.
//!
//! `#[ignore]`d: docs/02's unit lane is "no network, no docker".
//!
//! ```text
//! docker build -f deploy/airgap-test.Dockerfile -t panday-airgap-test .
//! cargo test -p xtask --test airgap_container -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

const IMAGE: &str = "panday-airgap-test";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives under the workspace root")
        .to_path_buf()
}

/// Build the image if it is not already here. The build stage needs a network — a kit is *built*
/// on a connected machine; it is *installed* on one that is not.
fn image() {
    let present = Command::new("docker")
        .args(["image", "inspect", IMAGE])
        .output()
        .expect("docker must be on PATH for the integration lane");
    if present.status.success() {
        return;
    }
    let built = Command::new("docker")
        .current_dir(repo_root())
        .args([
            "build",
            "-f",
            "deploy/airgap-test.Dockerfile",
            "-t",
            IMAGE,
            ".",
        ])
        .status()
        .expect("docker build");
    assert!(built.success(), "could not build {IMAGE}");
}

/// Run a shell snippet on the air-gapped machine. `--network none` is the locked door: the
/// container gets a loopback interface and nothing else.
fn airgapped(script: &str) -> (bool, String) {
    image();
    let out = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            "none",
            IMAGE,
            "sh",
            "-eu",
            "-c",
            script,
        ])
        .output()
        .expect("docker run");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

#[test]
#[ignore = "needs docker (deploy/airgap-test.Dockerfile)"]
fn the_air_gap_is_real() {
    // If this passes, every other test in this file proves nothing: a container that can still
    // reach the network would install happily whether or not the kit needed one.
    let (ok, out) = airgapped("getent hosts example.com || echo NO_DNS");
    assert!(ok, "{out}");
    assert!(
        out.contains("NO_DNS"),
        "the container resolved a hostname, so `--network none` is not in effect: {out}"
    );
}

#[test]
#[ignore = "needs docker (deploy/airgap-test.Dockerfile)"]
fn the_installer_completes_with_no_network() {
    let (ok, out) = airgapped("./install.sh");
    assert!(ok, "install.sh failed on a machine with no network:\n{out}");
    assert!(
        out.contains("installed to"),
        "the installer did not report success:\n{out}"
    );
}

#[test]
#[ignore = "needs docker (deploy/airgap-test.Dockerfile)"]
fn the_installed_binaries_are_where_the_readme_says_and_they_run() {
    // The README promises `~/.local/bin` and `~/.panday`. A kit that installs somewhere else is a
    // kit whose only documentation is wrong.
    let (ok, out) = airgapped(
        "./install.sh >/dev/null \
         && test -x \"$HOME/.local/bin/panday\" \
         && test -f \"$HOME/.panday/local.yaml\" \
         && test -f \"$HOME/.panday/catalog.yaml\" \
         && \"$HOME/.local/bin/panday\" --version \
         && \"$HOME/.local/bin/panday-local\" --help >/dev/null \
         && echo RAN",
    );
    assert!(ok, "{out}");
    assert!(out.contains("RAN"), "{out}");
}

#[test]
#[ignore = "needs docker (deploy/airgap-test.Dockerfile)"]
fn the_readme_never_promises_more_than_the_box_can_do() {
    // The defect this whole file was written for. M22.5's bar is "installs following only its own
    // README", and the README's single check that the install worked is `panday-local --serve`,
    // which *spawns* an inference runner (`panday_local::supervisor`, default `llama-server`).
    // A kit that ships no runner cannot execute its own verification step, and behind a locked
    // door there is no way to fetch one.
    //
    // Asserted against the box rather than against `KIT_LAYOUT`, because the question is not what
    // the layout list says — it is whether the sentences in INSTALL.md are true on the machine
    // INSTALL.md is for. Either half may change; they may not disagree.
    let (ok, out) = airgapped(
        "./install.sh >/dev/null \
         && { command -v \"$HOME/.local/bin/llama-server\" >/dev/null 2>&1 \
              || command -v \"$HOME/.local/bin/mistralrs-server\" >/dev/null 2>&1; } \
              && echo RUNNER_PRESENT || echo RUNNER_ABSENT; \
         cat INSTALL.md",
    );
    assert!(ok, "{out}");

    let claims_completeness = out.contains("Everything needed to run Panday");
    if out.contains("RUNNER_PRESENT") {
        assert!(
            claims_completeness,
            "the kit packs a runner but the README still hedges — the reader is told to bring \
             something that is already in the box:\n{out}"
        );
    } else {
        assert!(
            !claims_completeness,
            "the kit ships no inference runner, so `panday-local --serve` cannot run, yet the \
             README opens by claiming the box holds everything needed. Either pack one with \
             `xtask airgap --runner <path>` or keep the claim honest.\n{out}"
        );
        assert!(
            out.contains("does not include an inference runner")
                || out.contains("ships no inference runner"),
            "the absence has to be stated, not left for the reader to discover at the prompt:\n{out}"
        );
        // Bad news without a way out is still a dead end behind a locked door.
        assert!(
            out.contains("--base-url"),
            "the README names no runner-less path, so a reader who brought no runner is stuck:\n{out}"
        );
    }
}

#[test]
#[ignore = "needs docker (deploy/airgap-test.Dockerfile)"]
fn a_missing_runner_is_named_rather_than_crashed_into() {
    // If the reader ignores the README and runs `--serve` anyway, the failure has to name what is
    // missing. Nobody behind a locked door can attach a debugger, and "os error 2" on its own is
    // indistinguishable from a corrupt install.
    let (ok, out) = airgapped(
        // `|| status=$?` because the harness runs these under `sh -eu`, and a bare failing
        // command would abort the shell before the two lines that report what happened.
        "./install.sh >/dev/null; touch /tmp/m.gguf; status=0; \
         \"$HOME/.local/bin/panday-local\" --serve /tmp/m.gguf --workspace /tmp hello \
           >/dev/null 2>/tmp/e || status=$?; echo \"EXIT=$status\"; head -1 /tmp/e",
    );
    assert!(ok, "{out}");
    assert!(
        out.contains("EXIT=1"),
        "a kit that cannot serve must exit non-zero, or a script around it reports success:\n{out}"
    );
    assert!(
        out.contains("llama-server"),
        "the error must name the binary that is missing:\n{out}"
    );
}

#[test]
#[ignore = "needs docker (deploy/airgap-test.Dockerfile)"]
fn a_packed_model_reaches_the_model_directory_and_the_readme() {
    // `models/` had never been exercised by any test. M18.7 refuses a kit whose `--models`
    // directory holds no GGUF precisely so nobody discovers an empty box at the first prompt —
    // but nothing checked the other end, that a packed model actually arrives where the README
    // says it will.
    //
    // The kit is built with a 4KB stub (see the Dockerfile). That is enough to prove the path;
    // it is not enough to prove inference, which needs the runner this kit does not ship.
    let (ok, out) = airgapped(
        "./install.sh >/dev/null \
         && ls \"$HOME/.panday/models\" \
         && grep -c 'tiny-test.gguf' INSTALL.md",
    );
    assert!(ok, "{out}");
    assert!(
        out.contains("tiny-test.gguf"),
        "a packed model did not reach $MODEL_DIR:\n{out}"
    );
}
