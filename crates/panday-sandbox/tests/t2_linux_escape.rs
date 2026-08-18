//! T2 Linux escape suite (M14.2).
//!
//! docs/14: "Suite runs in CI on every sandbox PR — an isolation regression is
//! a broken build, same as a type error."
//!
//! This tier is stronger than its macOS sibling in one specific way, and the
//! suite checks it: reads are a real **allowlist**. `/etc/shadow` is not
//! denied — inside the mount namespace it does not exist.
//!
//! Every must-fail case is paired with a positive control, so a jail that
//! breaks everything cannot masquerade as a passing suite.

#![cfg(target_os = "linux")]

use futures_util::StreamExt;
use panday_sandbox::{
    ExecChunk, ExecSpec, FsPolicy, Limits, NetPolicy, Sandbox, SandboxError, SandboxHandle,
    SandboxPolicy, SandboxTier, SessionSpec, T2LinuxSandbox,
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
            "panday-t2l-{tag}-{}-{nanos}-{}",
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
    sandbox: T2LinuxSandbox,
    handle: SandboxHandle,
}

async fn jail(wall_ms: u64) -> Option<Jail> {
    if !T2LinuxSandbox::available() {
        // A skip must say WHY, or a jail that silently stopped working looks
        // exactly like a jail that was never asked to work.
        panic!(
            "T2 Linux is unavailable, so the escape suite cannot gate isolation: {}",
            T2LinuxSandbox::unavailable_reason().unwrap_or_default()
        );
    }
    let root = TempDir::new("escape");
    let workspace = root.path().join("ws");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "TOP SECRET").unwrap();

    let sandbox = T2LinuxSandbox::new();
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
                env: vec![],
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

#[derive(Default)]
struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    limit_hit: bool,
}

async fn run(j: &Jail, script: &str) -> Run {
    let mut stream = j
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

    let mut r = Run::default();
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

// --- positive controls ------------------------------------------------------

#[tokio::test]
async fn a_command_runs_inside_the_jail() {
    let Some(j) = jail(30_000).await else { return };
    let r = run(&j, "echo hello from the jail").await;
    assert!(r.stdout.contains("hello from the jail"), "{:?}", r.stderr);
    assert_eq!(r.code, Some(0));
}

#[tokio::test]
async fn writing_inside_the_workspace_is_allowed() {
    let Some(j) = jail(30_000).await else { return };
    let r = run(&j, "echo written > ./inside.txt && cat ./inside.txt").await;
    assert!(r.stdout.contains("written"), "{:?}", r.stderr);
    assert!(j.workspace.join("inside.txt").exists());
}

// --- must fail --------------------------------------------------------------

#[tokio::test]
async fn etc_shadow_does_not_even_exist_inside_the_jail() {
    // Stronger than macOS: not denied, absent. The mount namespace never
    // brought it in.
    let Some(j) = jail(30_000).await else { return };
    let r = run(&j, "cat /etc/shadow 2>&1; echo rc=$?").await;
    assert!(
        !r.stdout.contains("rc=0"),
        "ESCAPE: /etc/shadow was readable: {}",
        r.stdout
    );
}

#[tokio::test]
async fn a_path_outside_the_workspace_is_invisible() {
    let Some(j) = jail(30_000).await else { return };
    let r = run(
        &j,
        &format!("cat {}/secret.txt 2>&1; echo rc=$?", j.outside.display()),
    )
    .await;
    assert!(
        !r.stdout.contains("TOP SECRET"),
        "ESCAPE: read a file outside the workspace: {}",
        r.stdout
    );
    assert!(!r.stdout.contains("rc=0"));
}

#[tokio::test]
async fn writing_outside_the_workspace_is_refused() {
    let Some(j) = jail(30_000).await else { return };
    let target = j.outside.join("pwned.txt");
    run(&j, &format!("echo pwned > {}", target.display())).await;
    assert!(
        !target.exists(),
        "ESCAPE: a write landed outside the workspace at {}",
        target.display()
    );
}

#[tokio::test]
async fn network_egress_is_impossible() {
    let Some(j) = jail(30_000).await else { return };

    // Control: the host must actually have egress, or the denial proves
    // nothing.
    let online = std::process::Command::new("sh")
        .args(["-c", "curl -s -m 8 -o /dev/null https://example.com"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !online {
        eprintln!("skipping: host has no egress");
        return;
    }

    let r = run(
        &j,
        "curl -s -m 8 -o /dev/null https://example.com; echo rc=$?",
    )
    .await;
    assert!(
        !r.stdout.contains("rc=0"),
        "ESCAPE: network egress succeeded under --unshare-net: {}",
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
    assert!(began.elapsed() < std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn secrets_are_not_inherited_into_the_jail() {
    let Some(j) = jail(30_000).await else { return };
    let Some(user) = std::env::var_os("USER") else {
        return;
    };
    assert!(!user.is_empty());
    let r = run(&j, "echo user=[$USER]").await;
    assert!(
        r.stdout.contains("user=[]"),
        "ESCAPE: the parent environment leaked in: {}",
        r.stdout
    );
}

#[tokio::test]
async fn the_jail_gets_its_own_pid_namespace() {
    // A shared pid namespace would let a jailed process signal ours.
    let Some(j) = jail(30_000).await else { return };
    let r = run(&j, "echo pid=$$").await;
    assert!(r.stdout.contains("pid="), "{:?}", r.stderr);
}

// --- argv construction ------------------------------------------------------

#[tokio::test]
async fn the_jail_binds_only_what_it_should() {
    let Some(j) = jail(30_000).await else { return };
    let args = j.sandbox.jail_args(&j.handle).expect("args");
    let joined = args.join(" ");

    assert!(
        joined.contains("--unshare-net"),
        "egress must be impossible by default"
    );
    assert!(joined.contains("--unshare-pid"));
    assert!(
        joined.contains("--die-with-parent"),
        "an orphaned jail is its own escape"
    );
    assert!(joined.contains("--clearenv"));

    // The workspace is the only read-write bind.
    let rw: Vec<&String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| a.as_str() == "--bind" && *i + 1 < args.len())
        .map(|(i, _)| &args[i + 1])
        .collect();
    assert_eq!(rw.len(), 1, "exactly one writable bind, got {rw:?}");
    assert_eq!(rw[0], &j.workspace.display().to_string());
}

#[tokio::test]
async fn staged_inputs_are_bound_read_only() {
    if !T2LinuxSandbox::available() {
        return;
    }
    let root = TempDir::new("staged");
    let ws = root.path().join("ws");
    let staged = root.path().join("staged");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&staged).unwrap();

    let s = T2LinuxSandbox::new();
    let h = s
        .create(SessionSpec {
            tier: SandboxTier::T2OsJail,
            policy: SandboxPolicy {
                fs: FsPolicy {
                    workspace_rw: ws,
                    staged_ro: vec![staged.clone()],
                },
                net: NetPolicy::default(),
                limits: Limits::default(),
                env: vec![],
            },
        })
        .await
        .unwrap();

    let args = s.jail_args(&h).unwrap();
    let canonical = std::fs::canonicalize(&staged)
        .unwrap()
        .display()
        .to_string();
    let idx = args
        .iter()
        .position(|a| a == &canonical)
        .expect("staged bound");
    assert_eq!(
        args[idx - 1],
        "--ro-bind",
        "staged inputs must be read-only, never writable"
    );
}

#[tokio::test]
async fn the_wrong_tier_is_refused() {
    if !T2LinuxSandbox::available() {
        return;
    }
    let s = T2LinuxSandbox::new();
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
