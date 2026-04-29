use bytes::Bytes;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct Capture {
    pub stdout: Bytes,
    pub stderr: Bytes,
    pub exit_code: Option<i32>,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub truncated: bool,
    pub snapshotted: bool,
}
