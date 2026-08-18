//! Spawning and streaming a jailed child — shared by every process-based tier.
//!
//! T2 macOS and T2 Linux differ only in *how* they build the command; what
//! happens afterwards (pump both pipes, enforce the wall clock, kill on
//! consumer drop) is identical. Keeping one copy is deliberate: the last time
//! this crate had two hand-rolled versions of a shared concern, the duplicate
//! was the untested one and it shipped a bug.

use crate::{ExecChunk, ExecStream, SandboxError};

/// Run `command`, streaming stdout/stderr until it exits or overruns.
///
/// Two behaviours matter beyond the obvious:
///
/// * **Wall clock** — a breach SIGKILLs the child and reports
///   `LimitExceeded`. docs/13 reserves the graceful SIGTERM path for
///   cancellation; a wall-clock breach is already a failure.
/// * **Kill on consumer drop** — if the receiver goes away the caller has
///   cancelled, so the child is killed rather than left running. Without
///   this, cancelling merely stops *listening* to work that carries on
///   burning CPU (docs/13 §cancellation).
pub fn spawn_and_stream(
    mut command: tokio::process::Command,
    wall_ms: u64,
) -> Result<ExecStream, SandboxError> {
    use tokio::io::AsyncReadExt;

    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|e| SandboxError::Internal(format!("spawn: {e}")))?;

    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<ExecChunk, SandboxError>>(64);

    tokio::spawn(async move {
        let began = std::time::Instant::now();
        let mut out_buf = [0u8; 8192];
        let mut err_buf = [0u8; 8192];
        let mut out_done = false;
        let mut err_done = false;

        let deadline = tokio::time::sleep(std::time::Duration::from_millis(wall_ms));
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                // Biased so buffered output drains before the deadline arm
                // can fire: a command finishing exactly at the limit would
                // otherwise lose its last chunk.
                biased;

                n = stdout.read(&mut out_buf), if !out_done => match n {
                    Ok(0) => out_done = true,
                    Ok(n) => {
                        if tx.send(Ok(ExecChunk::Stdout(out_buf[..n].to_vec()))).await.is_err() {
                            let _ = child.kill().await;
                            return;
                        }
                    }
                    Err(_) => out_done = true,
                },
                n = stderr.read(&mut err_buf), if !err_done => match n {
                    Ok(0) => err_done = true,
                    Ok(n) => {
                        if tx.send(Ok(ExecChunk::Stderr(err_buf[..n].to_vec()))).await.is_err() {
                            let _ = child.kill().await;
                            return;
                        }
                    }
                    Err(_) => err_done = true,
                },
                // A silent command would otherwise never notice the consumer
                // is gone.
                _ = tx.closed() => {
                    let _ = child.kill().await;
                    return;
                }
                _ = &mut deadline => {
                    let _ = child.kill().await;
                    let _ = tx.send(Err(SandboxError::LimitExceeded(format!(
                        "wall clock exceeded {wall_ms}ms"
                    )))).await;
                    return;
                }
                status = child.wait(), if out_done && err_done => {
                    let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
                    let _ = tx.send(Ok(ExecChunk::Exit {
                        code,
                        wall_ms: began.elapsed().as_millis() as u64,
                    })).await;
                    return;
                }
            }
        }
    });

    Ok(Box::pin(futures_util::stream::unfold(
        rx,
        |mut rx| async move { rx.recv().await.map(|item| (item, rx)) },
    )))
}
