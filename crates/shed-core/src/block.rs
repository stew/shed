use std::time::Instant;

use crate::capture::Capture;
use crate::filter::FilterSpec;

/// Monotonic per-session block id. Renders to the user as `%1`, `%2`, …
/// and is never reused — eviction does not shift ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u64);

/// Lifecycle state of the spawned process behind a block.
///
/// `Idle` blocks were loaded from a notebook but have not been run in this
/// session — they have no capture and the user must trigger them
/// explicitly. `Running` blocks have no [`Capture`] yet. `Done` carries the
/// exit code; `Failed` carries a human-readable reason (spawn error,
/// cancellation, task error, …). `Snapshotted` is reserved for a future
/// feature where the user freezes a streaming capture mid-flight.
#[derive(Debug, Clone)]
pub enum BlockState {
    Idle,
    Running,
    Snapshotted,
    Done(i32),
    Failed(String),
}

/// A single command and its retroactive pipeline.
///
/// Each typed command produces one block. The captured stdout is held in
/// [`Block::capture`]; over the block's life the user appends, edits, and
/// removes filters in [`Block::pipeline`], which the renderer re-applies
/// on every redraw.
///
/// `last_touched` drives LRU eviction in [`Session`](crate::Session) —
/// editing a filter, opening the block, or piping into a new pipeline
/// updates it. Scrolling past does NOT touch.
#[derive(Debug, Clone)]
pub struct Block {
    pub id: BlockId,
    /// Pinned name (UI-set). Pinned blocks count toward the capture budget
    /// but are never evicted.
    pub name: Option<String>,
    pub argv: Vec<String>,
    pub capture: Option<Capture>,
    pub pipeline: Vec<FilterSpec>,
    pub state: BlockState,
    pub last_touched: Instant,
    /// Free-form note rendered above the block. Persisted to notebooks.
    pub pre_text: Option<String>,
    /// Free-form note rendered below the block's content. Persisted to
    /// notebooks.
    pub post_text: Option<String>,
}

impl Block {
    /// Total byte size of the capture (stdout + stderr), or 0 if the
    /// capture has been evicted.
    pub fn capture_size_bytes(&self) -> usize {
        self.capture
            .as_ref()
            .map_or(0, |c| c.stdout.len() + c.stderr.len())
    }
}
