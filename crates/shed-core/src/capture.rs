use bytes::Bytes;
use std::time::Instant;

use crate::value::Value;

/// A snapshot of a command's output.
///
/// Captures are produced by the binary crate's PTY-based exec module and
/// stored on a [`Shed`](crate::Shed). When the capture's bytes are fed
/// into a [`FilterSpec`](crate::FilterSpec) pipeline, parsers strip the
/// terminal escape sequences before structuring the data, so filters work
/// uniformly whether the capture came from a pipe or a PTY.
///
/// # Truncation
///
/// If a command's output exceeds the byte cap configured at spawn time, the
/// capture buffer stops accepting new bytes (`truncated = true`) but the
/// child process keeps running — the binary's reader drains the rest to
/// `/dev/null` so the child doesn't shed on a full pipe.
///
/// # PTY note
///
/// PTY captures merge `stderr` into `stdout`; the `stderr` field is empty
/// in that case. Pipe captures (no longer the default in v0) keep them
/// separate.
#[derive(Debug, Clone)]
pub struct Capture {
    pub stdout: Bytes,
    pub stderr: Bytes,
    /// Process exit code, or `None` if the wait failed.
    pub exit_code: Option<i32>,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    /// `true` if the capture buffer hit the byte cap and additional bytes
    /// were discarded.
    pub truncated: bool,
    /// `true` if the user manually froze a streaming capture (planned
    /// feature; always `false` in v0).
    pub snapshotted: bool,
    /// If set, this capture is a *structured snapshot* — typically taken
    /// when a shed referenced another shed via `@name` or `%N`. The
    /// pipeline applied to this shed starts from this value directly
    /// instead of re-parsing `stdout`, preserving column order and
    /// types across the boundary. PTY captures always have this as
    /// `None`; only snapshot sheds populate it.
    pub structured: Option<Value>,
}
