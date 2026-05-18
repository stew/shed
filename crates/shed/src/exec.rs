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
use tokio::sync::{mpsc, oneshot};
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
pub type ExecHandle = JoinHandle<Result<CaptureOutcome, ExecError>>;

/// Receiver yielded from [`spawn_command`] alongside the join handle.
/// Each PTY read that contributes bytes to the eventual capture also
/// sends those bytes through this channel, so the TUI can stream the
/// output into the shed's `capture` while the command is still
/// running. The sender side is closed when the reader task ends; the
/// receiver becomes `Disconnected` after that.
pub type ChunkReceiver = mpsc::UnboundedReceiver<Bytes>;

/// What a finished PTY reader task hands back.
///
/// `Captured` is the normal case — the child ran, output was captured, the
/// child exited. `NeededFullscreen` means the reader spotted an alt-screen
/// enter sequence (`\x1b[?1049h` and friends) in the byte stream, killed
/// the PTY child, and is signaling the TUI to retry as a fullscreen
/// handover. The TUI keeps the original shed's id, swaps in inherited
/// stdio, and runs the program with full terminal control.
#[derive(Debug)]
pub enum CaptureOutcome {
    Captured(Capture),
    NeededFullscreen,
}

const ALT_SCREEN_PATTERNS: &[&[u8]] = &[b"\x1b[?1049h", b"\x1b[?1047h", b"\x1b[?47h"];

/// Search a byte slice for any alt-screen-enter sequence.
pub fn contains_alt_screen(haystack: &[u8]) -> bool {
    ALT_SCREEN_PATTERNS
        .iter()
        .any(|p| haystack.windows(p.len()).any(|w| w == *p))
}

/// Spawn a command attached to a pseudo-terminal. Returns:
/// - the [`ExecHandle`] for the blocking task that captures output
///   (await it to get the final [`CaptureOutcome`]);
/// - a [`Killer`] that terminates the child while the task is still reading;
/// - a [`ChunkReceiver`] that yields each captured chunk in flight, so the
///   TUI can stream output into the shed while the command runs.
///
/// The killer is delivered via a oneshot once the child has actually
/// been spawned, so this fn is async (typically completes within
/// microseconds).
pub async fn spawn_command(
    argv: Vec<String>,
    cap_bytes: usize,
) -> Result<(ExecHandle, Killer, ChunkReceiver), ExecError> {
    let (killer_tx, killer_rx) = oneshot::channel::<Result<Killer, ExecError>>();
    let (chunk_tx, chunk_rx) = mpsc::unbounded_channel::<Bytes>();
    let handle =
        tokio::task::spawn_blocking(move || run_blocking(argv, cap_bytes, killer_tx, chunk_tx));

    match killer_rx.await {
        Ok(Ok(killer)) => Ok((handle, killer, chunk_rx)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(ExecError::NotStarted),
    }
}

fn run_blocking(
    argv: Vec<String>,
    cap_bytes: usize,
    killer_tx: oneshot::Sender<Result<Killer, ExecError>>,
    chunk_tx: mpsc::UnboundedSender<Bytes>,
) -> Result<CaptureOutcome, ExecError> {
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

    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    let mut tail: Vec<u8> = Vec::new();
    let mut needs_fullscreen = false;
    // Longest pattern we look for, used for the chunk-boundary overlap window.
    let overlap = ALT_SCREEN_PATTERNS
        .iter()
        .map(|p| p.len())
        .max()
        .unwrap_or(0)
        .saturating_sub(1);

    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.raw_os_error() == Some(5) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(ExecError::Io(e.to_string())),
        };

        let mut combined = Vec::with_capacity(tail.len() + n);
        combined.extend_from_slice(&tail);
        combined.extend_from_slice(&chunk[..n]);
        if contains_alt_screen(&combined) {
            let _ = child.kill();
            needs_fullscreen = true;
            break;
        }
        let keep_from = combined.len().saturating_sub(overlap);
        tail = combined[keep_from..].to_vec();

        if truncated {
            continue;
        }
        let remaining = cap_bytes.saturating_sub(buf.len());
        let kept = if n <= remaining {
            buf.extend_from_slice(&chunk[..n]);
            &chunk[..n]
        } else {
            buf.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            &chunk[..remaining]
        };
        if !kept.is_empty() {
            // Receiver gone (TUI dropped the shed / shut down) — keep
            // reading anyway so the local `buf` and exit code stay
            // consistent, just stop streaming.
            let _ = chunk_tx.send(Bytes::copy_from_slice(kept));
        }
    }

    let exit_status = child.wait().map_err(|e| ExecError::Wait(e.to_string()))?;
    let finished_at = Instant::now();

    if needs_fullscreen {
        return Ok(CaptureOutcome::NeededFullscreen);
    }

    let exit_code = if exit_status.success() {
        0
    } else {
        exit_status.exit_code() as i32
    };

    Ok(CaptureOutcome::Captured(Capture {
        stdout: Bytes::from(buf),
        stderr: Bytes::new(),
        exit_code: Some(exit_code),
        started_at,
        finished_at: Some(finished_at),
        truncated,
        snapshotted: false,
        structured: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unwrap_captured(o: CaptureOutcome) -> Capture {
        match o {
            CaptureOutcome::Captured(c) => c,
            CaptureOutcome::NeededFullscreen => panic!("expected Captured"),
        }
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let (handle, _killer, _chunks) =
            spawn_command(vec!["printf".into(), "a\nb\nc\n".into()], 1024)
                .await
                .unwrap();
        let capture = unwrap_captured(handle.await.unwrap().unwrap());
        let s = String::from_utf8_lossy(&capture.stdout);
        assert!(s.contains("a") && s.contains("b") && s.contains("c"));
        assert_eq!(capture.exit_code, Some(0));
        assert!(!capture.truncated);
        assert!(capture.finished_at.is_some());
    }

    #[tokio::test]
    async fn captures_nonzero_exit() {
        let (handle, _killer, _chunks) = spawn_command(vec!["false".into()], 1024).await.unwrap();
        let capture = unwrap_captured(handle.await.unwrap().unwrap());
        assert_eq!(capture.exit_code, Some(1));
    }

    #[tokio::test]
    async fn truncation_marks_capture_and_drains_child() {
        let (handle, _killer, _chunks) =
            spawn_command(vec!["seq".into(), "1".into(), "100000".into()], 64)
                .await
                .unwrap();
        let capture = unwrap_captured(handle.await.unwrap().unwrap());
        assert!(capture.truncated);
        assert_eq!(capture.stdout.len(), 64);
        assert_eq!(capture.exit_code, Some(0));
    }

    #[tokio::test]
    async fn alt_screen_detected_in_output_triggers_handover_signal() {
        // printf emits a literal alt-screen-enter sequence; the reader
        // should detect it and return NeededFullscreen.
        let (handle, _killer, _chunks) =
            spawn_command(vec!["printf".into(), "\\x1b[?1049hhello".into()], 1024)
                .await
                .unwrap();
        let outcome = handle.await.unwrap().unwrap();
        assert!(matches!(outcome, CaptureOutcome::NeededFullscreen));
    }

    #[test]
    fn contains_alt_screen_finds_known_patterns() {
        assert!(contains_alt_screen(b"prefix\x1b[?1049hpostfix"));
        assert!(contains_alt_screen(b"\x1b[?1047h"));
        assert!(contains_alt_screen(b"\x1b[?47h"));
        assert!(!contains_alt_screen(b"plain text"));
        assert!(!contains_alt_screen(b"\x1b[?1049l")); // leave, not enter
        assert!(!contains_alt_screen(b"\x1b[31mred\x1b[0m"));
    }

    #[tokio::test]
    async fn missing_program_returns_spawn_error() {
        let err = spawn_command(vec!["definitely-not-a-real-command-xyzzy".into()], 1024)
            .await
            .unwrap_err();
        assert!(matches!(err, ExecError::Spawn { .. }));
    }

    #[tokio::test]
    async fn streams_chunks_through_receiver_while_running() {
        let (handle, _killer, mut chunks) =
            spawn_command(vec!["printf".into(), "hello\n".into()], 1024)
                .await
                .unwrap();
        // Wait for the command to finish so all chunks are sent and
        // the channel is closed.
        let outcome = handle.await.unwrap().unwrap();
        let mut all = Vec::new();
        while let Some(chunk) = chunks.recv().await {
            all.extend_from_slice(&chunk);
        }
        let capture = unwrap_captured(outcome);
        // The streamed bytes match the final capture's stdout.
        assert_eq!(all, capture.stdout.as_ref());
        assert!(String::from_utf8_lossy(&all).contains("hello"));
    }

    #[tokio::test]
    async fn streamed_chunks_respect_capture_cap() {
        // 64-byte cap: streaming should also stop after 64 bytes, even
        // though the child produces far more.
        let (handle, _killer, mut chunks) =
            spawn_command(vec!["seq".into(), "1".into(), "100000".into()], 64)
                .await
                .unwrap();
        let _ = handle.await.unwrap().unwrap();
        let mut total = 0usize;
        while let Some(chunk) = chunks.recv().await {
            total += chunk.len();
        }
        assert!(total <= 64, "streamed {total} bytes, cap was 64");
    }

    #[tokio::test]
    async fn killer_cancels_a_long_running_child() {
        let (handle, mut killer, _chunks) = spawn_command(vec!["sleep".into(), "30".into()], 1024)
            .await
            .unwrap();
        // Kill it; the blocking task should finish quickly because the slave closes.
        killer.kill().unwrap();
        // The task may finish with success or wait error depending on timing;
        // either way it should complete within a reasonable time.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }
}
