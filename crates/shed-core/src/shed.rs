use std::time::Instant;

use crate::capture::Capture;
use crate::filter::FilterSpec;

/// Monotonic per-session shed id. Renders to the user as `%1`, `%2`, …
/// and is never reused — eviction does not shift ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShedId(pub u64);

/// Lifecycle state of the spawned process behind a shed.
///
/// `Idle` sheds were loaded from a notebook but have not been run in this
/// session — they have no capture and the user must trigger them
/// explicitly. `Running` sheds have no [`Capture`] yet. `Done` carries the
/// exit code; `Failed` carries a human-readable reason (spawn error,
/// cancellation, task error, …). `Snapshotted` is reserved for a future
/// feature where the user freezes a streaming capture mid-flight.
#[derive(Debug, Clone)]
pub enum ShedState {
    Idle,
    Running,
    Snapshotted,
    Done(i32),
    Failed(String),
}

/// A single command and its retroactive pipeline.
///
/// Each typed command produces one shed. The captured stdout is held in
/// [`Shed::capture`]; over the shed's life the user appends, edits, and
/// removes filters in [`Shed::pipeline`], which the renderer re-applies
/// on every redraw.
///
/// `last_touched` drives LRU eviction in [`Session`](crate::Session) —
/// editing a filter, opening the shed, or piping into a new pipeline
/// updates it. Scrolling past does NOT touch.
#[derive(Debug, Clone)]
pub struct Shed {
    pub id: ShedId,
    /// Pinned name (UI-set). Pinned sheds count toward the capture budget
    /// but are never evicted.
    pub name: Option<String>,
    pub argv: Vec<String>,
    pub capture: Option<Capture>,
    pub pipeline: Vec<FilterSpec>,
    pub state: ShedState,
    pub last_touched: Instant,
    /// Free-form note rendered above the shed. Persisted to notebooks.
    pub pre_text: Option<String>,
    /// Free-form note rendered below the shed's content. Persisted to
    /// notebooks.
    pub post_text: Option<String>,
}

impl Shed {
    /// Total byte size of the capture (stdout + stderr), or 0 if the
    /// capture has been evicted.
    pub fn capture_size_bytes(&self) -> usize {
        self.capture
            .as_ref()
            .map_or(0, |c| c.stdout.len() + c.stderr.len())
    }
}
