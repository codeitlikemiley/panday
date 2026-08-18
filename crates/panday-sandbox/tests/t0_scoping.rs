//! T0 FS scoping (M14.1) — the escape suite for the in-process tier.
//!
//! docs/14 §the escape suite lists must-fail cases per tier. T0 has no
//! process, so the exec/network/pids cases do not apply; what remains is the
//! whole of T0's isolation: **no path may resolve outside the workspace**.
//! Every test below is an attempt to break that.

use panday_sandbox::{
    Access, ExecSpec, FsPolicy, Limits, Sandbox, SandboxError, SandboxHandle, SandboxPolicy,
    SandboxTier, SessionSpec, T0Sandbox,
};
use std::path::{Path, PathBuf};

/// A self-cleaning temp directory.
///
/// Hand-rolled rather than pulling in `tempfile`/`tempdir`: neither is in the
/// docs/02 dependency table, and a test helper is not worth widening it.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "panday-{tag}-{}-{}-{}",
            std::process::id(),
            nanos,
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
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

/// A workspace with a staged read-only input beside it, plus a secret file
/// outside both that nothing may reach.
struct Fixture {
    _root: TempDir,
    workspace: PathBuf,
    staged: PathBuf,
    outside_secret: PathBuf,
    sandbox: T0Sandbox,
    handle: SandboxHandle,
}

async fn fixture() -> Fixture {
    let root = TempDir::new("t0");
    let base = root.path();

    let workspace = base.join("workspace");
    let staged = base.join("staged");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(workspace.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(staged.join("input.txt"), "staged input").unwrap();

    // Stands in for /etc/shadow: outside every allowed root.
    let outside_secret = base.join("secret.txt");
    std::fs::write(&outside_secret, "TOP SECRET").unwrap();

    let sandbox = T0Sandbox::new();
    let handle = sandbox
        .create(SessionSpec {
            tier: SandboxTier::T0InProcess,
            policy: SandboxPolicy {
                fs: FsPolicy {
                    workspace_rw: workspace.clone(),
                    staged_ro: vec![staged.clone()],
                },
                limits: Limits::default(),
                ..Default::default()
            },
        })
        .await
        .expect("create");

    Fixture {
        // Canonicalised so comparisons hold on macOS, where /tmp is a symlink.
        workspace: std::fs::canonicalize(&workspace).unwrap(),
        staged: std::fs::canonicalize(&staged).unwrap(),
        outside_secret: std::fs::canonicalize(&outside_secret).unwrap(),
        _root: root,
        sandbox,
        handle,
    }
}

fn is_denied(e: &SandboxError) -> bool {
    matches!(e, SandboxError::PolicyViolation(_))
}

// ---------------------------------------------------------------------------
// The allowed cases — a scoping check that denies everything is not a
// sandbox, it is a brick.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reads_and_writes_inside_the_workspace_are_allowed() {
    let f = fixture().await;

    let got = f
        .sandbox
        .get(&f.handle, PathBuf::from("main.rs"))
        .await
        .expect("relative read inside the workspace");
    assert_eq!(got, b"fn main() {}");

    f.sandbox
        .put(&f.handle, PathBuf::from("new/nested.txt"), b"hi".to_vec())
        .await
        .expect("write to a new nested path inside the workspace");
    assert_eq!(
        std::fs::read(f.workspace.join("new/nested.txt")).unwrap(),
        b"hi"
    );
}

#[tokio::test]
async fn staged_inputs_are_readable() {
    let f = fixture().await;
    let got = f
        .sandbox
        .get(&f.handle, f.staged.join("input.txt"))
        .await
        .expect("staged inputs are readable");
    assert_eq!(got, b"staged input");
}

#[tokio::test]
async fn an_absolute_path_inside_the_workspace_is_allowed() {
    let f = fixture().await;
    assert!(f
        .sandbox
        .get(&f.handle, f.workspace.join("main.rs"))
        .await
        .is_ok());
}

// ---------------------------------------------------------------------------
// Must-fail: the escape attempts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn staged_inputs_are_read_only() {
    // docs/14: staged inputs are mounted RO. Readable is not writable.
    let f = fixture().await;
    let err = f
        .sandbox
        .put(&f.handle, f.staged.join("input.txt"), b"tampered".to_vec())
        .await
        .expect_err("writing a staged input must be refused");
    assert!(is_denied(&err), "{err}");
    assert_eq!(
        std::fs::read(f.staged.join("input.txt")).unwrap(),
        b"staged input"
    );
}

#[tokio::test]
async fn dot_dot_traversal_out_of_the_workspace_is_refused() {
    let f = fixture().await;
    for attempt in [
        "../secret.txt",
        "../../secret.txt",
        "subdir/../../secret.txt",
        "./../secret.txt",
    ] {
        let err = f
            .sandbox
            .get(&f.handle, PathBuf::from(attempt))
            .await
            .unwrap_err();
        assert!(is_denied(&err), "{attempt}: {err}");
    }
}

#[tokio::test]
async fn an_absolute_path_outside_the_workspace_is_refused() {
    let f = fixture().await;
    let err = f
        .sandbox
        .get(&f.handle, f.outside_secret.clone())
        .await
        .expect_err("absolute escape must be refused");
    assert!(is_denied(&err), "{err}");
}

#[tokio::test]
async fn reading_etc_shadow_is_refused() {
    // The canonical case from docs/14's escape suite.
    let f = fixture().await;
    let err = f
        .sandbox
        .get(&f.handle, PathBuf::from("/etc/shadow"))
        .await
        .expect_err("/etc/shadow must never be readable");
    assert!(
        matches!(err, SandboxError::PolicyViolation(_)),
        "must be refused by POLICY, not merely because the OS happened to deny it: {err}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_pointing_out_of_the_workspace_does_not_smuggle_access() {
    // The classic bypass: the path is textually inside the workspace, but the
    // link resolves outside. Canonicalising before the check is what catches it.
    let f = fixture().await;
    std::os::unix::fs::symlink(&f.outside_secret, f.workspace.join("escape_hatch")).unwrap();

    let err = f
        .sandbox
        .get(&f.handle, PathBuf::from("escape_hatch"))
        .await
        .expect_err("a symlink out of the workspace must be refused");
    assert!(is_denied(&err), "{err}");
    // Prove the test was meaningful: the target really was readable.
    assert_eq!(std::fs::read(&f.outside_secret).unwrap(), b"TOP SECRET");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_directory_does_not_smuggle_writes() {
    let f = fixture().await;
    let outside_dir = f.outside_secret.parent().unwrap().join("outside_dir");
    std::fs::create_dir_all(&outside_dir).unwrap();
    std::os::unix::fs::symlink(&outside_dir, f.workspace.join("linked")).unwrap();

    let err = f
        .sandbox
        .put(&f.handle, PathBuf::from("linked/pwned.txt"), b"x".to_vec())
        .await
        .expect_err("writing through a directory symlink must be refused");
    assert!(is_denied(&err), "{err}");
    assert!(
        !outside_dir.join("pwned.txt").exists(),
        "the write actually landed outside the workspace"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_that_stays_inside_the_workspace_still_works() {
    // Guard against over-blocking: symlinks are not the enemy, escaping is.
    let f = fixture().await;
    std::os::unix::fs::symlink(f.workspace.join("main.rs"), f.workspace.join("alias.rs")).unwrap();

    let got = f
        .sandbox
        .get(&f.handle, PathBuf::from("alias.rs"))
        .await
        .expect("an internal symlink must remain usable");
    assert_eq!(got, b"fn main() {}");
}

// ---------------------------------------------------------------------------
// Tier semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t0_cannot_exec_anything() {
    // The tier is defined by having no process to run in (docs/14). A tool
    // needing exec must declare T2+.
    let f = fixture().await;
    let err = f
        .sandbox
        .exec(
            &f.handle,
            ExecSpec {
                cmd: vec!["/bin/sh".into(), "-c".into(), "echo pwned".into()],
                cwd: None,
                pty: false,
                stdin: None,
            },
        )
        .await
        // ExecStream is not Debug; drop the Ok side before unwrapping.
        .map(|_| ())
        .expect_err("T0 must not execute");
    assert!(matches!(
        err,
        SandboxError::Unsupported(SandboxTier::T0InProcess)
    ));
}

#[tokio::test]
async fn snapshot_is_t3_only() {
    let f = fixture().await;
    assert!(matches!(
        f.sandbox.snapshot(&f.handle).await.map(|_| ()),
        Err(SandboxError::Unsupported(_))
    ));
}

#[tokio::test]
async fn creating_with_a_nonexistent_workspace_fails_loudly() {
    // Silently creating it would hide a typo'd path until something wrote
    // into the wrong place.
    let s = T0Sandbox::new();
    let err = s
        .create(SessionSpec {
            tier: SandboxTier::T0InProcess,
            policy: SandboxPolicy {
                fs: FsPolicy {
                    workspace_rw: PathBuf::from("/definitely/not/here"),
                    staged_ro: vec![],
                },
                ..Default::default()
            },
        })
        .await
        .expect_err("must refuse a workspace that does not exist");
    assert!(matches!(err, SandboxError::PolicyViolation(_)), "{err}");
}

#[tokio::test]
async fn the_wrong_tier_is_refused_rather_than_silently_downgraded() {
    let s = T0Sandbox::new();
    let err = s
        .create(SessionSpec {
            tier: SandboxTier::T2OsJail,
            policy: SandboxPolicy::default(),
        })
        .await
        .expect_err("T0 must not pretend to be T2");
    assert!(matches!(
        err,
        SandboxError::Unsupported(SandboxTier::T2OsJail)
    ));
}

#[tokio::test]
async fn a_write_over_the_disk_limit_is_refused() {
    let root = TempDir::new("t0-limit");
    let ws = root.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let s = T0Sandbox::new();
    let h = s
        .create(SessionSpec {
            tier: SandboxTier::T0InProcess,
            policy: SandboxPolicy {
                fs: FsPolicy {
                    workspace_rw: ws,
                    staged_ro: vec![],
                },
                limits: Limits {
                    disk_bytes: 8,
                    ..Default::default()
                },
                ..Default::default()
            },
        })
        .await
        .unwrap();

    let err = s
        .put(&h, PathBuf::from("big.bin"), vec![0u8; 1024])
        .await
        .expect_err("must respect the disk limit");
    assert!(matches!(err, SandboxError::LimitExceeded(_)), "{err}");
}

#[tokio::test]
async fn resolve_reports_read_and_write_roots_differently() {
    let f = fixture().await;
    // Same path, different verdicts — the asymmetry is the point.
    let staged_file = f.staged.join("input.txt");
    assert!(f
        .sandbox
        .resolve(&f.handle, &staged_file, Access::Read)
        .is_ok());
    assert!(f
        .sandbox
        .resolve(&f.handle, &staged_file, Access::Write)
        .is_err());
}

#[tokio::test]
async fn destroy_forgets_the_session_without_deleting_the_users_files() {
    let f = fixture().await;
    let workspace = f.workspace.clone();
    let handle = f.handle.clone();

    f.sandbox.destroy(handle.clone()).await.unwrap();

    assert!(
        workspace.join("main.rs").exists(),
        "destroy must never delete the user's own workspace at this tier"
    );
    assert!(
        f.sandbox
            .get(&handle, PathBuf::from("main.rs"))
            .await
            .is_err(),
        "a destroyed session must no longer resolve paths"
    );
}
