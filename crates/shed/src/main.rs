//! shed — interactive shell with retroactive pipelines.
//!
//! The binary entry point is intentionally thin: it spins up the tokio
//! runtime, hands off to [`tui::run`], and translates the result into a
//! process exit code.
//!
//! ## Modules
//!
//! - [`tui`] — ratatui-based TUI: focus model, filter form, event loop,
//!   shed rendering. Calls into `shed_core` for data-model state and
//!   filter execution; into [`exec`] for spawning commands; into [`ansi`]
//!   for rendering captured output.
//! - [`exec`] — PTY-based command execution via `portable-pty`. Returns a
//!   tokio `JoinHandle` for the (blocking) reader task plus a `ChildKiller`
//!   so cancellation actually terminates the child.
//! - [`ansi`] — ANSI escape parser using `vte`. Walks captured bytes and
//!   emits ratatui-styled `Span`s with SGR colors and modifiers.

use std::path::PathBuf;
use std::process::ExitCode;

mod ansi;
mod exec;
mod tui;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let notebook = match parse_args(&args) {
        Ok(n) => n,
        Err(msg) => {
            eprintln!("shed: {msg}");
            eprintln!("usage: shed [NOTEBOOK.json]");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = tui::run(notebook).await {
        eprintln!("shed: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn parse_args(args: &[String]) -> Result<Option<PathBuf>, String> {
    match args {
        [] => Ok(None),
        [arg] if arg == "-h" || arg == "--help" => {
            println!("usage: shed [NOTEBOOK.json]");
            std::process::exit(0);
        }
        [path] => Ok(Some(PathBuf::from(path))),
        _ => Err(format!("unexpected args: {}", args[1..].join(" "))),
    }
}
