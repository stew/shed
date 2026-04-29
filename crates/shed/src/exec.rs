use bytes::Bytes;
use shed_core::Capture;
use std::time::Instant;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read child output: {0}")]
    Read(#[source] std::io::Error),
    #[error("failed to wait for child: {0}")]
    Wait(#[source] std::io::Error),
}

pub async fn run_command(argv: &[String], cap_bytes: usize) -> Result<Capture, ExecError> {
    let started_at = Instant::now();

    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| ExecError::Spawn {
            program: argv[0].clone(),
            source,
        })?;

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    let (stdout_res, stderr_res) = tokio::join!(
        read_capped(stdout, cap_bytes),
        read_capped(stderr, cap_bytes),
    );
    let (stdout_buf, stdout_trunc) = stdout_res.map_err(ExecError::Read)?;
    let (stderr_buf, stderr_trunc) = stderr_res.map_err(ExecError::Read)?;

    let status = child.wait().await.map_err(ExecError::Wait)?;
    let finished_at = Instant::now();

    Ok(Capture {
        stdout: Bytes::from(stdout_buf),
        stderr: Bytes::from(stderr_buf),
        exit_code: status.code(),
        started_at,
        finished_at: Some(finished_at),
        truncated: stdout_trunc || stderr_trunc,
        snapshotted: false,
    })
}

// Reads to EOF. Once `cap` bytes have been buffered, further bytes are drained
// and discarded so the child can keep running without blocking on a full pipe.
async fn read_capped<R>(mut reader: R, cap: usize) -> Result<(Vec<u8>, bool), std::io::Error>
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
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
        let argv = vec!["printf".into(), "a\nb\nc\n".into()];
        let capture = run_command(&argv, 1024).await.unwrap();
        assert_eq!(&capture.stdout[..], b"a\nb\nc\n");
        assert_eq!(capture.exit_code, Some(0));
        assert!(!capture.truncated);
        assert!(capture.finished_at.is_some());
    }

    #[tokio::test]
    async fn captures_nonzero_exit() {
        let argv = vec!["false".into()];
        let capture = run_command(&argv, 1024).await.unwrap();
        assert_eq!(capture.exit_code, Some(1));
    }

    #[tokio::test]
    async fn truncation_marks_capture_and_drains_child() {
        let argv = vec!["seq".into(), "1".into(), "100000".into()];
        let capture = run_command(&argv, 64).await.unwrap();
        assert!(capture.truncated);
        assert_eq!(capture.stdout.len(), 64);
        assert_eq!(capture.exit_code, Some(0));
    }

    #[tokio::test]
    async fn missing_program_returns_spawn_error() {
        let argv = vec!["definitely-not-a-real-command-xyzzy".into()];
        let err = run_command(&argv, 1024).await.unwrap_err();
        assert!(matches!(err, ExecError::Spawn { .. }));
    }
}
