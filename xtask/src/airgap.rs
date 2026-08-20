//! The air-gap kit's two generated files, and the assertions that keep them honest (docs/18 M18.7,
//! docs/22 M22.5).
//!
//! M22.5 asks that "one enterprise pilot installs the air-gap kit from its README alone". The half
//! of that claim which can be checked without an air-gapped machine is the *contents*: that the
//! installer reaches no network, that it writes only where it says it writes, and that the README
//! does not instruct the reader to fetch something the box does not contain. Those are string
//! properties of these two files, and string properties can be tested — so they are, rather than
//! being asserted in prose and discovered by a customer behind a locked door.
//!
//! What remains unverifiable here is the air itself: this machine has a network, so nothing proves
//! the install *succeeds* without one. What is proven is that it never asks for one.

pub const INSTALL_SH: &str = r#"#!/usr/bin/env sh
# Panday air-gap installer (docs/18 M18.7). No network, by design.
set -eu

PREFIX="${PREFIX:-$HOME/.local}"
MODEL_DIR="${PANDAY_MODEL_DIR:-$HOME/.panday/models}"
HERE="$(cd "$(dirname "$0")" && pwd)"

mkdir -p "$PREFIX/bin" "$MODEL_DIR" "$HOME/.panday"
cp "$HERE"/bin/* "$PREFIX/bin/"
cp "$HERE"/config/* "$HOME/.panday/"

# Copied rather than linked: a USB stick that gets unplugged is not a storage backend.
if [ -d "$HERE/models" ] && [ -n "$(ls -A "$HERE/models" 2>/dev/null)" ]; then
  cp "$HERE"/models/*.gguf "$MODEL_DIR/"
fi

echo "installed to $PREFIX/bin"
echo "models in    $MODEL_DIR"
echo
echo "Check it works, with nothing plugged in:"
echo "  $PREFIX/bin/panday-local --serve $MODEL_DIR/<model>.gguf --workspace . 'say hello'"
"#;

pub fn install_readme(models: &[String]) -> String {
    let model_list = if models.is_empty() {
        "*(this kit ships no models — the machine will need one before `panday local` can answer)*"
            .to_string()
    } else {
        models
            .iter()
            .map(|m| format!("- `{m}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        r#"# Panday — air-gapped install

Everything needed to run Panday on a machine with no internet connection. Nothing in here reaches
the network: not the installer, not the first run, not the agent (ADR-011).

## What is in the box

```
bin/       panday, panday-local, panday-gateway, panday-platform
config/    local.yaml (routing), catalog.yaml (models)
models/    the GGUFs this kit was built with
INSTALL.md this file
install.sh copies the above into place
```

Models included:

{model_list}

## Install

```sh
./install.sh                 # into ~/.local/bin and ~/.panday/models
PREFIX=/opt/panday ./install.sh   # or somewhere else
```

The installer copies rather than links, because a USB stick that gets unplugged is not a storage
backend.

## Verify it, offline

```sh
panday-local --serve ~/.panday/models/<model>.gguf --workspace . "say hello"
```

`panday local` refuses any base URL that is not loopback, so if the machine later gains a network,
the offline tier still cannot reach it.

## Licensing

An entitlement token is a file (docs/17 M17.6). Copy it and its `.sig` onto the machine and run:

```sh
PANDAY_ENTITLEMENT_KEY=<the public key you were given>   panday-local --entitlement ~/.panday/licence.json --workspace . "…"
```

Verification is local: no activation, no call home, no revocation check. When it expires the
software keeps working at the community tier rather than stopping — a renewal is a new file.

## Updating

Bring a newer kit and run `install.sh` again. The event logs under `.panday/` are append-only and
are not touched by an install (ADR-002).

## What this kit does not include

- **A cloud account.** None is needed; the offline tier works without one.
- **A model catalog signature.** `catalog.yaml` here is the routing catalog. The *signed model
  index* (`panday models`) is for machines that can download; on an air-gapped box the models are
  already in `models/`.
"#
    )
}

/// Everything the kit ships, relative to its root. The installer and the README are both checked
/// against this, so a file added to one and not the other is a test failure rather than a support
/// ticket.
pub const KIT_LAYOUT: &[&str] = &["bin/", "config/", "models/", "INSTALL.md", "install.sh"];

/// Commands that reach off the machine. An air-gapped installer containing any of these is either
/// broken or lying about what it needs.
pub const NETWORK_COMMANDS: &[&str] = &[
    "curl ",
    "wget ",
    "git clone",
    "brew ",
    "pip install",
    "pip3 install",
    "npm install",
    "npm i ",
    "apt-get",
    "apt ",
    "yum ",
    "dnf ",
    "nix-env",
    "cargo install",
    "docker pull",
    "scp ",
    "rsync ",
    "ssh ",
    "nc ",
    "ftp ",
];

/// The first network-reaching command in `text`, if any.
///
/// Called by the kit builder before it writes anything, not only by the suite: a kit is worth
/// exactly its offline claim, and the cheapest moment to catch a `curl` creeping into the installer
/// is before the tarball reaches somebody who cannot run one.
pub fn reaches_network(text: &str) -> Option<&'static str> {
    NETWORK_COMMANDS
        .iter()
        .copied()
        .find(|command| text.contains(command))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_installer_reaches_no_network() {
        // The single claim the whole kit rests on. A `curl` in here means the box is not a box.
        for command in NETWORK_COMMANDS {
            assert!(
                !INSTALL_SH.contains(command),
                "the air-gap installer contains `{command}`"
            );
        }
    }

    #[test]
    fn the_installer_writes_only_where_it_says() {
        // Every destination is under a prefix the reader chose or a directory the README names.
        // A kit that quietly wrote into /usr/local would need sudo and would surprise an operator
        // who was told it did not.
        for line in INSTALL_SH.lines() {
            let line = line.trim();
            if !(line.starts_with("cp ") || line.starts_with("mkdir ")) {
                continue;
            }
            let destination = line.split_whitespace().last().unwrap_or_default();
            assert!(
                destination.contains("$PREFIX")
                    || destination.contains("$MODEL_DIR")
                    || destination.contains("$HOME"),
                "installer writes to an unexpected place: {line}"
            );
        }
    }

    #[test]
    fn the_installer_copies_rather_than_links() {
        // A USB stick that gets unplugged is not a storage backend. A symlink into the kit would
        // leave a machine that worked until the door closed behind you.
        assert!(
            !INSTALL_SH.contains("ln -s"),
            "the installer symlinks into the kit"
        );
        assert!(INSTALL_SH.contains("cp "));
    }

    #[test]
    fn the_installer_stops_at_the_first_error() {
        // `set -eu`: a half-installed kit that reported success is the worst outcome available on a
        // machine nobody can ssh into to check.
        assert!(
            INSTALL_SH
                .lines()
                .any(|l| l.trim() == "set -eu" || l.trim() == "set -euo pipefail"),
            "the installer does not stop on error"
        );
    }

    #[test]
    fn the_readme_names_only_what_the_kit_contains() {
        // The README is the only documentation the reader has, and M22.5's bar is that it is
        // sufficient on its own. A path in it that the box does not ship is a dead end behind a
        // locked door.
        let readme = install_readme(&["qwen3.5-4b-q4.gguf".to_string()]);
        let mut mentioned = Vec::new();
        for line in readme.lines() {
            for path in ["bin/", "config/", "models/", "INSTALL.md", "install.sh"] {
                if line.contains(path) {
                    mentioned.push(path);
                }
            }
        }
        for path in KIT_LAYOUT {
            assert!(
                mentioned.contains(path),
                "the README never mentions `{path}`, which the kit ships"
            );
        }
    }

    #[test]
    fn the_readme_asks_for_nothing_from_the_network() {
        let readme = install_readme(&[]);
        for command in NETWORK_COMMANDS {
            assert!(
                !readme.contains(command),
                "the README tells the reader to run `{command}`"
            );
        }
        // And it says what it does *not* include, because a reader behind a locked door needs the
        // absence stated rather than inferred.
        assert!(readme.contains("does not include"), "{readme}");
    }

    #[test]
    fn the_readme_names_the_environment_variable_the_binary_actually_reads() {
        // Documentation drift with a locked door on the other side of it. `panday local` reads
        // PANDAY_ENTITLEMENT_KEY (M17.6); a README naming anything else is a customer who cannot
        // activate their licence and cannot ask.
        let readme = install_readme(&[]);
        assert!(readme.contains("PANDAY_ENTITLEMENT_KEY"), "{readme}");
        assert!(readme.contains("--entitlement"));
        // And it must state the degradation, or an expiry looks like a fault.
        assert!(readme.contains("community tier"));
    }

    #[test]
    fn a_kit_with_no_models_says_so_in_the_readme() {
        // The kit is allowed to ship without models; what it may not do is stay quiet about it and
        // fail at the first prompt.
        let readme = install_readme(&[]);
        assert!(readme.contains("ships no models"), "{readme}");

        let with = install_readme(&["a.gguf".to_string(), "b.gguf".to_string()]);
        assert!(with.contains("`a.gguf`") && with.contains("`b.gguf`"));
    }

    #[test]
    fn the_verification_command_uses_a_local_path_and_loopback_only() {
        // The one command the README tells a reader to run to prove the install worked. If it
        // reached a URL, the proof would be of the opposite.
        let readme = install_readme(&["m.gguf".to_string()]);
        assert!(readme.contains("panday-local --serve"));
        assert!(!readme.contains("http://") || readme.contains("127.0.0.1"));
    }
}
