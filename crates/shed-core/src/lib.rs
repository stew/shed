//! Core data model and filter execution for shed.
//!
//! shed is an interactive shell that captures every command's output as a
//! [`Block`] holding a [`Capture`] of stdout/stderr, then lets the user build
//! a [`FilterSpec`] pipeline retroactively. This crate is pure data: no I/O,
//! no UI dependencies. The binary crate (`shed`) handles process spawning
//! (via PTY), terminal rendering (via ratatui), and the event loop.
//!
//! # Pipeline shape
//!
//! Captured stdout starts as raw [`PipelineValue::Bytes`]. The first filter
//! in any pipeline is a *parser* (`from-lines`, `from-fields`, `from-csv`,
//! `from-json`, `from-regex`) that converts bytes to
//! [`PipelineValue::Structured`] — typically a [`Value::List`] of
//! [`Value::Record`]s. Downstream filters operate on the structured form.
//!
//! Apply a single filter with [`Filter::apply`]; apply with diagnostic stats
//! (currently: rows silently dropped by `where` due to type mismatch) using
//! [`apply_with_notes`].
//!
//! # Session and eviction
//!
//! A [`Session`] holds blocks in id order and enforces an LRU eviction
//! policy: when the total bytes of unnamed (unpinned) captures exceed
//! [`DEFAULT_CAPTURE_BUDGET_BYTES`], the oldest-touched unpinned captures are
//! dropped (their [`Block::capture`] becomes `None`). Pinned captures count
//! toward the budget but are never evicted.

pub mod aliases;
pub mod block;
pub mod capture;
pub mod filter;
pub mod notebook;
pub mod session;
pub mod value;

pub use aliases::{ALIASES_VERSION, Alias, AliasError, AliasFile};
pub use block::{Block, BlockId, BlockState};
pub use capture::Capture;
pub use filter::{
    CompareOp, Filter, FilterError, FilterNotes, FilterSpec, PipelineValue, Predicate,
    SortDirection, SortKey, apply_with_notes,
};
pub use notebook::{NOTEBOOK_VERSION, Notebook, NotebookEntry, NotebookError};
pub use session::{DEFAULT_CAPTURE_BUDGET_BYTES, Session};
pub use value::Value;
