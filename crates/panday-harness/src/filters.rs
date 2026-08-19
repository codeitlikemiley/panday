//! The `pre_tool` filter pack (M20.2, docs/20 T1).
//!
//! > "**pre_tool hooks** run deterministic filters (e.g., block `curl … | sh`,
//! > block writes outside workspace) — cheap, testable, model-free."
//!
//! Model-free is the whole point. Everything else in the injection defence
//! depends on a model behaving (the provenance rule) or a human being present (the
//! gate); these rules hold when neither is true. They are also the cheapest layer,
//! so they run first and their cost is a string scan.
//!
//! ## What this is not
//!
//! It is not a sandbox and not a substitute for one. A determined command can be
//! written to evade any pattern list — `$(printf '\\143url')` is `curl` — so a
//! filter that "blocks egress" would be a false promise; T2's `--unshare-net` is
//! what actually blocks egress (docs/14). These rules catch the *unobfuscated*
//! shape of an attack, which is what injected instructions overwhelmingly look
//! like, because the attacker is writing for a model rather than for a parser.
//!
//! Each rule therefore states what it catches and what it does not, and the tests
//! include the evasions that get through — so nobody reads this file as a
//! boundary.

use crate::hooks::{Hook, PreTool};
use panday_types::Json;
use std::path::{Component, Path, PathBuf};

/// A deterministic pre-tool rule.
pub trait Filter: Send + Sync {
    fn name(&self) -> &'static str;
    /// `None` to allow, `Some(reason)` to veto.
    fn check(&self, tool: &str, args: &Json) -> Option<String>;
}

/// Runs a list of filters, first veto wins.
///
/// Registered as one `Hook` rather than one per rule so the veto reason names the
/// rule that fired: "blocked by pipe_to_shell" is actionable, "blocked by the
/// filter pack" sends someone reading code.
pub struct FilterPack {
    filters: Vec<Box<dyn Filter>>,
}

impl FilterPack {
    pub fn new(filters: Vec<Box<dyn Filter>>) -> Self {
        Self { filters }
    }

    /// The default pack: the rules docs/20 names, plus the ones its threat list
    /// implies. Every one is a *shape* an injected instruction takes.
    pub fn default_pack(workspace: impl Into<PathBuf>) -> Self {
        Self::new(vec![
            Box::new(PipeToShell),
            Box::new(WriteOutsideWorkspace::new(workspace)),
            Box::new(DestructiveRoot),
            Box::new(SecretEcho),
            Box::new(CredentialPathRead),
        ])
    }

    pub fn len(&self) -> usize {
        self.filters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }
}

impl Hook for FilterPack {
    fn name(&self) -> &str {
        "filter_pack"
    }

    fn pre_tool(&self, tool: &str, args: &Json) -> PreTool {
        for f in &self.filters {
            if let Some(reason) = f.check(tool, args) {
                return PreTool::Veto(format!("blocked by {}: {reason}", f.name()));
            }
        }
        PreTool::Proceed
    }
}

/// Every string value in the arguments, at any depth.
///
/// Scanning values rather than the serialized JSON: the serialized form contains
/// escapes and key names, so `"cmd":"echo curl"` and a key literally named `curl`
/// would look the same to a substring check on the whole blob.
fn strings(args: &Json) -> Vec<&str> {
    let mut out = Vec::new();
    fn walk<'a>(v: &'a Json, out: &mut Vec<&'a str>) {
        match v {
            Json::String(s) => out.push(s.as_str()),
            Json::Array(items) => items.iter().for_each(|i| walk(i, out)),
            Json::Object(map) => map.values().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    walk(args, &mut out);
    out
}

/// `curl … | sh` and its family — docs/20's named example.
///
/// Catches: a download piped into an interpreter, in the plain form. Does not
/// catch: obfuscation, a two-step download-then-run, or a fetch by a tool other
/// than a shell. The last is deliberate — a `read_file` of a URL is not a shell
/// pipeline, and widening this rule to "any fetch" would veto ordinary work.
pub struct PipeToShell;

const FETCHERS: &[&str] = &["curl", "wget", "http ", "https ", "nc ", "ftp "];
const INTERPRETERS: &[&str] = &[
    "sh", "bash", "zsh", "python", "python3", "ruby", "perl", "node",
];

impl Filter for PipeToShell {
    fn name(&self) -> &'static str {
        "pipe_to_shell"
    }

    fn check(&self, _tool: &str, args: &Json) -> Option<String> {
        for s in strings(args) {
            let lower = s.to_lowercase();
            if !FETCHERS.iter().any(|f| lower.contains(f)) {
                continue;
            }
            // A pipe into an interpreter, or a process substitution feeding one.
            for interp in INTERPRETERS {
                for pattern in [format!("| {interp}"), format!("|{interp}")] {
                    if lower.contains(&pattern) {
                        return Some(format!(
                            "a download piped into `{interp}` executes code nobody read"
                        ));
                    }
                }
            }
            if lower.contains("<(") || lower.contains("$(curl") || lower.contains("`curl") {
                return Some(
                    "a download substituted into a command executes code nobody read".into(),
                );
            }
        }
        None
    }
}

/// Writes outside the workspace — docs/20's other named example.
///
/// Catches: an absolute path outside the workspace, and a relative path that
/// escapes it via `..`, in any argument that names a path. Does not catch: a
/// symlink inside the workspace pointing out of it — that is what T2's mount
/// scoping is for, and this rule does not touch the filesystem to find out
/// (a filter that stats paths is a filter with a TOCTOU race).
pub struct WriteOutsideWorkspace {
    workspace: PathBuf,
}

impl WriteOutsideWorkspace {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }
}

/// Lexical normalization: resolve `.` and `..` without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

const WRITE_TOOLS: &[&str] = &["write_file", "edit_file", "delete_file", "move_file"];

impl Filter for WriteOutsideWorkspace {
    fn name(&self) -> &'static str {
        "write_outside_workspace"
    }

    fn check(&self, tool: &str, args: &Json) -> Option<String> {
        if !WRITE_TOOLS.contains(&tool) {
            return None;
        }
        let workspace = normalize(&self.workspace);
        for key in ["path", "dest", "to", "file"] {
            let Some(raw) = args.get(key).and_then(|v| v.as_str()) else {
                continue;
            };
            // A `~`-relative path is not absolute, so joining it to the workspace
            // would place `workspace/~/.ssh/authorized_keys` "inside". Whatever the
            // caller meant — `$HOME` after shell expansion, or a literal directory
            // named `~` — it is not the workspace path they wrote, so it is refused
            // rather than guessed at.
            if raw.starts_with('~') {
                return Some(format!(
                    "`{raw}` is a home-relative path, not a workspace path"
                ));
            }
            let candidate = if Path::new(raw).is_absolute() {
                normalize(Path::new(raw))
            } else {
                normalize(&workspace.join(raw))
            };
            if !candidate.starts_with(&workspace) {
                return Some(format!(
                    "`{raw}` resolves outside the workspace ({})",
                    workspace.display()
                ));
            }
        }
        None
    }
}

/// `rm -rf /` and friends: destruction whose target is the machine rather than
/// the work.
///
/// Not a general "destructive command" rule — deleting files is normal work, and
/// a filter that vetoed it would be turned off within a day. This catches the
/// specific shapes that are never the intent of a task.
pub struct DestructiveRoot;

impl Filter for DestructiveRoot {
    fn name(&self) -> &'static str {
        "destructive_root"
    }

    fn check(&self, _tool: &str, args: &Json) -> Option<String> {
        for s in strings(args) {
            let compact = s.split_whitespace().collect::<Vec<_>>().join(" ");
            let lower = compact.to_lowercase();
            for pattern in [
                "rm -rf /",
                "rm -fr /",
                "rm -rf ~",
                "rm -rf $home",
                "mkfs",
                "dd if=/dev/zero of=/dev/",
                ":(){ :|:& };:",
            ] {
                if lower.contains(pattern) {
                    return Some(format!("`{pattern}` destroys the machine, not the task"));
                }
            }
            // `rm -rf /*` and `rm -rf /etc` — a slash-rooted absolute target.
            if (lower.starts_with("rm -rf /") || lower.starts_with("rm -fr /"))
                && !lower.starts_with("rm -rf ./")
            {
                return Some("a recursive delete rooted at `/` is never the task".into());
            }
        }
        None
    }
}

/// A command that reads a secret out of the environment and puts it somewhere.
///
/// docs/20 T4: secrets are injected into a sandbox's env for the tool that
/// declared them; the exfiltration step is what this catches. `echo $TOKEN` alone
/// is not vetoed — printing a variable is a normal debugging act, and the tool
/// output is scrubbed anyway (`ScrubSecrets`). What is vetoed is a secret-shaped
/// variable in the same command as a network destination.
pub struct SecretEcho;

const SECRET_VAR_MARKERS: &[&str] = &["token", "secret", "key", "password", "credential"];

impl Filter for SecretEcho {
    fn name(&self) -> &'static str {
        "secret_egress"
    }

    fn check(&self, _tool: &str, args: &Json) -> Option<String> {
        for s in strings(args) {
            let lower = s.to_lowercase();
            let has_secret_var = lower.contains('$')
                && SECRET_VAR_MARKERS.iter().any(|m| {
                    lower
                        .split('$')
                        .skip(1)
                        .any(|after| after.trim_start_matches('{').to_lowercase().contains(m))
                });
            let has_destination = FETCHERS.iter().any(|f| lower.contains(f))
                || lower.contains("@")
                    && (lower.contains("scp ") || lower.contains("ssh ") || lower.contains("mail"));
            if has_secret_var && has_destination {
                return Some("a secret-shaped variable is being sent off the machine".into());
            }
        }
        None
    }
}

/// Reads of well-known credential files.
///
/// The T2 tiers deny these at the OS level where they can (docs/14), but a
/// *native* T0 tool runs in our process against the user's real filesystem, and
/// this is the layer that covers it.
pub struct CredentialPathRead;

const CREDENTIAL_PATHS: &[&str] = &[
    // The whole directory, not just private keys: `authorized_keys` is a
    // persistence vector rather than a secret, and `known_hosts` is a map of where
    // this machine can reach. None of it is a project file.
    ".ssh/",
    ".aws/credentials",
    ".config/gcloud/",
    ".gnupg/",
    ".netrc",
    ".npmrc",
    ".docker/config.json",
    "/etc/shadow",
    "/etc/master.passwd",
    ".kube/config",
    ".git-credentials",
];

impl Filter for CredentialPathRead {
    fn name(&self) -> &'static str {
        "credential_path"
    }

    fn check(&self, _tool: &str, args: &Json) -> Option<String> {
        for s in strings(args) {
            let lower = s.to_lowercase();
            if let Some(hit) = CREDENTIAL_PATHS.iter().find(|p| lower.contains(**p)) {
                return Some(format!("`{hit}` holds credentials, not project files"));
            }
        }
        None
    }
}
