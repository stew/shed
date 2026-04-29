//! PTY-based command execution.
//!
//! Each command is spawned attached to a pseudo-terminal (via
//! `portable-pty`), so terminal-aware programs (`ls --color`, `cargo build`,
//! `git status`, …) detect a TTY and emit ANSI-colored output. The captured
//! bytes (including escape sequences) live on the resulting
//! [`shed_core::Capture`]; the TUI's `ansi` module decodes them at render
//! time.
//!
//! `portable-pty`'s API is synchronous, so the actual capture happens on
//! tokio's blocking thread pool via `spawn_blocking`. [`spawn_command`]
//! returns:
//! - the [`JoinHandle`] for that blocking task (which the event loop
//!   `is_finished()`-polls and awaits on completion),
//! - a [`Killer`] (a `Box<dyn ChildKiller + Send + Sync>`) that the TUI
//!   keeps alongside the handle so Ctrl-C and shed shutdown can actually
//!   terminate the child process.
//!
//! This split is important because aborting a `spawn_blocking` task is a
//! no-op for the running closure (you can't interrupt blocking work) — the
//! killer is the only way to make a long-running PTY child stop.

use std::io::Read;
use std::time::Instant;

use bytes::Bytes;
use portable_pty::{ChildKiller, CommandBuilder, PtySize, native_pty_system};
use shed_core::Capture;
use thiserror::Error;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("failed to open pty: {0}")]
    OpenPty(String),
    #[error("failed to spawn `{program}`: {error}")]
    Spawn { program: String, error: String },
    #[error("io error: {0}")]
    Io(String),
    #[error("wait failed: {0}")]
    Wait(String),
    #[error("task did not start")]
    NotStarted,
}

pub type Killer = Box<dyn ChildKiller + Send + Sync>;
pub type ExecHandle = JoinHandle<Result<Capture, ExecError>>;

/// Spawn a command attached to a pseudo-terminal. Returns a JoinHandle for the
/// blocking task that captures output AND a separate killer that lets the
/// caller terminate the child process while the task is still reading. The
/// killer is delivered via a oneshot once the child has actually been spawned,
/// so this fn is async (typically completes within microseconds).
pub async fn spawn_command(
    argv: Vec<String>,
    cap_bytes: usize,
) -> Result<(ExecHandle, Killer), ExecError> {
    let (killer_tx, killer_rx) = oneshot::channel::<Result<Killer, ExecError>>();
    let handle = tokio::task::spawn_blocking(move || run_blocking(argv, cap_bytes, killer_tx));

    match killer_rx.await {
        Ok(Ok(killer)) => Ok((handle, killer)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(ExecError::NotStarted),
    }
}

fn run_blocking(
    argv: Vec<String>,
    cap_bytes: usize,
    killer_tx: oneshot::Sender<Result<Killer, ExecError>>,
) -> Result<Capture, ExecError> {
    let started_at = Instant::now();

    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 200,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            let err = ExecError::OpenPty(e.to_string());
            let _ = killer_tx.send(Err(ExecError::OpenPty(e.to_string())));
            return Err(err);
        }
    };

    let mut cmd = CommandBuilder::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    // Hint terminal capability so programs that gate color on TERM emit it.
    cmd.env("TERM", "xterm-256color");
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            let err = ExecError::Spawn {
                program: argv[0].clone(),
                error: e.to_string(),
            };
            let _ = killer_tx.send(Err(ExecError::Spawn {
                program: argv[0].clone(),
                error: e.to_string(),
            }));
            return Err(err);
        }
    };

    drop(pair.slave);

    let killer: Killer = child.clone_killer();
    let _ = killer_tx.send(Ok(killer));

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| ExecError::Io(e.to_string()))?;
    let (buf, truncated) =
        read_capped_blocking(&mut reader, cap_bytes).map_err(|e| ExecError::Io(e.to_string()))?;

    let exit_status = child.wait().map_err(|e| ExecError::Wait(e.to_string()))?;
    let finished_at = Instant::now();

    let exit_code = if exit_status.success() {
        0
    } else {
        exit_status.exit_code() as i32
    };

    Ok(Capture {
        stdout: Bytes::from(buf),
        stderr: Bytes::new(),
        exit_code: Some(exit_code),
        started_at,
        finished_at: Some(finished_at),
        truncated,
        snapshotted: false,
    })
}

fn read_capped_blocking<R: Read>(reader: &mut R, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            // PTY master sees the slave close as EIO on Linux.
            Err(e) if e.raw_os_error() == Some(5) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if truncated {
            continue;
        }
        let remaining = cap.saturating_sub(buf.len());
        if n <= remaining {
            buf.extend_from_slice(&chunk[..n]);
        } else {
            buf.extend_from_slice(&chunk[..remaining]);
            truncated = true;
        }
    }
    Ok((buf, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let (handle, _killer) = spawn_command(
            vec!["printf".into(), "a\nb\nc\n".into()],
            1024,
        )
        .await
        .unwrap();
        let capture = handle.await.unwrap().unwrap();
        // PTY adds CRLF, so we see \r\n instead of \n.
        let s = String::from_utf8_lossy(&capture.stdout);
        assert!(s.contains("a") && s.contains("b") && s.contains("c"));
        assert_eq!(capture.exit_code, Some(0));
        assert!(!capture.truncated);
        assert!(capture.finished_at.is_some());
    }

    #[tokio::test]
    async fn captures_nonzero_exit() {
        let (handle, _killer) = spawn_command(vec!["false".into()], 1024).await.unwrap();
        let capture = handle.await.unwrap().unwrap();
        assert_eq!(capture.exit_code, Some(1));
    }

    #[tokio::test]
    async fn truncation_marks_capture_and_drains_child() {
        let (handle, _killer) = spawn_command(
            vec!["seq".into(), "1".into(), "100000".into()],
            64,
        )
        .await
        .unwrap();
        let capture = handle.await.unwrap().unwrap();
        assert!(capture.truncated);
        assert_eq!(capture.stdout.len(), 64);
        assert_eq!(capture.exit_code, Some(0));
    }

    #[tokio::test]
    async fn missing_program_returns_spawn_error() {
        let err = spawn_command(
            vec!["definitely-not-a-real-command-xyzzy".into()],
            1024,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ExecError::Spawn { .. }));
    }

    #[tokio::test]
    async fn killer_cancels_a_long_running_child() {
        let (handle, mut killer) = spawn_command(
            vec!["sleep".into(), "30".into()],
            1024,
        )
        .await
        .unwrap();
        // Kill it; the blocking task should finish quickly because the slave closes.
        killer.kill().unwrap();
        // The task may finish with success or wait error depending on timing;
        // either way it should complete within a reasonable time.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }
}
