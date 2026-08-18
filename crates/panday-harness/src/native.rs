//! The native toolset (docs/13 §tools, M13.2).
//!
//! > Native set (phase 1): `read_file`, `write_file`, `edit_file`, `bash`,
//! > `grep`, `glob`, `web_fetch`, `spawn_subagent`.
//!
//! `web_fetch` needs the egress proxy (M14.2) and `spawn_subagent` is M13.6;
//! the other six are here. Every one routes through a [`Sandbox`], so the
//! path policy is enforced by the tier rather than by each tool remembering
//! to check — the mistake that class of code always eventually makes.

use crate::tools::{SideEffects, Tool, ToolCtx, ToolOutcome, ToolReq, ToolSpec};
use panday_sandbox::{ExecChunk, ExecSpec, Sandbox, SandboxHandle, SandboxTier};
use panday_types::Json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The sandbox a tool acts through, plus the workspace root it is scoped to.
#[derive(Clone)]
pub struct Workspace {
    pub sandbox: Arc<dyn Sandbox>,
    pub handle: SandboxHandle,
    /// Canonical workspace root — used by the tools that enumerate files
    /// (`grep`, `glob`), which need a directory walk the `Sandbox` trait
    /// does not expose.
    pub root: PathBuf,
}

impl Workspace {
    pub fn new(sandbox: Arc<dyn Sandbox>, handle: SandboxHandle, root: PathBuf) -> Self {
        Self {
            sandbox,
            handle,
            root,
        }
    }

    /// Enumerate files under the root, refusing to leave it.
    ///
    /// Symlinks are resolved and checked rather than followed blindly: a link
    /// to `/` would otherwise make `grep` walk the whole disk and quietly
    /// exfiltrate whatever it matched.
    fn walk(&self, limit: usize) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];

        while let Some(dir) = stack.pop() {
            if out.len() >= limit {
                break;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(canonical) = std::fs::canonicalize(&path) else {
                    continue;
                };
                if !canonical.starts_with(&self.root) {
                    continue; // a link out of the workspace
                }
                // Noise that is never what anyone is searching for, and that
                // dwarfs the real tree.
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == ".git" || name == "target" || name == "node_modules" {
                    continue;
                }
                if canonical.is_dir() {
                    stack.push(canonical);
                } else {
                    out.push(canonical);
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
        out.sort();
        out
    }

    fn relative(&self, p: &Path) -> String {
        p.strip_prefix(&self.root)
            .unwrap_or(p)
            .to_string_lossy()
            .to_string()
    }
}

fn ok(raw: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        raw: raw.into(),
        is_error: false,
    }
}

/// Tool failures are observations the model can correct, never panics.
fn err(raw: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        raw: raw.into(),
        is_error: true,
    }
}

fn str_arg<'a>(args: &'a Json, key: &str) -> Result<&'a str, ToolOutcome> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| err(format!("missing required argument `{key}`")))
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

pub struct ReadFile(pub Workspace);

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a file from the workspace. Returns its contents with line numbers."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path relative to the workspace root."}
                },
                "required": ["path"]
            }),
        }
    }

    fn requirements(&self) -> ToolReq {
        ToolReq {
            sandbox_tier: SandboxTier::T0InProcess,
            side_effects: SideEffects::None,
            independent: true,
        }
    }

    async fn call(&self, _ctx: ToolCtx, args: Json) -> ToolOutcome {
        let path = match str_arg(&args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };
        match self
            .0
            .sandbox
            .get(&self.0.handle, PathBuf::from(path))
            .await
        {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                // Line numbers so the model can cite and edit precisely.
                let numbered: String = text
                    .lines()
                    .enumerate()
                    .map(|(i, l)| format!("{:>6}\t{l}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n");
                ok(numbered)
            }
            Err(e) => err(format!("read_file({path}): {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

pub struct WriteFile(pub Workspace);

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create or overwrite a file in the workspace.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn requirements(&self) -> ToolReq {
        ToolReq {
            sandbox_tier: SandboxTier::T0InProcess,
            // Overwriting is idempotent for the same content, and replaying it
            // after a crash reaches the same end state — but it is not
            // side-effect free, so `read_only` denies it.
            side_effects: SideEffects::Idempotent,
            independent: false,
        }
    }

    async fn call(&self, _ctx: ToolCtx, args: Json) -> ToolOutcome {
        let (path, content) = match (str_arg(&args, "path"), str_arg(&args, "content")) {
            (Ok(p), Ok(c)) => (p, c),
            (Err(e), _) | (_, Err(e)) => return e,
        };
        match self
            .0
            .sandbox
            .put(
                &self.0.handle,
                PathBuf::from(path),
                content.as_bytes().to_vec(),
            )
            .await
        {
            Ok(()) => ok(format!("wrote {} bytes to {path}", content.len())),
            Err(e) => err(format!("write_file({path}): {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// edit_file
// ---------------------------------------------------------------------------

pub struct EditFile(pub Workspace);

#[async_trait::async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace an exact string in a file. `old` must appear exactly once."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string", "description": "Exact text to replace; must be unique in the file."},
                    "new": {"type": "string"}
                },
                "required": ["path", "old", "new"]
            }),
        }
    }

    fn requirements(&self) -> ToolReq {
        ToolReq {
            sandbox_tier: SandboxTier::T0InProcess,
            side_effects: SideEffects::Idempotent,
            independent: false,
        }
    }

    async fn call(&self, _ctx: ToolCtx, args: Json) -> ToolOutcome {
        let (path, old, new) = match (
            str_arg(&args, "path"),
            str_arg(&args, "old"),
            str_arg(&args, "new"),
        ) {
            (Ok(p), Ok(o), Ok(n)) => (p, o, n),
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
        };

        let bytes = match self
            .0
            .sandbox
            .get(&self.0.handle, PathBuf::from(path))
            .await
        {
            Ok(b) => b,
            Err(e) => return err(format!("edit_file({path}): {e}")),
        };
        let text = String::from_utf8_lossy(&bytes).to_string();

        // Uniqueness is the safety property: an ambiguous match would edit an
        // arbitrary occurrence, and the model would have no way to know which.
        match text.matches(old).count() {
            0 => err(format!(
                "edit_file({path}): `old` not found. The file has {} lines; read it again \
                 — it may have changed.",
                text.lines().count()
            )),
            1 => {
                let updated = text.replacen(old, new, 1);
                match self
                    .0
                    .sandbox
                    .put(&self.0.handle, PathBuf::from(path), updated.into_bytes())
                    .await
                {
                    Ok(()) => ok(format!("edited {path}")),
                    Err(e) => err(format!("edit_file({path}): {e}")),
                }
            }
            n => err(format!(
                "edit_file({path}): `old` appears {n} times and must be unique. \
                 Include more surrounding context."
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

pub struct Grep(pub Workspace);

#[async_trait::async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search workspace file contents with a regular expression.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust regex syntax."},
                    "glob": {"type": "string", "description": "Optional path filter, e.g. \"*.rs\"."}
                },
                "required": ["pattern"]
            }),
        }
    }

    fn requirements(&self) -> ToolReq {
        ToolReq {
            sandbox_tier: SandboxTier::T0InProcess,
            side_effects: SideEffects::None,
            independent: true,
        }
    }

    async fn call(&self, _ctx: ToolCtx, args: Json) -> ToolOutcome {
        let pattern = match str_arg(&args, "pattern") {
            Ok(p) => p,
            Err(e) => return e,
        };
        // A bad pattern is the model's mistake to see, with the reason.
        let re = match regex::Regex::new(pattern) {
            Ok(re) => re,
            Err(e) => return err(format!("grep: invalid pattern `{pattern}`: {e}")),
        };
        let filter = args.get("glob").and_then(|v| v.as_str()).map(glob_to_regex);

        let mut hits = Vec::new();
        for file in self.0.walk(20_000) {
            let rel = self.0.relative(&file);
            if let Some(Ok(f)) = &filter {
                if !f.is_match(&rel) {
                    continue;
                }
            }
            let Ok(bytes) = std::fs::read(&file) else {
                continue;
            };
            // Binary files are noise in a text search.
            if bytes.contains(&0) {
                continue;
            }
            for (i, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                if re.is_match(line) {
                    hits.push(format!("{rel}:{}:{}", i + 1, line.trim_end()));
                    if hits.len() >= 500 {
                        break;
                    }
                }
            }
            if hits.len() >= 500 {
                break;
            }
        }

        if hits.is_empty() {
            // Distinct from an error: "no matches" is a useful answer.
            ok(format!("no matches for /{pattern}/"))
        } else {
            ok(hits.join("\n"))
        }
    }
}

// ---------------------------------------------------------------------------
// glob
// ---------------------------------------------------------------------------

pub struct GlobTool(pub Workspace);

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "List workspace files matching a glob, e.g. \"src/**/*.rs\".".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"pattern": {"type": "string"}},
                "required": ["pattern"]
            }),
        }
    }

    fn requirements(&self) -> ToolReq {
        ToolReq {
            sandbox_tier: SandboxTier::T0InProcess,
            side_effects: SideEffects::None,
            independent: true,
        }
    }

    async fn call(&self, _ctx: ToolCtx, args: Json) -> ToolOutcome {
        let pattern = match str_arg(&args, "pattern") {
            Ok(p) => p,
            Err(e) => return e,
        };
        let re = match glob_to_regex(pattern) {
            Ok(re) => re,
            Err(e) => return err(format!("glob: invalid pattern `{pattern}`: {e}")),
        };

        let matched: Vec<String> = self
            .0
            .walk(20_000)
            .iter()
            .map(|p| self.0.relative(p))
            .filter(|rel| re.is_match(rel))
            .collect();

        if matched.is_empty() {
            ok(format!("no files match `{pattern}`"))
        } else {
            ok(matched.join("\n"))
        }
    }
}

/// Translate a glob into an anchored regex.
///
/// Hand-rolled rather than adding the `glob` crate: the syntax we need is
/// three special forms, and `regex` is already a dependency.
/// `**` crosses directory separators, `*` does not, `?` is one non-separator.
fn glob_to_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    let mut re = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    re.push_str(".*");
                    i += 2;
                    // `**/` should also match zero directories, so `**/*.rs`
                    // finds `main.rs` at the root and not just nested files.
                    if chars.get(i) == Some(&'/') {
                        re.push_str("(?:/)?");
                        i += 1;
                    }
                    continue;
                }
                re.push_str("[^/]*");
            }
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    regex::Regex::new(&re)
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------

pub struct Bash(pub Workspace);

#[async_trait::async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command in the sandboxed workspace. No network access."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
                "required": ["cmd"]
            }),
        }
    }

    fn requirements(&self) -> ToolReq {
        ToolReq {
            // The only tool needing a real jail: it runs arbitrary commands.
            sandbox_tier: SandboxTier::T2OsJail,
            // NOT `Irreversible`, despite the temptation.
            //
            // `side_effects` drives two different decisions and `bash` needs
            // different answers for each:
            //
            //  * consent — docs/13's profile table says `dev` allows
            //    "read/edit/test", so running the test suite must not prompt.
            //    Marking the whole tool Irreversible forces Ask in EVERY
            //    profile including `unleashed`, which makes M13.2's
            //    "unattended" acceptance unreachable by construction.
            //  * replay safety — replaying an arbitrary shell command after a
            //    crash is genuinely unsafe, so `Irreversible` is the right
            //    answer there.
            //
            // The spec's mechanism for the gap is per-argument rules, not
            // per-tool: docs/13 §permissions gives `bash(rm -rf*) → Ask` "even
            // in unleashed". So the tool declares the common case and the
            // dangerous commands are gated by pattern (M13.3). M13.5 must not
            // read `Idempotent` here as "safe to re-run on resume" — see the
            // note added to docs/13.
            side_effects: SideEffects::Idempotent,
            independent: false,
        }
    }

    async fn call(&self, _ctx: ToolCtx, args: Json) -> ToolOutcome {
        use futures_util::StreamExt;

        let cmd = match str_arg(&args, "cmd") {
            Ok(c) => c,
            Err(e) => return e,
        };

        let stream = self
            .0
            .sandbox
            .exec(
                &self.0.handle,
                ExecSpec {
                    cmd: vec!["/bin/sh".into(), "-c".into(), cmd.to_string()],
                    cwd: None,
                    pty: false,
                    stdin: None,
                },
            )
            .await;

        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => return err(format!("bash: {e}")),
        };

        let mut out = String::new();
        let mut code = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(ExecChunk::Stdout(b)) | Ok(ExecChunk::Stderr(b)) => {
                    out.push_str(&String::from_utf8_lossy(&b))
                }
                Ok(ExecChunk::Exit { code: c, .. }) => code = Some(c),
                Err(e) => {
                    // A limit breach still carries whatever ran before it —
                    // discarding that would hide why the command was slow.
                    out.push_str(&format!("\n[{e}]"));
                    return ToolOutcome {
                        raw: out,
                        is_error: true,
                    };
                }
            }
        }

        match code {
            Some(0) => ok(out),
            Some(c) => ToolOutcome {
                raw: format!("{out}\n[exit status {c}]"),
                is_error: true,
            },
            None => ToolOutcome {
                raw: format!("{out}\n[no exit status reported]"),
                is_error: true,
            },
        }
    }
}

/// Register the phase-1 native set.
///
/// `web_fetch` (needs the egress proxy, M14.2) and `spawn_subagent` (M13.6)
/// are deliberately absent rather than stubbed — a tool the model can call
/// but that cannot work is worse than one it never sees.
pub fn register_native(registry: &mut crate::tools::ToolRegistry, ws: Workspace) {
    registry.register(Box::new(ReadFile(ws.clone())));
    registry.register(Box::new(WriteFile(ws.clone())));
    registry.register(Box::new(EditFile(ws.clone())));
    registry.register(Box::new(Grep(ws.clone())));
    registry.register(Box::new(GlobTool(ws.clone())));
    registry.register(Box::new(Bash(ws)));
}
