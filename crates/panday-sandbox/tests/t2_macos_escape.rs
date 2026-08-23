//! T2 macOS escape suite (M14.3) — the "parity subset" docs/14 calls for.
//!
//! > "Must-fail tests: read `/etc/shadow` · write outside workspace · connect
//! > to non-allowlisted IP ... exceed wall-clock ... Suite runs in CI on every
//! > sandbox PR — an isolation regression is a broken build, same as a type
//! > error."
//!
//! Every must-fail case here is paired with a **positive control** proving the
//! same operation succeeds unsandboxed. Without that, a test suite passes just
//! as happily against a sandbox that breaks everything, or against a machine
//! where the operation was never possible.

#![cfg(target_os = "macos")]

use futures_util::StreamExt;
use panday_sandbox::{
    ExecChunk, ExecSpec, FsPolicy, Limits, NetPolicy, Sandbox, SandboxError, SandboxHandle,
    SandboxPolicy, SandboxTier, SeatbeltProfile, SessionSpec, T2MacosSandbox,
};
use std::path::{Path, PathBuf};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "panday-t2-{tag}-{}-{nanos}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Jail {
    _root: TempDir,
    workspace: PathBuf,
    outside: PathBuf,
    sandbox: T2MacosSandbox,
    handle: SandboxHandle,
}

async fn jail(wall_ms: u64) -> Option<Jail> {
    if !T2MacosSandbox::available() {
        return None;
    }
    let root = TempDir::new("escape");
    let workspace = root.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "TOP SECRET").unwrap();

    let sandbox = T2MacosSandbox::new();
    let handle = sandbox
        .create(SessionSpec {
            tier: SandboxTier::T2OsJail,
            policy: SandboxPolicy {
                fs: FsPolicy {
                    workspace_rw: workspace.clone(),
                    staged_ro: vec![],
                },
                net: NetPolicy::default(),
                limits: Limits {
                    wall_clock_ms: wall_ms,
                    ..Default::default()
                },
                ..Default::default()
            },
        })
        .await
        .expect("create T2 session");

    Some(Jail {
        workspace: std::fs::canonicalize(&workspace).unwrap(),
        outside: std::fs::canonicalize(&outside).unwrap(),
        _root: root,
        sandbox,
        handle,
    })
}

struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    limit_hit: bool,
}

async fn run(j: &Jail, script: &str) -> Run {
    let stream = j
        .sandbox
        .exec(
            &j.handle,
            ExecSpec {
                cmd: vec!["/bin/sh".into(), "-c".into(), script.into()],
                cwd: None,
                pty: false,
                stdin: None,
            },
        )
        .await
        .expect("exec");

    let mut r = Run {
        stdout: String::new(),
        stderr: String::new(),
        code: None,
        limit_hit: false,
    };
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item {
            Ok(ExecChunk::Stdout(b)) => r.stdout.push_str(&String::from_utf8_lossy(&b)),
            Ok(ExecChunk::Stderr(b)) => r.stderr.push_str(&String::from_utf8_lossy(&b)),
            Ok(ExecChunk::Exit { code, .. }) => r.code = Some(code),
            Err(SandboxError::LimitExceeded(_)) => {
                r.limit_hit = true;
                break;
            }
            Err(e) => panic!("unexpected exec error: {e}"),
        }
    }
    r
}

// ---------------------------------------------------------------------------
// Positive controls — a jail that breaks everything must not look like a pass
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_command_runs_and_reports_its_output_and_exit_code() {
    let Some(j) = jail(30_000).await else { return };
    let r = run(&j, "echo hello from the jail; exit 0").await;
    assert!(r.stdout.contains("hello from the jail"), "{:?}", r.stdout);
    assert_eq!(r.code, Some(0));
}

#[tokio::test]
async fn a_nonzero_exit_is_reported_not_swallowed() {
    let Some(j) = jail(30_000).await else { return };
    let r = run(&j, "echo to-stderr >&2; exit 3").await;
    assert_eq!(r.code, Some(3));
    assert!(r.stderr.contains("to-stderr"));
}

#[tokio::test]
async fn writing_inside_the_workspace_is_allowed() {
    // The control for the write-escape test below.
    let Some(j) = jail(30_000).await else { return };
    let r = run(&j, "echo written > ./inside.txt && cat ./inside.txt").await;
    assert!(
        r.stdout.contains("written"),
        "stdout={:?} stderr={:?} code={:?} ws={}",
        r.stdout,
        r.stderr,
        r.code,
        j.workspace.display()
    );
    assert!(j.workspace.join("inside.txt").exists());
}

// ---------------------------------------------------------------------------
// Must fail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn writing_outside_the_workspace_is_refused() {
    let Some(j) = jail(30_000).await else { return };
    let target = j.outside.join("pwned.txt");

    let r = run(&j, &format!("echo pwned > {}", target.display())).await;

    assert!(
        !target.exists(),
        "ESCAPE: the sandbox let a write land outside the workspace at {}",
        target.display()
    );
    assert_ne!(r.code, Some(0), "the shell should have reported failure");
}

#[tokio::test]
async fn appending_outside_the_workspace_is_refused() {
    // `>>` takes a different path through the kernel than `>`.
    let Some(j) = jail(30_000).await else { return };
    let target = j.outside.join("secret.txt");
    let before = std::fs::read_to_string(&target).unwrap();

    run(&j, &format!("echo appended >> {}", target.display())).await;

    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        before,
        "ESCAPE: a file outside the workspace was modified"
    );
}

#[tokio::test]
async fn deleting_outside_the_workspace_is_refused() {
    let Some(j) = jail(30_000).await else { return };
    let target = j.outside.join("secret.txt");

    run(&j, &format!("rm -f {}", target.display())).await;

    assert!(
        target.exists(),
        "ESCAPE: a file outside the workspace was deleted"
    );
}

#[tokio::test]
async fn network_egress_is_denied_by_default() {
    let Some(j) = jail(30_000).await else { return };

    // Positive control first: prove the host actually has working egress, so
    // a passing test cannot merely mean "this machine is offline".
    let unsandboxed = std::process::Command::new("/usr/bin/curl")
        .args(["-s", "-m", "8", "-o", "/dev/null", "https://example.com"])
        .status();
    let host_online = matches!(unsandboxed, Ok(s) if s.success());
    if !host_online {
        eprintln!("skipping: host has no egress, so the denial proves nothing");
        return;
    }

    let r = run(
        &j,
        "curl -s -m 8 -o /dev/null https://example.com; echo curl=$?",
    )
    .await;
    assert!(
        !r.stdout.contains("curl=0"),
        "ESCAPE: network egress succeeded under a default-deny policy: {}",
        r.stdout
    );
}

#[tokio::test]
async fn a_command_that_overruns_the_wall_clock_is_killed() {
    let Some(j) = jail(700).await else { return };
    let began = std::time::Instant::now();

    let r = run(&j, "sleep 30; echo should-never-print").await;

    assert!(r.limit_hit, "the wall-clock breach was not reported");
    assert!(!r.stdout.contains("should-never-print"));
    assert!(
        began.elapsed() < std::time::Duration::from_secs(10),
        "the kill did not actually happen promptly: {:?}",
        began.elapsed()
    );
}

#[tokio::test]
async fn secrets_are_not_inherited_into_the_jail() {
    // docs/20 T4: env is scrubbed; injection is explicit.
    //
    // Uses a variable the parent already has rather than setting one:
    // `std::env::set_var` mutates process-global state that every concurrent
    // test's child inherits at spawn time, which makes the whole suite flaky.
    let Some(j) = jail(30_000).await else { return };
    let Some(user) = std::env::var_os("USER") else {
        eprintln!("skipping: no USER in the parent environment to leak");
        return;
    };
    assert!(!user.is_empty());

    // Only USER is a valid witness: `sh` synthesises SHELL/PWD/IFS itself, so
    // their presence inside the jail is not evidence of inheritance.
    let r = run(&j, "echo user=[$USER] home=[$HOME]").await;

    assert!(
        r.stdout.contains("user=[]"),
        "ESCAPE: the parent environment leaked into the jail: {}",
        r.stdout
    );
    // HOME is deliberately re-set to the workspace, not inherited.
    assert!(
        r.stdout
            .contains(&format!("home=[{}]", j.workspace.display())),
        "HOME should point at the workspace, got: {}",
        r.stdout
    );
}

// ---------------------------------------------------------------------------
// Profile generation
// ---------------------------------------------------------------------------

#[test]
fn a_path_containing_a_quote_cannot_inject_profile_directives() {
    // The profile is generated text built from paths we do not control. A
    // workspace named `x"` could otherwise close the literal and append
    // `(allow default)`.
    let fs = FsPolicy {
        workspace_rw: PathBuf::from("/tmp/evil\") (allow default) (subpath \"/"),
        staged_ro: vec![],
    };
    let profile = SeatbeltProfile::from_policy(&fs, &NetPolicy::default()).expect("must escape");

    // The payload text appears inside the (escaped) path literal, which is
    // inert. What matters is whether it escaped the literal and became a
    // directive — so strip every string literal and inspect what is left.
    let directives = strip_sbpl_literals(&profile.0);
    assert!(
        !directives.contains("allow default"),
        "profile injection succeeded — payload reached directive level:\n{directives}"
    );
    assert!(
        profile.0.contains("\\\""),
        "the embedded quote was not escaped:\n{}",
        profile.0
    );
}

/// Remove SBPL string literals, honouring backslash escapes, leaving only the
/// directive skeleton. A payload that never leaves a literal cannot execute.
fn strip_sbpl_literals(profile: &str) -> String {
    let mut out = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in profile.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn a_path_with_control_characters_is_refused_rather_than_mangled() {
    let fs = FsPolicy {
        workspace_rw: PathBuf::from("/tmp/bad\nname"),
        staged_ro: vec![],
    };
    let err = SeatbeltProfile::from_policy(&fs, &NetPolicy::default()).unwrap_err();
    assert!(matches!(err, SandboxError::PolicyViolation(_)), "{err}");
}

#[test]
fn the_profile_denies_by_default_and_scopes_writes() {
    let fs = FsPolicy {
        workspace_rw: PathBuf::from("/tmp/ws"),
        staged_ro: vec![PathBuf::from("/tmp/staged")],
    };
    let p = SeatbeltProfile::from_policy(&fs, &NetPolicy::default())
        .unwrap()
        .0;

    assert!(p.contains("(deny default)"));
    assert!(
        p.contains("(deny network*)"),
        "default policy must deny egress"
    );
    assert!(p.contains("(allow file-write* (subpath \"/tmp/ws\"))"));
    assert!(
        !p.contains("(allow file-write* (subpath \"/tmp/staged\"))"),
        "staged inputs must be readable but never writable"
    );
}

#[test]
fn sensitive_paths_are_denied_for_reading() {
    let fs = FsPolicy {
        workspace_rw: PathBuf::from("/tmp/ws"),
        staged_ro: vec![],
    };
    let p = SeatbeltProfile::from_policy(&fs, &NetPolicy::default())
        .unwrap()
        .0;
    assert!(p.contains("/etc/master.passwd"));
    if std::env::var_os("HOME").is_some() {
        assert!(p.contains(".ssh"), "ssh keys must be denied");
        assert!(p.contains("Keychains"));
    }
}

#[tokio::test]
async fn ssh_keys_are_unreadable_from_inside_the_jail() {
    let Some(j) = jail(30_000).await else { return };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let ssh_dir = home.join(".ssh");
    if !ssh_dir.exists() {
        eprintln!("skipping: no ~/.ssh on this host");
        return;
    }

    let r = run(&j, &format!("ls {} 2>&1; echo rc=$?", ssh_dir.display())).await;
    assert!(
        !r.stdout.contains("rc=0"),
        "ESCAPE: ~/.ssh was listable from inside the jail: {}",
        r.stdout
    );
}

#[tokio::test]
async fn the_wrong_tier_is_refused() {
    if !T2MacosSandbox::available() {
        return;
    }
    let s = T2MacosSandbox::new();
    let err = s
        .create(SessionSpec {
            tier: SandboxTier::T3MicroVm,
            policy: SandboxPolicy::default(),
        })
        .await
        .expect_err("must not pretend to be T3");
    assert!(matches!(
        err,
        SandboxError::Unsupported(SandboxTier::T3MicroVm)
    ));
}

// ── M14.8: a policy no tier can enforce is refused ───────────────────────────

#[tokio::test]
async fn a_named_host_in_the_allowlist_is_refused_not_granted() {
    // docs/14 §policy specifies a per-domain allowlist served by an egress proxy. The proxy is not
    // built, so this tier cannot tell `api.github.com` from anything else.
    //
    // The bug this pins: a non-empty allowlist used to mean "the proxy filters, so open the gate",
    // and with no proxy that granted the payload the host's whole network — loopback services, the
    // LAN, the cloud metadata endpoint. Asking for one host got everything. Nothing in the tree
    // constructed such a policy, so it was never reachable; it was one caller away.
    if !T2MacosSandbox::available() {
        return;
    }
    let root = TempDir::new("allowlist");
    let workspace = root.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let err = T2MacosSandbox::new()
        .create(SessionSpec {
            tier: SandboxTier::T2OsJail,
            policy: SandboxPolicy {
                fs: FsPolicy {
                    workspace_rw: workspace,
                    staged_ro: vec![],
                },
                net: NetPolicy {
                    via_proxy: true,
                    allow: vec!["api.github.com".into()],
                },
                ..Default::default()
            },
        })
        .await
        .expect_err("a per-domain allowlist must be refused while no proxy exists");

    assert!(
        matches!(err, SandboxError::PolicyViolation(_)),
        "refused for the stated reason, not by accident: {err:?}"
    );
}
