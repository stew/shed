//! shed — interactive shell with retroactive pipelines.
//!
//! The binary entry point is intentionally thin: it spins up the tokio
//! runtime, hands off to [`tui::run`], and translates the result into a
//! process exit code.
//!
//! ## Modules
//!
//! - [`tui`] — ratatui-based TUI: focus model, filter form, event loop,
//!   block rendering. Calls into `shed_core` for data-model state and
//!   filter execution; into [`exec`] for spawning commands; into [`ansi`]
//!   for rendering captured output.
//! - [`exec`] — PTY-based command execution via `portable-pty`. Returns a
//!   tokio `JoinHandle` for the (blocking) reader task plus a `ChildKiller`
//!   so cancellation actually terminates the child.
//! - [`ansi`] — ANSI escape parser using `vte`. Walks captured bytes and
//!   emits ratatui-styled `Span`s with SGR colors and modifiers.

use std::process::ExitCode;

mod ansi;
mod exec;
mod tui;

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(e) = tui::run().await {
        eprintln!("shed: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
