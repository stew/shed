//! ratatui-based TUI: event loop, focus model, rendering, filter form.
//!
//! # Focus model
//!
//! shed has three focus contexts. The status bar at the bottom always
//! lists keys available in the current context:
//!
//! | Focus         | Purpose                                          |
//! |---------------|--------------------------------------------------|
//! | `Prompt`      | type commands; Enter spawns                      |
//! | `ShedCursor` | navigate sheds (↑↓), filters within (←→), edit |
//! | `FilterEdit`  | schema-aware form for the active filter          |
//! | `ShedExpand` | fullscreen pager for the selected shed          |
//!
//! Focus transitions: `Esc` from `Prompt` enters `ShedCursor` on the
//! newest shed; `Esc` from `ShedCursor` returns to `Prompt`; `f`/Enter
//! on a shed enters `FilterEdit`; `Esc` from `FilterEdit` cancels.
//!
//! # Concurrency
//!
//! Commands run as tokio `spawn_blocking` tasks (`portable-pty`'s API is
//! sync). The event loop polls completed tasks via
//! [`reap_completed`] each iteration and updates the corresponding shed.
//! The TUI never freezes during long-running commands; multiple commands
//! coexist with their own ⏵/●/⚠ glyphs.
//!
//! # Fullscreen handover
//!
//! Some commands (`top`, `vim`, `less`, …) need full terminal control.
//! [`spawn_prompt`] detects them via a built-in blacklist (or the `!`
//! prefix), sets [`App::pending_handover`], and the event loop performs
//! the handover at the top of its next iteration: tear down ratatui,
//! await the child with inherited stdio, re-init ratatui.
//!
//! # Form fields
//!
//! Each filter kind uses a small set of field types:
//! - **Select** — Kind, Column, Op, Direction, CsvDelim, CsvHasHeader: ←→ cycles
//! - **TextInput** — Pattern, RegexPattern, N (digits only), Rename "to": typing edits
//! - **Multi-select** — Columns (Select/Drop/Uniq): ↑↓ moves cursor, Space toggles
//! - **Multi-line list** — SortKeys, RenameMap: each row has its own state

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
#[cfg_attr(not(test), allow(unused_imports))]
use ratatui::{
    DefaultTerminal,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use shed_core::{
    Alias, AliasFile, Capture, CompareOp, Filter, FilterSpec, Notebook, NotebookEntry, OutputSpec,
    PipelineValue, Predicate, Session, Shed, ShedId, ShedState, SortDirection, SortKey, Value,
    apply_with_notes,
};
use tokio::task::JoinHandle;

use crate::ansi;
use crate::exec::{self, CaptureOutcome, ExecError, Killer};

mod clipboard;
mod completion;
mod input;
mod render;
mod tabs;
use clipboard::write_clipboard_osc52;
use completion::{CompletionState, cycle_completion};
use input::{
    InputOutcome, apply_readline_edit, handle_text_input, input_spans_with_cursor, render_input_bar,
};
use render::{cell_string, filter_error_lines, render_pipeline_value_with_max};
use tabs::{
    TabSlot, begin_rename_tab, drive_all_tabs, handle_rename_tab_input_key, try_handle_tab_key,
};

type CommandTask = JoinHandle<Result<CaptureOutcome, ExecError>>;

/// A clickable region of the screen registered by a draw pass.
/// Rebuilt every frame; hit-tested in [`handle_mouse_click`].
#[derive(Debug, Clone)]
struct ClickRegion {
    pub(crate) rect: Rect,
    pub(crate) action: ClickAction,
}

#[derive(Debug, Clone, Copy)]
enum ClickAction {
    /// Click on the `×` button on a shed — delete the shed.
    DeleteBlock(ShedId),
    /// Click on a tab in the tab bar — switch to that tab.
    SwitchTab(usize),
    /// Click on the `+` at the end of the tab bar — create a new tab.
    NewTab,
}

/// A right-clickable shed body. Records the inner rect plus the plain-text
/// rendering of each line so a right-click can target the specific line
/// under the cursor. Wrap is not currently accounted for — long lines that
/// wrap to multiple terminal rows resolve to the wrong index past row 0.
///
/// `cells` is populated when the body renders as a structured table; each
/// entry maps a screen rect to the typed cell value beneath it so
/// right-click can offer cell-specific actions (Copy cell, Copy filename,
/// …) before falling back to line-level options.
#[derive(Debug, Clone)]
struct BodyRegion {
    pub(crate) rect: Rect,
    pub(crate) shed_id: ShedId,
    pub(crate) lines: Vec<String>,
    pub(crate) cells: Vec<CellRegion>,
}

/// Absolute-coordinate hit rect for a single table cell, plus the typed
/// value the cell renders.
#[derive(Debug, Clone)]
struct CellRegion {
    pub(crate) rect: Rect,
    pub(crate) value: Value,
}

/// Pre-translation cell layout produced by the render functions.
/// `line_idx` is relative to the start of the body's rendered lines and
/// `x_offset` is in column units within that line — the caller (in
/// `draw_one_shed`) translates these to absolute screen coordinates
/// using the body's inner rect.
#[derive(Debug, Clone)]
struct CellLayout {
    pub(crate) line_idx: usize,
    pub(crate) x_offset: u16,
    pub(crate) width: u16,
    pub(crate) value: Value,
}

/// State for the floating context menu opened by right-clicking on a shed
/// body. `pos` is the top-left where the menu renders; the renderer
/// shifts it inward if it would overflow the frame.
#[derive(Debug, Clone)]
struct ContextMenu {
    pub(crate) pos: (u16, u16),
    pub(crate) items: Vec<ContextMenuItem>,
    pub(crate) selected: usize,
}

#[derive(Debug, Clone)]
struct ContextMenuItem {
    pub(crate) label: String,
    pub(crate) action: ContextMenuAction,
}

#[derive(Debug, Clone)]
enum ContextMenuAction {
    /// Write the payload to the system clipboard via OSC 52.
    CopyText(String),
    /// Insert the payload at the current prompt cursor.
    InsertAtPrompt(String),
}

/// Which single-line input bar is currently open. At most one input
/// bar is active at a time across the whole app, so they all share one
/// state slot ([`App::input_bar`]) rather than the historical 9-way
/// `*_input_mode/text/cursor` field tuples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputKind {
    /// Save-as path bar (Ctrl-S without a bound notebook path).
    Save,
    /// Open-notebook path bar (Ctrl-O).
    Open,
    /// Pin-shed name bar (`p` on a shed).
    Pin,
    /// Rerun argv bar (`r` on a shed). Pairs with `App::rerun_source_id`.
    Rerun,
    /// Write-output-to-file path bar (`w` on a shed).
    Write,
    /// In-place command editor (`e`/`f` on a shed's argv).
    CmdEdit,
    /// Alias-name bar (`A` on a shed).
    AliasName,
    /// Pager search bar (`/` or `?` in ShedExpand). Pairs with
    /// `App::search_input_backward` and `App::search_case_insensitive`.
    Search,
    /// Rename-tab bar (F2).
    RenameTab,
}

/// The single-line input bar's typed state. Lives on `App` as
/// `input_bar: Option<InputBar>`. Helper methods on `App`
/// (`open_input`, `close_input`, `is_input`, `input_text(_mut)`,
/// `input_cursor(_mut)`) hide the Option matching at the call site.
#[derive(Debug, Clone)]
pub(crate) struct InputBar {
    pub(crate) kind: InputKind,
    pub(crate) text: String,
    pub(crate) cursor: usize,
}

struct RunningCommand {
    handle: CommandTask,
    killer: Killer,
    /// Receiver of streaming output chunks from the reader task. Drained
    /// each tick by [`drain_streams`] into `stream_buf`.
    chunks: exec::ChunkReceiver,
    /// Accumulated bytes for the in-flight shed. Mirrored into
    /// `shed.capture.stdout` (as a `Bytes` copy) every time new chunks
    /// arrive, so the renderer sees streaming output. Dropped on reap;
    /// the final [`Capture`] from `handle.await` replaces it.
    stream_buf: BytesMut,
}

#[derive(Debug, Clone)]
struct HandoverRequest {
    argv: Vec<String>,
    /// If `Some`, reuse this shed's id (for auto-handover after
    /// alt-screen detection); if `None`, allocate a new one (for
    /// user-initiated handover via blacklist or `!` prefix).
    reuse_shed: Option<ShedId>,
}

/// Re-run a command with an edited argv but inherit the original
/// shed's pipeline. Created in ShedCursor by `r`; consumed at the
/// top of app_loop so we can spawn from an async context.
#[derive(Debug, Clone)]
struct RerunRequest {
    argv: Vec<String>,
    pipeline: Vec<FilterSpec>,
    force_fullscreen: bool,
}

const MAX_SORT_KEYS: usize = 5;

const FULLSCREEN_PROGRAMS: &[&str] = &[
    "top",
    "htop",
    "btop",
    "atop",
    "glances",
    "iotop",
    "iftop",
    "ncdu",
    "vi",
    "vim",
    "nvim",
    "emacs",
    "emacsclient",
    "nano",
    "pico",
    "helix",
    "hx",
    "micro",
    "kak",
    "less",
    "more",
    "most",
    "view",
    "man",
    "info",
    "pinfo",
    "tmux",
    "screen",
    "byobu",
    "zellij",
    "ssh",
    "mosh",
    "telnet",
    "rlogin",
    "tig",
    "lazygit",
    "gitui",
    "ranger",
    "nnn",
    "lf",
    "fzf",
    "sk",
];

fn needs_fullscreen(argv: &[String]) -> bool {
    let Some(prog) = argv.first() else {
        return false;
    };
    let basename = prog.rsplit('/').next().unwrap_or(prog);
    FULLSCREEN_PROGRAMS.contains(&basename)
}

const CAPTURE_CAP: usize = 16 * 1024 * 1024;
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const PREVIEW_LINES: usize = 8;
const MAX_UNDO_DEPTH: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Prompt,
    ShedCursor,
    /// Pipeline-edit mode for the cursor shed. Reveals the command and
    /// each filter on its own line with ←→ navigation; `f`/Enter opens
    /// the form editor for the active slot. `Esc` returns to ShedCursor.
    /// Shed-level actions (run, delete, pin, etc.) live in ShedCursor;
    /// EditShed is purely for pipeline mutation.
    EditShed,
    FilterEdit,
    ShedExpand,
    EnvEdit,
    Palette,
    NoteEdit,
    AliasManage,
}

/// State for the aliases manage view (Focus::AliasManage). Cursor is
/// the selected row; the alias list itself comes from `App.aliases`.
#[derive(Debug, Clone, Default)]
struct AliasManageState {
    pub(crate) cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotePosition {
    Pre,
    Post,
}

/// Multi-line text buffer state for editing a shed's pre or post note.
/// Cursor is a char index into `buffer`. Up/down navigation isn't
/// supported in v0; horizontal nav + backspace/delete cover most short
/// notes.
#[derive(Debug, Clone)]
struct NoteEditState {
    pub(crate) shed_id: ShedId,
    pub(crate) position: NotePosition,
    pub(crate) buffer: Vec<char>,
    pub(crate) cursor: usize,
}

impl NoteEditState {
    fn new(shed_id: ShedId, position: NotePosition, initial: Option<&str>) -> Self {
        let buffer: Vec<char> = initial.map(|s| s.chars().collect()).unwrap_or_default();
        let cursor = buffer.len();
        Self {
            shed_id,
            position,
            buffer,
            cursor,
        }
    }

    fn buffer_string(&self) -> String {
        self.buffer.iter().collect()
    }
}

#[derive(Debug, Clone)]
struct PaletteState {
    pub(crate) input: String,
    pub(crate) cursor: usize,
}

impl PaletteState {
    fn new() -> Self {
        Self {
            input: String::new(),
            cursor: 0,
        }
    }
}

/// One entry in the command palette. `enabled` decides whether the action
/// shows up at all in the current app state (e.g., "Pin shed" only makes
/// sense when a shed is selected); `handler` mutates `App` to perform
/// the action — typically by setting focus / state for downstream
/// handling, since most actions mirror existing keybindings.
struct Action {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) enabled: fn(&App) -> bool,
    pub(crate) handler: fn(&mut App),
}

fn always_enabled(_: &App) -> bool {
    true
}

fn any_sheds_exist(app: &App) -> bool {
    app.session.sheds().next().is_some()
}

fn shed_selected(app: &App) -> bool {
    app.session.cursor().is_some()
}

fn shed_with_capture(app: &App) -> bool {
    app.session
        .cursor()
        .and_then(|id| app.session.shed(id))
        .map(|b| b.capture.is_some())
        .unwrap_or(false)
}

const ACTIONS: &[Action] = &[
    Action {
        name: "Quit shed",
        description: "Exit the program",
        enabled: always_enabled,
        handler: |app| {
            app.quit = true;
        },
    },
    Action {
        name: "Focus prompt",
        description: "Return focus to the command prompt",
        enabled: always_enabled,
        handler: |app| {
            app.focus = Focus::Prompt;
            app.session.set_cursor(None);
        },
    },
    Action {
        name: "Focus newest shed",
        description: "Move the cursor to the most recent shed",
        enabled: any_sheds_exist,
        handler: |app| {
            if let Some(id) = app.newest_shed_id() {
                app.session.set_cursor(Some(id));
                app.reset_pipeline_cursor();
                app.focus = Focus::ShedCursor;
            }
        },
    },
    Action {
        name: "Open env editor",
        description: "Browse and modify environment variables",
        enabled: always_enabled,
        handler: |app| {
            app.env_edit = Some(EnvEditState::new());
            app.focus = Focus::EnvEdit;
        },
    },
    Action {
        name: "New tab",
        description: "Create a new tab with a fresh empty session",
        enabled: always_enabled,
        handler: |app| {
            app.new_tab();
        },
    },
    Action {
        name: "Close tab",
        description: "Close the active tab (refused when only one)",
        enabled: |app| app.tabs.len() > 1,
        handler: |app| {
            app.close_active_tab();
        },
    },
    Action {
        name: "Rename tab",
        description: "Set a custom name for the active tab",
        enabled: always_enabled,
        handler: begin_rename_tab,
    },
    Action {
        name: "Add filter",
        description: "Add a filter to the selected shed",
        enabled: shed_with_capture,
        handler: |app| {
            open_filter_edit(app);
        },
    },
    Action {
        name: "Insert filter",
        description: "Insert a filter before the cursor's filter",
        enabled: shed_with_capture,
        handler: |app| {
            open_filter_insert(app);
        },
    },
    Action {
        name: "Drop filter",
        description: "Remove the filter at the pipeline cursor",
        enabled: shed_with_capture,
        handler: |app| {
            app.focus = Focus::EditShed;
            drop_filter_at_cursor(app);
        },
    },
    Action {
        name: "Edit shed (pipeline)",
        description: "Reveal the command + filters and navigate them",
        enabled: shed_selected,
        handler: |app| {
            enter_edit_shed(app);
        },
    },
    Action {
        name: "View shed (pager)",
        description: "Open the selected shed in the fullscreen pager",
        enabled: shed_selected,
        handler: |app| {
            app.expand_scroll = 0;
            app.focus = Focus::ShedExpand;
        },
    },
    Action {
        name: "Write shed to file",
        description: "Save filtered output (.csv, .tsv, .json, or plain)",
        enabled: shed_selected,
        handler: |app| {
            app.focus = Focus::ShedCursor;
            app.open_input(InputKind::Write, String::new());
        },
    },
    Action {
        name: "Pin shed",
        description: "Open name input for the selected shed",
        enabled: shed_selected,
        handler: |app| {
            if let Some(id) = app.session.cursor() {
                let existing = app
                    .session
                    .shed(id)
                    .and_then(|b| b.name.clone())
                    .unwrap_or_default();
                app.open_input(InputKind::Pin, existing);
                app.focus = Focus::ShedCursor;
            }
        },
    },
    Action {
        name: "Unpin shed",
        description: "Clear the selected shed's name",
        enabled: shed_selected,
        handler: |app| {
            if let Some(id) = app.session.cursor() {
                app.session.unpin(id);
            }
            app.focus = Focus::ShedCursor;
        },
    },
    Action {
        name: "Edit command in place",
        description: "Edit the selected shed's argv; re-runs it and any pinned-name dependents",
        enabled: shed_selected,
        handler: |app| {
            app.focus = Focus::ShedCursor;
            open_cmd_edit(app);
        },
    },
    Action {
        name: "Rerun command",
        description: "Re-run the shed's command (edited) with the same pipeline",
        enabled: shed_selected,
        handler: |app| {
            if let Some(id) = app.session.cursor()
                && let Some(shed) = app.session.shed(id)
            {
                let joined = shlex::try_join(shed.argv.iter().map(String::as_str))
                    .unwrap_or_else(|_| shed.argv.join(" "));
                app.open_input(InputKind::Rerun, joined);
                app.rerun_source_id = Some(id);
            }
            app.focus = Focus::ShedCursor;
        },
    },
    Action {
        name: "Cancel running command",
        description: "Kill the selected shed's child process",
        enabled: shed_selected,
        handler: |app| {
            let _ = cancel_at_cursor(app);
            app.focus = Focus::ShedCursor;
        },
    },
    Action {
        name: "Run shed in place",
        description: "Re-spawn the selected shed's command, replacing its capture",
        enabled: shed_selected,
        handler: |app| {
            app.focus = Focus::ShedCursor;
            run_cursor_shed_in_place(app);
        },
    },
    Action {
        name: "Delete shed",
        description: "Remove the selected shed from the session",
        enabled: shed_selected,
        handler: |app| {
            app.focus = Focus::ShedCursor;
            delete_shed_at_cursor(app);
        },
    },
    Action {
        name: "Edit pre-note",
        description: "Edit the note rendered above the selected shed",
        enabled: shed_selected,
        handler: |app| {
            open_note_edit(app, NotePosition::Pre);
        },
    },
    Action {
        name: "Edit post-note",
        description: "Edit the note rendered below the selected shed",
        enabled: shed_selected,
        handler: |app| {
            open_note_edit(app, NotePosition::Post);
        },
    },
    Action {
        name: "Save notebook",
        description: "Save the session as a notebook (.json)",
        enabled: always_enabled,
        handler: |app| {
            begin_save(app);
        },
    },
    Action {
        name: "Open notebook",
        description: "Load a notebook from disk (replaces the current session)",
        enabled: always_enabled,
        handler: |app| {
            begin_open(app);
        },
    },
    Action {
        name: "Save shed as alias",
        description: "Save the selected shed's argv + pipeline as a global alias",
        enabled: shed_selected,
        handler: |app| {
            app.focus = Focus::ShedCursor;
            open_alias_save(app);
        },
    },
    Action {
        name: "Manage aliases",
        description: "Browse and delete saved aliases",
        enabled: always_enabled,
        handler: |app| {
            open_alias_manage(app);
        },
    },
    Action {
        name: "Undo",
        description: "Reverse the most recent structural change to the notebook",
        enabled: |app| !app.undo_stack.is_empty(),
        handler: |app| {
            undo(app);
        },
    },
    Action {
        name: "Redo",
        description: "Re-apply the most recently undone change",
        enabled: |app| !app.redo_stack.is_empty(),
        handler: |app| {
            redo(app);
        },
    },
];

fn matches_for_input(input: &str, app: &App) -> Vec<&'static Action> {
    let words: Vec<String> = input
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    ACTIONS
        .iter()
        .filter(|a| (a.enabled)(app))
        .filter(|a| {
            let lower = a.name.to_lowercase();
            words.iter().all(|w| lower.contains(w))
        })
        .collect()
}

#[derive(Debug, Clone)]
enum EnvInputMode {
    None,
    Filter,
    Edit(String),
    Add,
}

#[derive(Debug, Clone)]
struct EnvEditState {
    pub(crate) cursor: usize,
    pub(crate) filter: String,
    pub(crate) input_mode: EnvInputMode,
    pub(crate) input_buffer: String,
}

impl EnvEditState {
    fn new() -> Self {
        Self {
            cursor: 0,
            filter: String::new(),
            input_mode: EnvInputMode::None,
            input_buffer: String::new(),
        }
    }

    /// Snapshot of `std::env::vars()` sorted by key, optionally filtered
    /// by `self.filter` (case-insensitive substring on the key).
    pub(super) fn entries(&self) -> Vec<(String, String)> {
        let mut all: Vec<(String, String)> = std::env::vars().collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        if self.filter.is_empty() {
            all
        } else {
            let f = self.filter.to_lowercase();
            all.into_iter()
                .filter(|(k, _)| k.to_lowercase().contains(&f))
                .collect()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterKind {
    FromLines,
    FromFields,
    FromCsv,
    FromJson,
    FromRegex,
    Where,
    Select,
    Drop,
    Take,
    Skip,
    SortBy,
    Uniq,
    Count,
    Rename,
    Split,
    Join,
}

impl FilterKind {
    const ALL: &'static [FilterKind] = &[
        FilterKind::FromLines,
        FilterKind::FromFields,
        FilterKind::FromCsv,
        FilterKind::FromJson,
        FilterKind::FromRegex,
        FilterKind::Where,
        FilterKind::Select,
        FilterKind::Drop,
        FilterKind::Take,
        FilterKind::Skip,
        FilterKind::SortBy,
        FilterKind::Uniq,
        FilterKind::Count,
        FilterKind::Rename,
        FilterKind::Split,
        FilterKind::Join,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            FilterKind::FromLines => "from-lines",
            FilterKind::FromFields => "from-fields",
            FilterKind::FromCsv => "from-csv",
            FilterKind::FromJson => "from-json",
            FilterKind::FromRegex => "from-regex",
            FilterKind::Where => "where",
            FilterKind::Select => "select",
            FilterKind::Drop => "drop",
            FilterKind::Take => "take",
            FilterKind::Skip => "skip",
            FilterKind::SortBy => "sort-by",
            FilterKind::Uniq => "uniq",
            FilterKind::Count => "count",
            FilterKind::Rename => "rename",
            FilterKind::Split => "split",
            FilterKind::Join => "join",
        }
    }

    pub(super) fn description(self) -> &'static str {
        match self {
            FilterKind::FromLines => "parse bytes into one record per line",
            FilterKind::FromFields => "parse bytes into records by whitespace (auto _1, _2, …)",
            FilterKind::FromCsv => "parse bytes as CSV (delimiter + optional header row)",
            FilterKind::FromJson => "parse bytes as JSON (array of objects → rows)",
            FilterKind::FromRegex => "parse each line by a regex; named captures become columns",
            FilterKind::Where => "keep rows whose column matches a predicate",
            FilterKind::Select => "keep only the chosen columns",
            FilterKind::Drop => "remove the chosen columns",
            FilterKind::Take => "keep the first N rows",
            FilterKind::Skip => "drop the first N rows",
            FilterKind::SortBy => "sort rows by a column (numeric coercion when both sides parse)",
            FilterKind::Uniq => "drop duplicate rows (optionally keyed by columns)",
            FilterKind::Count => "collapse to a single row with the row count",
            FilterKind::Rename => "rename columns (leave a row blank to keep its current name)",
            FilterKind::Split => "split each row's column value by a delimiter, one row per piece",
            FilterKind::Join => {
                "concatenate every row's column value with a delimiter into one row"
            }
        }
    }
}

const DELIM_CHOICES: &[(char, &str)] = &[
    (',', "comma"),
    ('\t', "tab"),
    (';', "semicolon"),
    ('|', "pipe"),
];

fn delim_label(c: char) -> &'static str {
    DELIM_CHOICES
        .iter()
        .find_map(|(ch, label)| if *ch == c { Some(*label) } else { None })
        .unwrap_or("?")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    Kind,
    Column,
    Op,
    Pattern,
    N,
    Columns,
    CsvDelim,
    CsvHasHeader,
    RegexPattern,
    SortKeys,
    RenameMap,
    WhereCombine,
    WhereClauseSelect,
    TargetColumn,
    DelimText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhereOp {
    Matches,
    Contains,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl WhereOp {
    const ALL: &'static [WhereOp] = &[
        WhereOp::Matches,
        WhereOp::Contains,
        WhereOp::Eq,
        WhereOp::Ne,
        WhereOp::Lt,
        WhereOp::Le,
        WhereOp::Gt,
        WhereOp::Ge,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            WhereOp::Matches => "matches",
            WhereOp::Contains => "contains",
            WhereOp::Eq => "=",
            WhereOp::Ne => "≠",
            WhereOp::Lt => "<",
            WhereOp::Le => "≤",
            WhereOp::Gt => ">",
            WhereOp::Ge => "≥",
        }
    }

    pub(super) fn description(self) -> &'static str {
        match self {
            WhereOp::Matches => "regex match (unanchored)",
            WhereOp::Contains => "case-sensitive substring",
            WhereOp::Eq => "equal",
            WhereOp::Ne => "not equal",
            WhereOp::Lt => "less than",
            WhereOp::Le => "less than or equal",
            WhereOp::Gt => "greater than",
            WhereOp::Ge => "greater than or equal",
        }
    }

    pub(super) fn value_label(self) -> &'static str {
        match self {
            WhereOp::Matches => "pattern",
            WhereOp::Contains => "substring",
            _ => "value",
        }
    }

    pub(super) fn to_compare_op(self) -> Option<CompareOp> {
        match self {
            WhereOp::Eq => Some(CompareOp::Eq),
            WhereOp::Ne => Some(CompareOp::Ne),
            WhereOp::Lt => Some(CompareOp::Lt),
            WhereOp::Le => Some(CompareOp::Le),
            WhereOp::Gt => Some(CompareOp::Gt),
            WhereOp::Ge => Some(CompareOp::Ge),
            _ => None,
        }
    }
}

fn compare_op_to_where_op(op: CompareOp) -> WhereOp {
    match op {
        CompareOp::Eq => WhereOp::Eq,
        CompareOp::Ne => WhereOp::Ne,
        CompareOp::Lt => WhereOp::Lt,
        CompareOp::Le => WhereOp::Le,
        CompareOp::Gt => WhereOp::Gt,
        CompareOp::Ge => WhereOp::Ge,
    }
}

#[derive(Debug, Clone)]
struct WhereClause {
    pub(crate) column: usize,
    pub(crate) op: WhereOp,
    pub(crate) pattern: String,
}

impl WhereClause {
    fn default_for(_available_columns: &[String]) -> Self {
        Self {
            column: 0,
            op: WhereOp::Matches,
            pattern: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhereCombine {
    And,
    Or,
}

impl WhereCombine {
    pub(super) fn name(self) -> &'static str {
        match self {
            WhereCombine::And => "AND",
            WhereCombine::Or => "OR",
        }
    }

    pub(super) fn description(self) -> &'static str {
        match self {
            WhereCombine::And => "all clauses must match",
            WhereCombine::Or => "any clause may match",
        }
    }

    fn flip(self) -> Self {
        match self {
            WhereCombine::And => WhereCombine::Or,
            WhereCombine::Or => WhereCombine::And,
        }
    }
}

fn combine_predicates(preds: Vec<Predicate>, combine: WhereCombine) -> Predicate {
    let mut iter = preds.into_iter();
    let first = iter.next().expect("at least one predicate");
    iter.fold(first, |acc, p| match combine {
        WhereCombine::And => Predicate::And(Box::new(acc), Box::new(p)),
        WhereCombine::Or => Predicate::Or(Box::new(acc), Box::new(p)),
    })
}

/// Walk a predicate tree and try to flatten it into a (combine, clauses)
/// pair the form can edit. Flat And-chains and Or-chains flatten cleanly;
/// mixed nesting and Not fall back to a single empty clause.
fn decompose_predicate(p: &Predicate, columns: &[String]) -> (WhereCombine, Vec<WhereClause>) {
    let mut clauses = Vec::new();
    match p {
        Predicate::And(_, _) => {
            if collect_chain(p, &mut clauses, columns, true) {
                return (WhereCombine::And, clauses);
            }
        }
        Predicate::Or(_, _) => {
            if collect_chain(p, &mut clauses, columns, false) {
                return (WhereCombine::Or, clauses);
            }
        }
        leaf => {
            if let Some(c) = leaf_to_clause(leaf, columns) {
                return (WhereCombine::And, vec![c]);
            }
        }
    }
    (WhereCombine::And, Vec::new())
}

/// Collect leaves of a flat And-chain (`is_and`) or Or-chain into `clauses`.
/// Returns false if the tree mixes operators or contains a Not — in that
/// case the caller falls back to a single empty clause.
fn collect_chain(
    p: &Predicate,
    clauses: &mut Vec<WhereClause>,
    columns: &[String],
    is_and: bool,
) -> bool {
    match p {
        Predicate::And(a, b) if is_and => {
            collect_chain(a, clauses, columns, is_and) && collect_chain(b, clauses, columns, is_and)
        }
        Predicate::Or(a, b) if !is_and => {
            collect_chain(a, clauses, columns, is_and) && collect_chain(b, clauses, columns, is_and)
        }
        leaf => match leaf_to_clause(leaf, columns) {
            Some(c) => {
                clauses.push(c);
                true
            }
            None => false,
        },
    }
}

fn leaf_to_clause(p: &Predicate, columns: &[String]) -> Option<WhereClause> {
    match p {
        Predicate::Matches { column, pattern } => Some(WhereClause {
            column: columns.iter().position(|c| c == column).unwrap_or(0),
            op: WhereOp::Matches,
            pattern: pattern.clone(),
        }),
        Predicate::Contains { column, substring } => Some(WhereClause {
            column: columns.iter().position(|c| c == column).unwrap_or(0),
            op: WhereOp::Contains,
            pattern: substring.clone(),
        }),
        Predicate::Compare { column, op, value } => Some(WhereClause {
            column: columns.iter().position(|c| c == column).unwrap_or(0),
            op: compare_op_to_where_op(*op),
            pattern: value_to_input_string(value),
        }),
        _ => None,
    }
}

fn parse_value_input(s: &str) -> Value {
    let trimmed = s.trim();
    match trimmed {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "" => Value::String(String::new()),
        _ => {
            if let Ok(i) = trimmed.parse::<i64>() {
                return Value::Int(i);
            }
            if let Ok(f) = trimmed.parse::<f64>() {
                return Value::Float(f);
            }
            Value::String(trimmed.to_string())
        }
    }
}

fn value_to_input_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => s.clone(),
        Value::Bytes(b) => format!("<{} bytes>", b.len()),
        _ => format!("{v:?}"),
    }
}

/// How the in-progress filter will be committed when the user presses
/// Enter. Set when entering FilterEdit and respected by `apply_filter_edit`,
/// the live preview's hypothetical pipeline, and the stack-pane renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditMode {
    /// Append to the end of the pipeline (`f` from `+ add` slot).
    Add,
    /// Replace the existing filter at this index (`f` on a filter).
    Edit(usize),
    /// Insert as a new filter before the existing one at this index
    /// (`i` on a filter); existing filters from this index onward
    /// shift right by one.
    Insert(usize),
}

struct FilterEditState {
    pub(crate) shed_id: ShedId,
    pub(crate) kind: FilterKind,
    pub(crate) where_clauses: Vec<WhereClause>,
    pub(crate) where_active_clause: usize,
    pub(crate) where_combine: WhereCombine,
    pub(crate) n_input: String,
    pub(crate) column_selections: Vec<bool>,
    pub(crate) column_cursor: usize,
    pub(crate) csv_delim: char,
    pub(crate) csv_has_header: bool,
    pub(crate) regex_pattern: String,
    pub(crate) sort_keys: Vec<(usize, SortDirection)>,
    pub(crate) sort_keys_cursor: usize,
    pub(crate) rename_to_inputs: Vec<String>,
    pub(crate) rename_cursor: usize,
    pub(crate) target_column: usize,
    pub(crate) delim_text: String,
    pub(crate) available_columns: Vec<String>,
    pub(crate) field: FormField,
    pub(crate) mode: EditMode,
}

impl FilterEditState {
    fn empty(shed_id: ShedId, available_columns: Vec<String>, mode: EditMode) -> Self {
        let column_selections = vec![false; available_columns.len()];
        Self {
            shed_id,
            kind: FilterKind::FromLines,
            where_clauses: vec![WhereClause::default_for(&available_columns)],
            where_active_clause: 0,
            where_combine: WhereCombine::And,
            n_input: String::new(),
            column_selections,
            column_cursor: 0,
            csv_delim: ',',
            csv_has_header: true,
            regex_pattern: String::new(),
            sort_keys: if available_columns.is_empty() {
                Vec::new()
            } else {
                vec![(0, SortDirection::Asc)]
            },
            sort_keys_cursor: 0,
            rename_to_inputs: vec![String::new(); available_columns.len()],
            rename_cursor: 0,
            target_column: 0,
            delim_text: String::new(),
            available_columns,
            field: FormField::Kind,
            mode,
        }
    }

    fn for_add(shed: &Shed) -> Self {
        let available_columns = compute_schema_at(shed, shed.pipeline.len());
        let mut state = Self::empty(shed.id, available_columns, EditMode::Add);
        state.kind = if state.available_columns.is_empty() {
            FilterKind::FromLines
        } else {
            FilterKind::Where
        };
        state
    }

    fn for_insert(shed: &Shed, index: usize) -> Self {
        let available_columns = compute_schema_at(shed, index);
        let mut state = Self::empty(shed.id, available_columns, EditMode::Insert(index));
        state.kind = if state.available_columns.is_empty() {
            FilterKind::FromLines
        } else {
            FilterKind::Where
        };
        state
    }

    fn for_edit(shed: &Shed, index: usize) -> Self {
        let available_columns = compute_schema_at(shed, index);
        let mut state = Self::empty(shed.id, available_columns, EditMode::Edit(index));
        match shed.pipeline.get(index) {
            Some(FilterSpec::FromLines) => state.kind = FilterKind::FromLines,
            Some(FilterSpec::FromFields) => state.kind = FilterKind::FromFields,
            Some(FilterSpec::FromCsv { delim, has_header }) => {
                state.kind = FilterKind::FromCsv;
                state.csv_delim = *delim;
                state.csv_has_header = *has_header;
            }
            Some(FilterSpec::FromJson) => state.kind = FilterKind::FromJson,
            Some(FilterSpec::FromRegex { pattern }) => {
                state.kind = FilterKind::FromRegex;
                state.regex_pattern = pattern.clone();
            }
            Some(FilterSpec::SortBy { keys }) => {
                state.kind = FilterKind::SortBy;
                state.sort_keys = keys
                    .iter()
                    .take(MAX_SORT_KEYS)
                    .map(|k| {
                        let idx = state
                            .available_columns
                            .iter()
                            .position(|c| c == &k.column)
                            .unwrap_or(0);
                        (idx, k.direction)
                    })
                    .collect();
                if state.sort_keys.is_empty() && !state.available_columns.is_empty() {
                    state.sort_keys.push((0, SortDirection::Asc));
                }
            }
            Some(FilterSpec::Uniq { by }) => {
                state.kind = FilterKind::Uniq;
                if let Some(cols) = by {
                    for col in cols {
                        if let Some(i) = state.available_columns.iter().position(|c| c == col) {
                            state.column_selections[i] = true;
                        }
                    }
                }
            }
            Some(FilterSpec::Count) => state.kind = FilterKind::Count,
            Some(FilterSpec::Rename { pairs }) => {
                state.kind = FilterKind::Rename;
                for (from, to) in pairs {
                    if let Some(i) = state.available_columns.iter().position(|c| c == from)
                        && let Some(slot) = state.rename_to_inputs.get_mut(i)
                    {
                        *slot = to.clone();
                    }
                }
            }
            Some(FilterSpec::Split { column, delimiter }) => {
                state.kind = FilterKind::Split;
                state.target_column = state
                    .available_columns
                    .iter()
                    .position(|c| c == column)
                    .unwrap_or(0);
                state.delim_text = delimiter.clone();
            }
            Some(FilterSpec::Join { column, delimiter }) => {
                state.kind = FilterKind::Join;
                state.target_column = state
                    .available_columns
                    .iter()
                    .position(|c| c == column)
                    .unwrap_or(0);
                state.delim_text = delimiter.clone();
            }
            Some(FilterSpec::Where { predicate }) => {
                state.kind = FilterKind::Where;
                let (combine, mut clauses) =
                    decompose_predicate(predicate, &state.available_columns);
                if clauses.is_empty() {
                    clauses.push(WhereClause::default_for(&state.available_columns));
                }
                state.where_combine = combine;
                state.where_clauses = clauses;
                state.where_active_clause = 0;
            }
            Some(FilterSpec::Select { columns }) => {
                state.kind = FilterKind::Select;
                for col in columns {
                    if let Some(i) = state.available_columns.iter().position(|c| c == col) {
                        state.column_selections[i] = true;
                    }
                }
            }
            Some(FilterSpec::Drop { columns }) => {
                state.kind = FilterKind::Drop;
                for col in columns {
                    if let Some(i) = state.available_columns.iter().position(|c| c == col) {
                        state.column_selections[i] = true;
                    }
                }
            }
            Some(FilterSpec::Take { n }) => {
                state.kind = FilterKind::Take;
                state.n_input = n.to_string();
            }
            Some(FilterSpec::Skip { n }) => {
                state.kind = FilterKind::Skip;
                state.n_input = n.to_string();
            }
            None => {
                state.kind = if state.available_columns.is_empty() {
                    FilterKind::FromLines
                } else {
                    FilterKind::Where
                };
            }
        }
        state
    }

    pub(super) fn fields(&self) -> &'static [FormField] {
        match self.kind {
            FilterKind::FromLines
            | FilterKind::FromFields
            | FilterKind::FromJson
            | FilterKind::Count => &[FormField::Kind],
            FilterKind::FromCsv => &[
                FormField::Kind,
                FormField::CsvDelim,
                FormField::CsvHasHeader,
            ],
            FilterKind::FromRegex => &[FormField::Kind, FormField::RegexPattern],
            FilterKind::Where => &[
                FormField::Kind,
                FormField::WhereCombine,
                FormField::WhereClauseSelect,
                FormField::Column,
                FormField::Op,
                FormField::Pattern,
            ],
            FilterKind::Select | FilterKind::Drop | FilterKind::Uniq => {
                &[FormField::Kind, FormField::Columns]
            }
            FilterKind::Take | FilterKind::Skip => &[FormField::Kind, FormField::N],
            FilterKind::SortBy => &[FormField::Kind, FormField::SortKeys],
            FilterKind::Rename => &[FormField::Kind, FormField::RenameMap],
            FilterKind::Split | FilterKind::Join => &[
                FormField::Kind,
                FormField::TargetColumn,
                FormField::DelimText,
            ],
        }
    }

    fn field_height(&self, field: FormField) -> u16 {
        match field {
            FormField::SortKeys => {
                let n = self.sort_keys.len();
                let with_add = if n < MAX_SORT_KEYS { n + 1 } else { n };
                with_add.max(1) as u16
            }
            FormField::RenameMap => self.available_columns.len().max(1) as u16,
            _ => 1,
        }
    }

    pub(super) fn form_lines(&self) -> u16 {
        self.fields().iter().map(|f| self.field_height(*f)).sum()
    }

    fn cycle_delim(&mut self, delta: i32) {
        let i = DELIM_CHOICES
            .iter()
            .position(|(c, _)| *c == self.csv_delim)
            .unwrap_or(0) as i32;
        let new_i = (i + delta).rem_euclid(DELIM_CHOICES.len() as i32) as usize;
        self.csv_delim = DELIM_CHOICES[new_i].0;
    }

    fn cycle_field(&mut self, delta: i32) {
        let fs = self.fields();
        let idx = fs.iter().position(|f| *f == self.field).unwrap_or(0) as i32;
        let new_idx = (idx + delta).rem_euclid(fs.len() as i32) as usize;
        self.field = fs[new_idx];
    }

    fn ensure_field_valid(&mut self) {
        if !self.fields().contains(&self.field) {
            self.field = FormField::Kind;
        }
    }

    fn active_clause(&self) -> Option<&WhereClause> {
        self.where_clauses.get(self.where_active_clause)
    }

    fn active_clause_mut(&mut self) -> Option<&mut WhereClause> {
        self.where_clauses.get_mut(self.where_active_clause)
    }

    fn cycle_kind(&mut self, delta: i32) {
        let i = FilterKind::ALL
            .iter()
            .position(|k| *k == self.kind)
            .unwrap_or(0) as i32;
        let new_i = (i + delta).rem_euclid(FilterKind::ALL.len() as i32) as usize;
        self.kind = FilterKind::ALL[new_i];
        self.ensure_field_valid();
    }

    fn cycle_column(&mut self, delta: i32) {
        if self.available_columns.is_empty() {
            return;
        }
        let len = self.available_columns.len();
        if let Some(clause) = self.active_clause_mut() {
            let i = clause.column as i32;
            clause.column = (i + delta).rem_euclid(len as i32) as usize;
        }
    }

    fn cycle_clause_op(&mut self, delta: i32) {
        if let Some(clause) = self.active_clause_mut() {
            let i = WhereOp::ALL
                .iter()
                .position(|o| *o == clause.op)
                .unwrap_or(0) as i32;
            let new_i = (i + delta).rem_euclid(WhereOp::ALL.len() as i32) as usize;
            clause.op = WhereOp::ALL[new_i];
        }
    }

    pub(super) fn selected_column(&self) -> Option<&str> {
        let idx = self.active_clause().map(|c| c.column)?;
        self.available_columns.get(idx).map(|s| s.as_str())
    }

    pub(super) fn active_op(&self) -> WhereOp {
        self.active_clause()
            .map(|c| c.op)
            .unwrap_or(WhereOp::Matches)
    }

    pub(super) fn active_pattern(&self) -> &str {
        self.active_clause()
            .map(|c| c.pattern.as_str())
            .unwrap_or("")
    }

    fn selected_columns(&self) -> Vec<String> {
        self.column_selections
            .iter()
            .enumerate()
            .filter_map(|(i, sel)| {
                if *sel {
                    self.available_columns.get(i).cloned()
                } else {
                    None
                }
            })
            .collect()
    }

    fn parsed_n(&self) -> Option<usize> {
        self.n_input.trim().parse().ok()
    }

    pub(super) fn build_filter(&self) -> Option<FilterSpec> {
        match self.kind {
            FilterKind::FromLines => Some(FilterSpec::FromLines),
            FilterKind::FromFields => Some(FilterSpec::FromFields),
            FilterKind::FromCsv => Some(FilterSpec::FromCsv {
                delim: self.csv_delim,
                has_header: self.csv_has_header,
            }),
            FilterKind::FromJson => Some(FilterSpec::FromJson),
            FilterKind::FromRegex => {
                if self.regex_pattern.trim().is_empty() {
                    None
                } else {
                    Some(FilterSpec::FromRegex {
                        pattern: self.regex_pattern.clone(),
                    })
                }
            }
            FilterKind::Where => {
                if self.where_clauses.is_empty() || self.available_columns.is_empty() {
                    return None;
                }
                let leaves: Vec<Predicate> = self
                    .where_clauses
                    .iter()
                    .filter_map(|clause| {
                        let column = self.available_columns.get(clause.column)?.to_string();
                        let pred = match clause.op {
                            WhereOp::Matches => Predicate::Matches {
                                column,
                                pattern: clause.pattern.clone(),
                            },
                            WhereOp::Contains => Predicate::Contains {
                                column,
                                substring: clause.pattern.clone(),
                            },
                            other => Predicate::Compare {
                                column,
                                op: other.to_compare_op()?,
                                value: parse_value_input(&clause.pattern),
                            },
                        };
                        Some(pred)
                    })
                    .collect();
                if leaves.is_empty() {
                    None
                } else {
                    Some(FilterSpec::Where {
                        predicate: combine_predicates(leaves, self.where_combine),
                    })
                }
            }
            FilterKind::Select => {
                let cols = self.selected_columns();
                if cols.is_empty() {
                    None
                } else {
                    Some(FilterSpec::Select { columns: cols })
                }
            }
            FilterKind::Drop => {
                let cols = self.selected_columns();
                if cols.is_empty() {
                    None
                } else {
                    Some(FilterSpec::Drop { columns: cols })
                }
            }
            FilterKind::Take => self.parsed_n().map(|n| FilterSpec::Take { n }),
            FilterKind::Skip => self.parsed_n().map(|n| FilterSpec::Skip { n }),
            FilterKind::SortBy => {
                if self.sort_keys.is_empty() {
                    return None;
                }
                let keys: Vec<SortKey> = self
                    .sort_keys
                    .iter()
                    .filter_map(|(idx, dir)| {
                        self.available_columns.get(*idx).map(|col| SortKey {
                            column: col.clone(),
                            direction: *dir,
                        })
                    })
                    .collect();
                if keys.is_empty() {
                    None
                } else {
                    Some(FilterSpec::SortBy { keys })
                }
            }
            FilterKind::Uniq => {
                let cols = self.selected_columns();
                Some(FilterSpec::Uniq {
                    by: if cols.is_empty() { None } else { Some(cols) },
                })
            }
            FilterKind::Count => Some(FilterSpec::Count),
            FilterKind::Rename => {
                let pairs: Vec<(String, String)> = self
                    .rename_to_inputs
                    .iter()
                    .enumerate()
                    .filter_map(|(i, to)| {
                        let to = to.trim();
                        if to.is_empty() {
                            None
                        } else {
                            self.available_columns
                                .get(i)
                                .map(|from| (from.clone(), to.to_string()))
                        }
                    })
                    .collect();
                if pairs.is_empty() {
                    None
                } else {
                    Some(FilterSpec::Rename { pairs })
                }
            }
            FilterKind::Split => {
                let column = self.available_columns.get(self.target_column)?.clone();
                Some(FilterSpec::Split {
                    column,
                    delimiter: self.delim_text.clone(),
                })
            }
            FilterKind::Join => {
                let column = self.available_columns.get(self.target_column)?.clone();
                Some(FilterSpec::Join {
                    column,
                    delimiter: self.delim_text.clone(),
                })
            }
        }
    }
}

fn compute_schema_at(shed: &Shed, before_index: usize) -> Vec<String> {
    let Some(capture) = &shed.capture else {
        return Vec::new();
    };
    // Inherited structured snapshots (typed @name / %N transport) carry
    // their schema in `capture.structured`, so the filter form sees the
    // columns immediately — no implicit `from-json` needed.
    let mut value = match &capture.structured {
        Some(v) => PipelineValue::Structured(v.clone()),
        None => PipelineValue::Bytes(capture.stdout.clone()),
    };
    for filter in shed.pipeline.iter().take(before_index) {
        match filter.apply(value) {
            Ok(v) => value = v,
            Err(_) => return Vec::new(),
        }
    }
    match value {
        PipelineValue::Structured(Value::List(items)) => items
            .iter()
            .find_map(|v| match v {
                Value::Record(r) => Some(r.keys().cloned().collect()),
                _ => None,
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

struct App {
    pub(crate) session: Session,
    pub(crate) prompt: String,
    /// Byte offset of the insertion caret in [`App::prompt`]. Always a
    /// char boundary in `[0, prompt.len()]`.
    pub(crate) prompt_cursor: usize,
    pub(crate) history: Vec<String>,
    pub(crate) history_cursor: Option<usize>,
    /// Active tab-completion cycle, or `None` if Tab hasn't been pressed
    /// since the last edit. Cleared on any non-Tab key in a completion
    /// context. See [`cycle_completion`].
    pub(crate) completion: Option<CompletionState>,
    pub(crate) rerun_source_id: Option<ShedId>,
    pub(crate) pending_rerun: Option<RerunRequest>,
    /// True when the ShedCursor's "filter cursor" has been pulled left
    /// past the first filter onto the command itself. Visually highlights
    /// the argv span; Enter opens the in-place command editor.
    pub(crate) command_focused: bool,
    pub(crate) last_cwd: Option<PathBuf>,
    pub(crate) env_edit: Option<EnvEditState>,
    pub(crate) note_edit: Option<NoteEditState>,
    pub(crate) palette_state: Option<PaletteState>,
    pub(crate) palette_prev_focus: Option<Focus>,
    pub(crate) focus: Focus,
    pub(crate) filter_edit: Option<FilterEditState>,
    pub(crate) pipeline_cursor: usize,
    pub(crate) expand_scroll: usize,
    pub(crate) search_query: String,
    pub(crate) search_anchor_scroll: usize,
    pub(crate) search_input_backward: bool,
    pub(crate) search_case_insensitive: bool,
    pub(crate) flash: Option<String>,
    pub(crate) quit: bool,
    pub(crate) running: HashMap<ShedId, RunningCommand>,
    pub(crate) pending_handover: Option<HandoverRequest>,
    /// Path the notebook is bound to (set by `--open`, by Ctrl-O, or by
    /// the first Ctrl-S that prompted for a path). Subsequent Ctrl-S
    /// saves silently to this path.
    pub(crate) notebook_path: Option<PathBuf>,
    /// Cross-session aliases: typing the alias name at the prompt
    /// materialises a shed with the saved argv + pipeline. Loaded once
    /// from `aliases_path` on startup, rewritten on every change.
    pub(crate) aliases: AliasFile,
    pub(crate) aliases_path: Option<PathBuf>,
    /// Pending overwrite confirmation when `A` collides with an existing
    /// alias name. Holds the would-be entry; user resolves with y/n/c.
    pub(crate) alias_overwrite: Option<Alias>,
    /// Manage view state (Focus::AliasManage). `None` outside the view.
    pub(crate) alias_manage: Option<AliasManageState>,
    /// `true` when the session has unsaved changes. Set whenever a shed
    /// is added, edited, pinned/unpinned, re-run, or its pipeline mutated.
    /// Cleared on save/load.
    pub(crate) dirty: bool,
    /// JSON-serialised snapshot of the *pinned* sheds at the last save
    /// or load. The exit prompt fires only when the current pinned JSON
    /// differs from this — unpinned-shed edits are scratch work and
    /// don't nag the user on quit.
    pub(crate) saved_pinned_json: String,
    /// Queue of sheds to run in sequence (head first). Built by walking
    /// `@-ref` deps so a snapshot shed runs its source before itself.
    /// The event loop kicks off one at a time and gates on terminal state.
    pub(crate) pending_run_chain: VecDeque<ShedId>,
    /// Shed currently being processed by the run-in-place machinery.
    /// While `Some`, the next chain item won't start. Cleared once the
    /// shed reaches a terminal state.
    pub(crate) chain_in_flight: Option<ShedId>,
    /// Snapshots taken before each structural mutation. Bounded; oldest
    /// drops first when full. Captures are shared via `bytes::Bytes`
    /// refcounting so the memory cost is roughly one BTreeMap clone per
    /// entry.
    pub(crate) undo_stack: Vec<Session>,
    /// Snapshots that were undone past. Cleared on every fresh
    /// structural mutation so redo only chains forward through actual
    /// undos.
    pub(crate) redo_stack: Vec<Session>,
    /// The single-line input bar at the bottom of the screen, when one
    /// is open. At most one is open at a time (save / open / pin /
    /// rerun / write / cmd-edit / alias-name / search / rename-tab —
    /// see [`InputKind`]).
    pub(crate) input_bar: Option<InputBar>,
    /// "Save before quitting?" exit prompt. Showing while non-None;
    /// keys map to y / n / c (cancel).
    pub(crate) exit_prompt: Option<ExitPrompt>,
    /// Clickable screen regions registered by the last draw pass.
    /// Rebuilt every frame; hit-tested when a mouse click arrives.
    pub(crate) click_regions: Vec<ClickRegion>,
    /// Per-shed body regions captured during the last draw pass. Right
    /// clicks hit-test these to open the line-targeted context menu.
    pub(crate) body_regions: Vec<BodyRegion>,
    /// Open right-click context menu, if any. While `Some`, all mouse
    /// and key events route to the menu before normal focus handling.
    pub(crate) context_menu: Option<ContextMenu>,
    /// Tabs. Always non-empty; `tabs[active_tab]` corresponds to the
    /// fields above (its `stashed` is `None`). Other entries hold their
    /// per-tab persistent state in `stashed`.
    pub(crate) tabs: Vec<TabSlot>,
    pub(crate) active_tab: usize,
}

/// Disposition of the save-on-quit prompt. `AwaitingPath` is the rare
/// case where the user said "save" but no notebook path is bound, so we
/// reuse the save input bar to ask for one before quitting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitPrompt {
    Confirm,
    AwaitingPath,
}

impl Drop for App {
    fn drop(&mut self) {
        // Kill any still-running children; the blocking PTY-reader tasks will
        // then exit on their own as the slave closes. Abort the JoinHandles
        // for good measure (the spawn_blocking task will continue briefly
        // until reader returns, which is fine — we're exiting).
        for (_, mut cmd) in self.running.drain() {
            let _ = cmd.killer.kill();
            cmd.handle.abort();
        }
        // Same for any inactive tabs that have running commands.
        for slot in &mut self.tabs {
            if let Some(stashed) = &mut slot.stashed {
                for (_, mut cmd) in stashed.running.drain() {
                    let _ = cmd.killer.kill();
                    cmd.handle.abort();
                }
            }
        }
    }
}

impl App {
    fn new() -> Self {
        Self {
            session: Session::new(),
            prompt: String::new(),
            prompt_cursor: 0,
            history: load_history_from_default_path().unwrap_or_default(),
            history_cursor: None,
            completion: None,
            rerun_source_id: None,
            pending_rerun: None,
            command_focused: false,
            last_cwd: None,
            env_edit: None,
            note_edit: None,
            palette_state: None,
            palette_prev_focus: None,
            focus: Focus::Prompt,
            filter_edit: None,
            pipeline_cursor: 0,
            expand_scroll: 0,
            search_query: String::new(),
            search_anchor_scroll: 0,
            search_input_backward: false,
            search_case_insensitive: false,
            flash: None,
            quit: false,
            running: HashMap::new(),
            pending_handover: None,
            notebook_path: None,
            aliases: load_aliases_from_default_path().unwrap_or_default(),
            aliases_path: aliases_file_path(),
            alias_overwrite: None,
            alias_manage: None,
            dirty: false,
            saved_pinned_json: pinned_entries_json(&Session::new()),
            pending_run_chain: VecDeque::new(),
            chain_in_flight: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            input_bar: None,
            exit_prompt: None,
            click_regions: Vec::new(),
            body_regions: Vec::new(),
            context_menu: None,
            tabs: vec![TabSlot::new_active(None)],
            active_tab: 0,
        }
    }

    /// Open an input bar. Replaces any currently-open bar — only one
    /// can be active at a time. `initial` pre-fills the text; the
    /// cursor lands at the end.
    pub(crate) fn open_input(&mut self, kind: InputKind, initial: String) {
        let cursor = initial.len();
        self.input_bar = Some(InputBar {
            kind,
            text: initial,
            cursor,
        });
    }

    /// Close any open input bar.
    pub(crate) fn close_input(&mut self) {
        self.input_bar = None;
    }

    /// `true` when the currently-open input bar is of `kind`.
    pub(crate) fn is_input(&self, kind: InputKind) -> bool {
        self.input_bar.as_ref().map(|b| b.kind) == Some(kind)
    }

    pub(crate) fn input_text(&self) -> &str {
        self.input_bar
            .as_ref()
            .map(|b| b.text.as_str())
            .unwrap_or("")
    }

    pub(crate) fn input_cursor(&self) -> usize {
        self.input_bar.as_ref().map(|b| b.cursor).unwrap_or(0)
    }

    /// Mutable handles to the open bar's text + cursor for readline
    /// editing helpers. Returns `None` if no bar is open.
    pub(crate) fn input_mut(&mut self) -> Option<(&mut String, &mut usize)> {
        self.input_bar
            .as_mut()
            .map(|b| (&mut b.text, &mut b.cursor))
    }

    /// Take the open bar's text out, replacing it with empty and
    /// closing the bar. Used by commit handlers that consume the input.
    pub(crate) fn take_input_text(&mut self) -> String {
        match self.input_bar.take() {
            Some(b) => b.text,
            None => String::new(),
        }
    }

    /// Snapshot the current session and push it onto the undo stack
    /// before applying a structural mutation. Clears redo (any fresh
    /// edit invalidates the redo path) and marks the notebook dirty.
    /// Capped at `MAX_UNDO_DEPTH`; oldest entries drop first.
    fn savepoint(&mut self) {
        self.undo_stack.push(self.session.clone());
        if self.undo_stack.len() > MAX_UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
        self.dirty = true;
    }

    fn newest_shed_id(&self) -> Option<ShedId> {
        self.session.sheds().last().map(|b| b.id)
    }

    fn shed_ids_in_order(&self) -> Vec<ShedId> {
        self.session.sheds().map(|b| b.id).collect()
    }

    fn move_cursor(&mut self, delta: i32) {
        let ids = self.shed_ids_in_order();
        if ids.is_empty() {
            return;
        }
        match self.session.cursor() {
            None => {
                // Scratch box is "selected" — `↑` walks back to the
                // last real shed; `↓` is a no-op (already at end).
                if delta < 0 {
                    self.session.set_cursor(ids.last().copied());
                    self.reset_pipeline_cursor();
                }
            }
            Some(cur) => {
                let Some(i) = ids.iter().position(|id| *id == cur) else {
                    return;
                };
                let target = i as i32 + delta;
                if target >= ids.len() as i32 {
                    // `↓` past the last real shed parks on the
                    // scratch box but *stays in ShedCursor*, so `↑`
                    // walks back instead of jumping into prompt
                    // history. To start typing the user activates
                    // the scratch box via Enter / Space / `e`.
                    self.session.set_cursor(None);
                    self.command_focused = false;
                    return;
                }
                let new_i = target.clamp(0, ids.len() as i32 - 1) as usize;
                self.session.set_cursor(Some(ids[new_i]));
                self.reset_pipeline_cursor();
            }
        }
    }

    fn reset_pipeline_cursor(&mut self) {
        self.command_focused = false;
        self.pipeline_cursor = self
            .session
            .cursor()
            .and_then(|id| self.session.shed(id))
            .map(|b| b.pipeline.len())
            .unwrap_or(0);
    }

    fn cursor_shed_pipeline_len(&self) -> Option<usize> {
        self.session
            .cursor()
            .and_then(|id| self.session.shed(id))
            .map(|b| b.pipeline.len())
    }
}

pub async fn run(notebook: Option<PathBuf>) -> io::Result<()> {
    let mut terminal = enter_tui();
    let result = app_loop(&mut terminal, notebook).await;
    leave_tui();
    result
}

/// Initialise ratatui *and* enable mouse capture. Mirrors
/// `ratatui::init()` but also turns on crossterm mouse events so
/// clicks on per-shed `×` buttons (and any future click targets) are
/// delivered to the event loop. Best-effort: if the terminal doesn't
/// support mouse capture, `execute!` silently fails and we still
/// return a usable TUI.
fn enter_tui() -> DefaultTerminal {
    let terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture);
    terminal
}

/// Tear down what [`enter_tui`] set up. Must be paired one-to-one.
fn leave_tui() {
    let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
}

async fn app_loop(terminal: &mut DefaultTerminal, notebook: Option<PathBuf>) -> io::Result<()> {
    let mut app = App::new();
    if let Some(path) = notebook {
        // Best-effort: if the file doesn't exist yet, treat the path as a
        // not-yet-saved location (so Ctrl-S writes there). Otherwise load
        // the notebook into the session.
        if path.exists() {
            load_from_path(&mut app, &path);
        } else {
            app.notebook_path = Some(path);
        }
    }
    loop {
        if let Some(req) = app.pending_handover.take() {
            perform_handover(terminal, &mut app, req).await?;
        }
        if let Some(req) = app.pending_rerun.take() {
            perform_rerun(&mut app, req).await;
        }
        if app.chain_in_flight.is_none()
            && let Some(next) = app.pending_run_chain.pop_front()
        {
            app.chain_in_flight = Some(next);
            perform_run_in_place(&mut app, next).await;
        }
        // Drive every tab so background tabs keep streaming output and
        // reaping finished children; advance_run_chain runs only for the
        // active tab (chain dispatch needs perform_run_in_place which is
        // tied to the active state).
        drive_all_tabs(&mut app).await;
        advance_run_chain(&mut app);
        let mut regions: Vec<ClickRegion> = Vec::new();
        let mut bodies: Vec<BodyRegion> = Vec::new();
        terminal.draw(|f| render::draw(f, &app, &mut regions, &mut bodies))?;
        app.click_regions = regions;
        app.body_regions = bodies;
        if app.quit {
            return Ok(());
        }
        if event::poll(POLL_TIMEOUT)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.flash = None;
                    handle_key(&mut app, key).await;
                }
                Event::Mouse(me) => {
                    handle_mouse(&mut app, me);
                }
                _ => {}
            }
        }
    }
}

async fn perform_rerun(app: &mut App, req: RerunRequest) {
    if req.force_fullscreen || needs_fullscreen(&req.argv) {
        app.pending_handover = Some(HandoverRequest {
            argv: req.argv,
            reuse_shed: None,
        });
        return;
    }

    // Builtins are state changes; re-running them as a copy doesn't make
    // sense, so just dispatch through the same path as a typed command.
    match req.argv[0].as_str() {
        "cd" => {
            run_cd_builtin(app, &req.argv);
            return;
        }
        "exit" | "quit" => {
            run_exit_builtin(app);
            return;
        }
        "export" => {
            run_export_builtin(app, &req.argv);
            return;
        }
        "unset" => {
            run_unset_builtin(app, &req.argv);
            return;
        }
        _ => {}
    }

    app.savepoint();
    let id = app.session.add_shed(req.argv.clone());
    if !req.pipeline.is_empty()
        && let Some(shed) = app.session.shed_mut(id)
    {
        shed.pipeline = req.pipeline;
    }
    match exec::spawn_command(req.argv, CAPTURE_CAP).await {
        Ok((handle, killer, chunks)) => {
            app.running.insert(
                id,
                RunningCommand {
                    handle,
                    killer,
                    chunks,
                    stream_buf: BytesMut::new(),
                },
            );
        }
        Err(e) => {
            app.session.set_state(id, ShedState::Failed(e.to_string()));
        }
    }
}

/// A reference to a shed from another shed's argv. shed's syntax for
/// "snapshot the output of this shed":
///
/// - `@name` — by pinned name. Resolved via `Session::lookup_by_name`.
/// - `%N`   — by monotonic id (visible in every shed's title).
///
/// Either form may appear as a *single-token* argv; mixing with other
/// args treats the whole thing as a regular external command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShedRef {
    Name(String),
    Id(ShedId),
}

/// Parse a single argv token as a [`ShedRef`]. The bare prefixes `@`
/// and `%` alone, or `%foo` (not all digits), return `None`.
fn parse_shed_ref(token: &str) -> Option<ShedRef> {
    if let Some(name) = token.strip_prefix('@')
        && !name.is_empty()
    {
        return Some(ShedRef::Name(name.to_string()));
    }
    if let Some(rest) = token.strip_prefix('%')
        && let Ok(n) = rest.parse::<u64>()
    {
        return Some(ShedRef::Id(ShedId(n)));
    }
    None
}

/// If `argv` is exactly one token that parses as a [`ShedRef`], return
/// it; otherwise `None`. Used at the dispatch sites that fork on
/// "is this a snapshot reference?" before treating argv as a regular
/// external command.
fn shed_ref_of(argv: &[String]) -> Option<ShedRef> {
    if argv.len() != 1 {
        return None;
    }
    parse_shed_ref(&argv[0])
}

/// True if `argv` is a single token that parses as a [`ShedRef`].
/// Convenience wrapper around [`shed_ref_of`]; only used in tests
/// since the runtime sites need the parsed `ShedRef` itself.
#[cfg(test)]
fn is_shed_ref(argv: &[String]) -> bool {
    shed_ref_of(argv).is_some()
}

/// Resolve a [`ShedRef`] to the concrete `ShedId` in `session`, or
/// `None` if the referenced shed isn't present.
fn resolve_shed_ref(session: &Session, r: &ShedRef) -> Option<ShedId> {
    match r {
        ShedRef::Name(name) => session.lookup_by_name(name),
        ShedRef::Id(id) => session.shed(*id).map(|_| *id),
    }
}

/// Render a [`ShedRef`] back to its argv-token form.
fn shed_ref_display(r: &ShedRef) -> String {
    match r {
        ShedRef::Name(n) => format!("@{n}"),
        ShedRef::Id(id) => format!("%{}", id.0),
    }
}

/// One `${…}` reference parsed out of an argv token. The parser allows
/// three shapes — see [`parse_interpolations`] for the grammar:
///
/// - `${name}` — the *own* shed's declared output `name`.
/// - `${@source}` or `${%N}` — the upstream source's implicit stdout
///   (trimmed). `output` is `None`.
/// - `${@source.name}` or `${%N.name}` — the upstream source's named
///   output `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InterpRef {
    Own(String),
    Source {
        source: ShedRef,
        output: Option<String>,
    },
}

/// One slice of an argv token after interpolation parsing — either
/// literal text or a single `${…}` reference. Tokens are reassembled
/// from these parts at spawn time.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenPart {
    Literal(String),
    Interp(InterpRef),
}

/// Parse all `${…}` references in `s`. Returns a `Vec<TokenPart>` that
/// reassembles to `s` when each `Interp` is resolved to its value.
/// Errors out (as a human-readable string) on malformed references —
/// unterminated `${`, empty `${}`, or syntactically invalid contents.
fn parse_interpolations(s: &str) -> Result<Vec<TokenPart>, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut literal_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Emit accumulated literal first.
            if i > literal_start {
                out.push(TokenPart::Literal(s[literal_start..i].to_string()));
            }
            // Find the matching `}`. Nested braces aren't supported.
            let body_start = i + 2;
            let Some(end_rel) = s[body_start..].find('}') else {
                return Err(format!("unterminated `${{` in {s:?}"));
            };
            let body = &s[body_start..body_start + end_rel];
            if body.is_empty() {
                return Err(format!("empty `${{}}` in {s:?}"));
            }
            out.push(TokenPart::Interp(parse_interp_body(body)?));
            i = body_start + end_rel + 1;
            literal_start = i;
        } else {
            i += 1;
        }
    }
    if literal_start < bytes.len() {
        out.push(TokenPart::Literal(s[literal_start..].to_string()));
    }
    Ok(out)
}

fn parse_interp_body(body: &str) -> Result<InterpRef, String> {
    // ${name} — own output.
    if !body.starts_with('@') && !body.starts_with('%') {
        if !is_valid_ident(body) {
            return Err(format!(
                "invalid output name `${{{body}}}` (expected letters/digits/underscore)"
            ));
        }
        return Ok(InterpRef::Own(body.to_string()));
    }
    // ${@source} / ${@source.field} / ${%N} / ${%N.field}
    let (head, field) = match body.split_once('.') {
        Some((h, f)) => (h, Some(f)),
        None => (body, None),
    };
    let source = parse_shed_ref(head).ok_or_else(|| format!("invalid source ref `${{{body}}}`"))?;
    if let Some(f) = field
        && !is_valid_ident(f)
    {
        return Err(format!("invalid output name in `${{{body}}}`"));
    }
    Ok(InterpRef::Source {
        source,
        output: field.map(String::from),
    })
}

fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Walk an argv looking for every distinct `${@source.…}` /
/// `${%N.…}` reference and return the unique [`ShedRef`]s. Used by
/// the run-chain machinery to discover dependencies inferred from
/// interpolations. Malformed interpolations are silently skipped here
/// (the real error fires at spawn time when [`resolve_argv`] runs).
fn argv_interp_sources(argv: &[String]) -> Vec<ShedRef> {
    let mut seen = Vec::new();
    for token in argv {
        let Ok(parts) = parse_interpolations(token) else {
            continue;
        };
        for part in parts {
            if let TokenPart::Interp(InterpRef::Source { source, .. }) = part
                && !seen.contains(&source)
            {
                seen.push(source);
            }
        }
    }
    seen
}

/// Resolve every `${…}` in `argv` to its value, using `own` for
/// own-output references and `session` for source references. Returns
/// the fully-resolved argv suitable for handing to exec, or an error
/// naming the unresolved reference (undefined output, missing source,
/// source not yet succeeded, …).
fn resolve_argv(
    argv: &[String],
    own: &std::collections::HashMap<String, String>,
    session: &Session,
) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(argv.len());
    for token in argv {
        let parts = parse_interpolations(token)?;
        let mut buf = String::new();
        for part in parts {
            match part {
                TokenPart::Literal(s) => buf.push_str(&s),
                TokenPart::Interp(InterpRef::Own(name)) => match own.get(&name) {
                    Some(v) => buf.push_str(v),
                    None => {
                        return Err(format!("undefined own output `${{{name}}}` in argv"));
                    }
                },
                TokenPart::Interp(InterpRef::Source { source, output }) => {
                    let value = lookup_source_output(session, &source, output.as_deref())?;
                    buf.push_str(&value);
                }
            }
        }
        out.push(buf);
    }
    Ok(out)
}

/// Pre-spawn setup: clear the shed's runtime `output_values`, seed
/// `Literal` outputs with their declared strings, generate a fresh
/// `TempPath` for each declared temp-path output, then resolve every
/// `${…}` in `argv` against the freshly-seeded own outputs + the
/// session's other sheds. Returns the resolved argv ready for exec.
///
/// Errors (returned as `Err`, caller stamps the shed `Failed`):
/// undefined own output, unknown source, source not yet completed
/// successfully, undeclared source output, malformed `${…}` syntax.
fn prepare_outputs_and_resolve(
    app: &mut App,
    id: ShedId,
    argv: &[String],
) -> Result<Vec<String>, String> {
    // Phase 1: clear + seed own output_values.
    if let Some(shed) = app.session.shed_mut(id) {
        shed.output_values.clear();
        for (name, spec) in &shed.outputs {
            let value = match spec {
                OutputSpec::Literal(s) => s.clone(),
                OutputSpec::TempPath => generate_temp_path(id, name),
            };
            shed.output_values.insert(name.clone(), value);
        }
    }
    // Phase 2: resolve argv. Pull own-output map out by value because
    // resolve_argv needs both `own` and `&session` and we can't borrow
    // through `&mut app` twice.
    let own = app
        .session
        .shed(id)
        .map(|s| s.output_values.clone())
        .unwrap_or_default();
    resolve_argv(argv, &own, &app.session)
}

/// Generate a unique temp path for a shed's `TempPath` output. Includes
/// the shed id, the output name, and nanos-since-epoch so concurrent
/// or repeated runs don't collide. The file is *not* created; the
/// spawned command is expected to write to the path.
fn generate_temp_path(id: ShedId, output_name: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir();
    dir.join(format!("shed-{}-{}-{}", id.0, output_name, nanos))
        .to_string_lossy()
        .into_owned()
}

/// Look up an upstream shed's output for `${@source.field}` /
/// `${%N.field}` / `${@source}` interpolation. The unnamed form returns
/// the source's trimmed stdout (mirrors shell `$(...)`); named forms
/// read from `Shed::output_values`. Either form requires the source to
/// have completed successfully — a missing source, an unsuccessful
/// source, or an undeclared output name returns a structured error.
fn lookup_source_output(
    session: &Session,
    source: &ShedRef,
    output: Option<&str>,
) -> Result<String, String> {
    let label = shed_ref_display(source);
    let Some(id) = resolve_shed_ref(session, source) else {
        return Err(format!("no shed at {label}"));
    };
    let Some(shed) = session.shed(id) else {
        return Err(format!("{label} missing from session"));
    };
    // Both implicit and named outputs require the source to have
    // succeeded — otherwise the value isn't trustworthy.
    if !matches!(shed.state, ShedState::Done(0) | ShedState::Snapshotted) {
        return Err(format!("{label} hasn't completed successfully"));
    }
    match output {
        None => {
            // Implicit ${@source} — trimmed stdout.
            let Some(cap) = shed.capture.as_ref() else {
                return Err(format!("{label} has no captured output"));
            };
            Ok(String::from_utf8_lossy(&cap.stdout).trim().to_string())
        }
        Some(name) => match shed.output_values.get(name) {
            Some(v) => Ok(v.clone()),
            None => Err(format!("{label} doesn't define output `{name}`")),
        },
    }
}

/// `Done`/`Failed`/`Snapshotted` are terminal — the run-chain machinery
/// uses this to decide when to advance. `Idle` and `Running` are not.
fn is_terminal_state(state: &ShedState) -> bool {
    matches!(
        state,
        ShedState::Done(_) | ShedState::Failed(_) | ShedState::Snapshotted
    )
}

/// Walk shed-ref deps (`@name` and `%N`) to compute the run order for
/// `target`. Sources that are `Idle` or `Running` get added before the
/// target so the chain either runs them first or simply waits on them.
/// `Done`/`Failed` sources are not re-run — the snapshot will use the
/// existing capture or fail with a clear error. Cycles are guarded via
/// `visited`.
fn build_run_chain(
    session: &Session,
    target: ShedId,
    visited: &mut HashSet<ShedId>,
) -> Vec<ShedId> {
    if !visited.insert(target) {
        return Vec::new();
    }
    let mut chain = Vec::new();
    let shed = match session.shed(target) {
        Some(b) => b,
        None => return chain,
    };
    // Snapshot data dep: argv is `@name` / `%N`.
    if let Some(r) = shed_ref_of(&shed.argv)
        && let Some(src_id) = resolve_shed_ref(session, &r)
        && let Some(src) = session.shed(src_id)
        && matches!(src.state, ShedState::Idle | ShedState::Running)
    {
        let mut sub = build_run_chain(session, src_id, visited);
        chain.append(&mut sub);
    }
    // Interpolation deps: any `${@source.…}` / `${%N.…}` in argv. Only
    // queue the source as a prereq if it hasn't already completed
    // successfully — a source already in `Done(0)`/`Snapshotted` has
    // valid output_values that downstream can read directly, no need to
    // re-run.
    for r in argv_interp_sources(&shed.argv) {
        let Some(src_id) = resolve_shed_ref(session, &r) else {
            continue;
        };
        let Some(src) = session.shed(src_id) else {
            continue;
        };
        if matches!(src.state, ShedState::Done(0) | ShedState::Snapshotted) {
            continue;
        }
        let mut sub = build_run_chain(session, src_id, visited);
        chain.append(&mut sub);
    }
    chain.push(target);
    chain
}

/// Walk *downward* from `source` to find every shed that references it
/// (recursively, so dependents-of-dependents are included). Output is
/// in BFS order so a downstream rebuild runs closest first. A
/// dependent is one whose argv is `@<source.name>` (if source is
/// pinned) or `%<source.id>`.
fn collect_dependents_recursive(
    session: &Session,
    source: ShedId,
    out: &mut Vec<ShedId>,
    visited: &mut HashSet<ShedId>,
) {
    if !visited.insert(source) {
        return;
    }
    let Some(src_shed) = session.shed(source) else {
        return;
    };
    let name_ref = src_shed.name.as_ref().map(|n| ShedRef::Name(n.clone()));
    let id_ref = ShedRef::Id(source);
    let direct: Vec<ShedId> = session
        .sheds()
        .filter(|b| {
            // Snapshot reference: argv is exactly `@source.name` / `%N`.
            let snapshot_match = b.argv.len() == 1
                && parse_shed_ref(&b.argv[0])
                    .is_some_and(|r| r == id_ref || name_ref.as_ref().is_some_and(|n| &r == n));
            if snapshot_match {
                return true;
            }
            // Interpolation reference: `${@source.…}` / `${%N.…}` anywhere.
            argv_interp_sources(&b.argv)
                .iter()
                .any(|r| r == &id_ref || name_ref.as_ref().is_some_and(|n| r == n))
        })
        .map(|b| b.id)
        .collect();
    for id in &direct {
        if !out.contains(id) {
            out.push(*id);
        }
    }
    for id in direct {
        collect_dependents_recursive(session, id, out, visited);
    }
}

/// Like `queue_run_chain`, but additionally re-runs any `@-ref`
/// dependents of `target` after `target` itself completes. Used by the
/// in-place command editor: changing a source's argv makes any snapshot
/// of it stale, so dependents need a refresh too.
fn queue_edit_chain(app: &mut App, target: ShedId) {
    let mut chain: Vec<ShedId> = Vec::new();
    let mut visited = HashSet::new();
    chain.extend(build_run_chain(&app.session, target, &mut visited));

    let mut down_visited: HashSet<ShedId> = HashSet::new();
    let mut downward: Vec<ShedId> = Vec::new();
    collect_dependents_recursive(&app.session, target, &mut downward, &mut down_visited);
    chain.extend(downward);

    let n = chain.len();
    if n > 1 {
        let dependents = n - 1;
        let plural = if dependents == 1 { "shed" } else { "sheds" };
        app.flash = Some(format!(
            "re-running %{} and {dependents} dependent {plural}",
            target.0
        ));
    }

    for id in chain {
        if app.chain_in_flight != Some(id) && !app.pending_run_chain.contains(&id) {
            app.pending_run_chain.push_back(id);
        }
    }
}

fn open_cmd_edit(app: &mut App) {
    let Some(id) = app.session.cursor() else {
        app.flash = Some("no shed selected".into());
        return;
    };
    let Some(shed) = app.session.shed(id) else {
        return;
    };
    if shed.argv.is_empty() {
        app.flash = Some("nothing to edit".into());
        return;
    }
    let joined = shlex::try_join(shed.argv.iter().map(String::as_str))
        .unwrap_or_else(|_| shed.argv.join(" "));
    app.open_input(InputKind::CmdEdit, joined);
}

/// Apply the edited command to the cursor shed in place, then queue
/// the shed plus any pinned-name dependents for re-run.
fn commit_cmd_edit(app: &mut App) {
    let input = app.take_input_text();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        app.flash = Some("command required".into());
        return;
    }
    let Some(argv) = shlex::split(trimmed) else {
        app.flash = Some("unmatched quote".into());
        return;
    };
    if argv.is_empty() {
        return;
    }
    let Some(id) = app.session.cursor() else {
        return;
    };
    if app.session.shed(id).is_none() {
        return;
    }
    app.savepoint();
    if let Some(shed) = app.session.shed_mut(id) {
        shed.argv = argv;
    }
    app.command_focused = false;
    queue_edit_chain(app, id);
}

fn queue_run_chain(app: &mut App, target: ShedId) {
    let mut visited = HashSet::new();
    let chain = build_run_chain(&app.session, target, &mut visited);
    let n = chain.len();
    if n > 1 {
        let dep_count = n - 1;
        let label = if dep_count == 1 { "1 dep" } else { "deps" };
        app.flash = Some(format!(
            "running {dep_count} {label} first, then %{}",
            target.0
        ));
    }
    for id in chain {
        if app.chain_in_flight != Some(id) && !app.pending_run_chain.contains(&id) {
            app.pending_run_chain.push_back(id);
        }
    }
}

/// Called after `reap_completed` each tick: if the in-flight shed is
/// now terminal, clear the slot. If it failed, abort the rest of the
/// chain since dependents would just see a stale or missing source.
fn advance_run_chain(app: &mut App) {
    let Some(id) = app.chain_in_flight else {
        return;
    };
    let state = app.session.shed(id).map(|b| b.state.clone());
    let terminal = match &state {
        Some(s) => is_terminal_state(s),
        None => true, // shed was deleted mid-chain
    };
    if !terminal {
        return;
    }
    let failed = matches!(state, Some(ShedState::Failed(_)));
    if failed && !app.pending_run_chain.is_empty() {
        let n = app.pending_run_chain.len();
        app.pending_run_chain.clear();
        let plural = if n == 1 { "shed" } else { "sheds" };
        app.flash = Some(format!("skipped {n} dependent {plural} — prereq failed"));
    }
    app.chain_in_flight = None;
}

/// Output produced by [`snapshot_ref`]: either raw bytes (passthrough
/// for byte-stream sources) or a structured [`Value`] (parsed rows from
/// the source's pipeline, transported directly so downstream sheds
/// inherit the schema and column order without re-parsing).
#[derive(Debug, Clone)]
enum SnapshotOutput {
    Bytes(Vec<u8>),
    Structured(Value),
}

/// Apply the referenced shed's pipeline to its current capture and
/// return the result. Byte-stream sources pass through as
/// [`SnapshotOutput::Bytes`]; structured sources pass through as
/// [`SnapshotOutput::Structured`] so the downstream shed can start its
/// own pipeline from the typed value (no JSON round-trip, column order
/// preserved).
fn snapshot_ref(session: &Session, r: &ShedRef) -> Result<SnapshotOutput, String> {
    let label = shed_ref_display(r);
    let id = resolve_shed_ref(session, r).ok_or_else(|| format!("no shed at {label}"))?;
    let shed = session
        .shed(id)
        .ok_or_else(|| format!("{label} missing from session"))?;
    let capture = shed
        .capture
        .as_ref()
        .ok_or_else(|| format!("{label} has no captured output yet"))?;

    let value = match apply_pipeline(capture, &shed.pipeline) {
        Ok((v, _)) => v,
        Err(e) => return Err(format!("{label} pipeline error: {e}")),
    };

    Ok(match value {
        PipelineValue::Bytes(b) => SnapshotOutput::Bytes(b.to_vec()),
        PipelineValue::Structured(v) => SnapshotOutput::Structured(v),
    })
}

/// Run `snapshot_ref` and write the result onto `id` as a synthetic
/// capture. Used both at create time (typing `@name` or `%N`) and on
/// re-run (Space on a snapshot shed). When the source produced
/// structured rows the snapshot capture carries them in
/// [`Capture::structured`] so the downstream pipeline starts from the
/// typed value directly.
fn populate_snapshot(app: &mut App, id: ShedId, r: &ShedRef) {
    let started_at = Instant::now();
    match snapshot_ref(&app.session, r) {
        Ok(output) => {
            let (stdout, structured) = match output {
                SnapshotOutput::Bytes(b) => (Bytes::from(b), None),
                SnapshotOutput::Structured(v) => (Bytes::new(), Some(v)),
            };
            let capture = Capture {
                stdout,
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at,
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: true,
                structured,
            };
            app.session.set_capture(id, capture);
            app.session.set_state(id, ShedState::Done(0));
        }
        Err(e) => {
            if let Some(b) = app.session.shed_mut(id) {
                b.capture = None;
            }
            app.session.set_state(id, ShedState::Failed(e));
        }
    }
}

/// Append a new snapshot shed referencing `@name` or `%N` and queue it
/// (along with any deps) on the run chain. The actual snapshot runs from
/// `perform_run_in_place` so the source's pipeline output is fresh.
fn spawn_ref_snapshot(app: &mut App, r: &ShedRef) {
    app.savepoint();
    let argv = vec![shed_ref_display(r)];
    let id = app.session.add_shed(argv);
    app.session.set_state(id, ShedState::Idle);
    queue_run_chain(app, id);
}

/// Remove the cursor shed from the session. Kills any running child
/// for that id, advances the cursor to the next surviving shed (or the
/// previous one if there is no next), or returns to the prompt if the
/// session is now empty.
fn delete_shed_at_cursor(app: &mut App) {
    let Some(id) = app.session.cursor() else {
        app.flash = Some("no shed selected".into());
        return;
    };
    delete_shed(app, id);
}

/// Remove a specific shed by id. Kills its running command (if any),
/// pulls it from the run chain, and removes it from the session. If
/// the deleted shed was under the cursor, advance the cursor to its
/// next sibling (or previous, or return to prompt if the session is
/// now empty); otherwise leave the cursor alone.
fn delete_shed(app: &mut App, id: ShedId) {
    if app.session.shed(id).is_none() {
        return;
    }
    app.savepoint();
    if let Some(mut cmd) = app.running.remove(&id) {
        let _ = cmd.killer.kill();
        cmd.handle.abort();
    }
    app.pending_run_chain.retain(|x| *x != id);
    if app.chain_in_flight == Some(id) {
        app.chain_in_flight = None;
    }
    let was_cursor = app.session.cursor() == Some(id);
    let ids = app.shed_ids_in_order();
    let next_cursor = ids.iter().position(|x| *x == id).and_then(|i| {
        ids.get(i + 1).copied().or_else(|| {
            if i == 0 {
                None
            } else {
                ids.get(i - 1).copied()
            }
        })
    });
    let removed = app.session.remove_shed(id);
    if removed.is_none() {
        return;
    }
    if was_cursor {
        app.session.set_cursor(next_cursor);
        if next_cursor.is_some() {
            app.reset_pipeline_cursor();
        } else {
            app.focus = Focus::Prompt;
        }
    }
    app.flash = Some(format!("deleted %{}", id.0));
}

/// Queue a re-spawn of the cursor's shed argv along with any unrun
/// `@-ref` dependencies, so deps run first. Idempotent: if the shed is
/// already running or already queued, this flashes a message instead.
fn run_cursor_shed_in_place(app: &mut App) {
    let Some(id) = app.session.cursor() else {
        app.flash = Some("no shed selected".into());
        return;
    };
    if app.running.contains_key(&id) {
        app.flash = Some(format!("%{} is still running", id.0));
        return;
    }
    if app.chain_in_flight == Some(id) || app.pending_run_chain.contains(&id) {
        app.flash = Some(format!("%{} already queued", id.0));
        return;
    }
    queue_run_chain(app, id);
}

async fn perform_run_in_place(app: &mut App, id: ShedId) {
    // The chain machinery may queue an already-running shed (e.g. when a
    // user requests a snapshot of @logs while @logs is running) — that's a
    // wait, not a re-spawn. Bail without touching capture or state.
    if app.running.contains_key(&id) {
        return;
    }
    let Some(shed) = app.session.shed(id) else {
        return;
    };
    let argv = shed.argv.clone();
    if argv.is_empty() {
        return;
    }

    if let Some(r) = shed_ref_of(&argv) {
        populate_snapshot(app, id, &r);
        return;
    }

    // Builtins: dispatch through their existing handlers so cd/export/unset
    // can take effect on shed's own state. They allocate fresh sheds rather
    // than reusing the cursor shed, matching prompt-spawn semantics.
    match argv[0].as_str() {
        "cd" => {
            run_cd_builtin(app, &argv);
            return;
        }
        "exit" | "quit" => {
            run_exit_builtin(app);
            return;
        }
        "export" => {
            run_export_builtin(app, &argv);
            return;
        }
        "unset" => {
            run_unset_builtin(app, &argv);
            return;
        }
        _ => {}
    }

    // Compute the shed's own output values (TempPath gets a fresh path
    // each spawn; Literal is its declared string), then resolve every
    // `${…}` interpolation in argv into a concrete string. Failures here
    // (undefined output, unresolved source, source not yet succeeded)
    // mark the shed Failed without spawning.
    let argv = match prepare_outputs_and_resolve(app, id, &argv) {
        Ok(a) => a,
        Err(e) => {
            app.session.set_state(id, ShedState::Failed(e));
            return;
        }
    };

    if needs_fullscreen(&argv) {
        app.pending_handover = Some(HandoverRequest {
            argv,
            reuse_shed: Some(id),
        });
        return;
    }

    if let Some(b) = app.session.shed_mut(id) {
        b.capture = None;
        b.state = ShedState::Running;
    }
    match exec::spawn_command(argv, CAPTURE_CAP).await {
        Ok((handle, killer, chunks)) => {
            app.running.insert(
                id,
                RunningCommand {
                    handle,
                    killer,
                    chunks,
                    stream_buf: BytesMut::new(),
                },
            );
        }
        Err(e) => {
            app.session.set_state(id, ShedState::Failed(e.to_string()));
        }
    }
}

async fn perform_handover(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    req: HandoverRequest,
) -> io::Result<()> {
    leave_tui();

    let id = match req.reuse_shed {
        Some(existing) => existing,
        None => {
            app.savepoint();
            app.session.add_shed(req.argv.clone())
        }
    };
    let status_result = tokio::process::Command::new(&req.argv[0])
        .args(&req.argv[1..])
        .status()
        .await;

    *terminal = enter_tui();

    match status_result {
        Ok(status) => {
            app.session
                .set_state(id, ShedState::Done(status.code().unwrap_or(-1)));
        }
        Err(e) => {
            app.session
                .set_state(id, ShedState::Failed(format!("spawn: {e}")));
        }
    }
    if req.reuse_shed.is_some() {
        app.flash = Some(format!("%{} switched to fullscreen mode", id.0));
    }
    Ok(())
}

/// Pull every pending stream chunk from each running command into its
/// `stream_buf`, and mirror the (possibly grown) buffer onto the
/// shed's `capture.stdout` so the renderer sees streaming output.
///
/// While streaming, the shed's capture has `exit_code: None` and
/// `finished_at: None` — those land when [`reap_completed`] replaces
/// the partial capture with the final one from the reader task.
fn drain_streams(app: &mut App) -> bool {
    let ids: Vec<ShedId> = app.running.keys().copied().collect();
    let mut any_progress = false;
    for id in ids {
        let Some(cmd) = app.running.get_mut(&id) else {
            continue;
        };
        let mut got_bytes = false;
        while let Ok(chunk) = cmd.chunks.try_recv() {
            cmd.stream_buf.extend_from_slice(&chunk);
            got_bytes = true;
        }
        if !got_bytes {
            continue;
        }
        any_progress = true;
        // Snapshot the running buffer as a fresh Bytes and mirror onto
        // the shed. Cost is O(N) per snapshot; render frequency caps
        // total work, and this is the simplest way to keep the
        // renderer (which reads `shed.capture`) unchanged.
        let snapshot = Bytes::copy_from_slice(&cmd.stream_buf);
        let started_at = app
            .session
            .shed(id)
            .and_then(|b| b.capture.as_ref().map(|c| c.started_at))
            .unwrap_or_else(Instant::now);
        let partial = Capture {
            stdout: snapshot,
            stderr: Bytes::new(),
            exit_code: None,
            started_at,
            finished_at: None,
            truncated: false,
            snapshotted: false,
            structured: None,
        };
        app.session.set_capture(id, partial);
    }
    any_progress
}

async fn reap_completed(app: &mut App) -> bool {
    let finished_ids: Vec<ShedId> = app
        .running
        .iter()
        .filter(|(_, c)| c.handle.is_finished())
        .map(|(id, _)| *id)
        .collect();
    let any_progress = !finished_ids.is_empty();
    for id in finished_ids {
        let Some(cmd) = app.running.remove(&id) else {
            continue;
        };
        match cmd.handle.await {
            Ok(Ok(CaptureOutcome::Captured(capture))) => {
                let exit = capture.exit_code.unwrap_or(-1);
                app.session.set_capture(id, capture);
                app.session.set_state(id, ShedState::Done(exit));
            }
            Ok(Ok(CaptureOutcome::NeededFullscreen)) => {
                let argv = app
                    .session
                    .shed(id)
                    .map(|b| b.argv.clone())
                    .unwrap_or_default();
                if argv.is_empty() {
                    app.session.set_state(
                        id,
                        ShedState::Failed("alt-screen detected, argv missing".into()),
                    );
                } else {
                    app.pending_handover = Some(HandoverRequest {
                        argv,
                        reuse_shed: Some(id),
                    });
                }
            }
            Ok(Err(e)) => {
                app.session.set_state(id, ShedState::Failed(e.to_string()));
            }
            Err(e) => {
                app.session
                    .set_state(id, ShedState::Failed(format!("task error: {e}")));
            }
        }
        app.session.evict_to_fit();
    }
    any_progress
}

async fn handle_key(app: &mut App, key: KeyEvent) {
    // Context menu, when open, owns the keyboard.
    if app.context_menu.is_some() {
        handle_context_menu_key(app, key);
        return;
    }
    // Exit confirmation takes priority over everything: y / n / c (cancel).
    if app.exit_prompt.is_some() {
        handle_exit_prompt_key(app, key);
        return;
    }
    // Notebook save/open input bars are overlaid on top of any focus.
    if app.is_input(InputKind::Save) {
        handle_save_input_key(app, key);
        return;
    }
    if app.is_input(InputKind::Open) {
        handle_open_input_key(app, key);
        return;
    }
    if app.is_input(InputKind::RenameTab) {
        handle_rename_tab_input_key(app, key);
        return;
    }
    // NoteEdit consumes its own keys so Ctrl-S commits the note rather
    // than triggering the global save-notebook binding.
    if app.focus == Focus::NoteEdit {
        handle_note_edit_key(app, key);
        return;
    }
    // Global tab-management bindings — fire from any focus.
    if try_handle_tab_key(app, key) {
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('d') => {
                request_quit(app);
                return;
            }
            KeyCode::Char('c') => {
                if app.focus == Focus::ShedCursor && cancel_at_cursor(app) {
                    return;
                }
                request_quit(app);
                return;
            }
            KeyCode::Char('p') => {
                open_palette(app);
                return;
            }
            KeyCode::Char('s') => {
                begin_save(app);
                return;
            }
            KeyCode::Char('o') => {
                begin_open(app);
                return;
            }
            KeyCode::Char('z') => {
                undo(app);
                return;
            }
            KeyCode::Char('y') => {
                redo(app);
                return;
            }
            _ => {}
        }
    }
    match app.focus {
        Focus::Prompt => handle_prompt_key(app, key).await,
        Focus::ShedCursor => handle_cursor_key(app, key),
        Focus::EditShed => handle_edit_shed_key(app, key),
        Focus::FilterEdit => handle_filter_edit_key(app, key),
        Focus::ShedExpand => handle_shed_expand_key(app, key),
        Focus::EnvEdit => handle_env_edit_key(app, key),
        Focus::Palette => handle_palette_key(app, key),
        Focus::NoteEdit => handle_note_edit_key(app, key),
        Focus::AliasManage => handle_alias_manage_key(app, key),
    }
}

/// Pop the latest savepoint and restore it as the active session. The
/// previous current session is pushed onto the redo stack so the user
/// can roll forward again. Captures and run-state of any sheds that
/// exist in both versions are *preserved* — undo only reverts the
/// structural fields (argv, name, pipeline, pre/post-text, cursor) so
/// you don't lose freshly produced output. Sheds that the snapshot
/// resurrects come back with whatever capture they had at snapshot time.
fn undo(app: &mut App) {
    let Some(prev) = app.undo_stack.pop() else {
        app.flash = Some("nothing to undo".into());
        return;
    };
    let current = app.session.clone();
    app.redo_stack.push(current);
    apply_snapshot(app, prev);
    app.dirty = true;
    app.flash = Some("undid last change".into());
}

fn redo(app: &mut App) {
    let Some(next) = app.redo_stack.pop() else {
        app.flash = Some("nothing to redo".into());
        return;
    };
    let current = app.session.clone();
    app.undo_stack.push(current);
    apply_snapshot(app, next);
    app.dirty = true;
    app.flash = Some("redid".into());
}

/// Replace `app.session` with `snap`, then patch live runtime state
/// (capture, run-state, last_touched) from the previous current onto
/// any shed whose id survives in the restored session. Finishes by
/// sanitizing app-level state that may now reference vanished sheds.
fn apply_snapshot(app: &mut App, snap: Session) {
    let prev = std::mem::replace(&mut app.session, snap);
    let ids: Vec<ShedId> = app.session.sheds().map(|b| b.id).collect();
    for id in ids {
        if let Some(prev_shed) = prev.shed(id) {
            let cap = prev_shed.capture.clone();
            let st = prev_shed.state.clone();
            let lt = prev_shed.last_touched;
            if let Some(restored) = app.session.shed_mut(id) {
                restored.capture = cap;
                restored.state = st;
                restored.last_touched = lt;
            }
        }
    }
    sanitize_after_restore(app);
}

/// Drop app-level references to sheds that may not exist after a
/// snapshot restore: pending run-chain entries, the in-flight chain
/// slot, the cursor, child processes whose sheds vanished. Also
/// rebases `pipeline_cursor` on the (possibly new) cursor shed.
fn sanitize_after_restore(app: &mut App) {
    if let Some(id) = app.session.cursor()
        && app.session.shed(id).is_none()
    {
        app.session.set_cursor(app.newest_shed_id());
    }
    app.command_focused = false;
    app.reset_pipeline_cursor();
    app.pending_run_chain.clear();
    app.chain_in_flight = None;
    let session_ids: HashSet<ShedId> = app.session.sheds().map(|b| b.id).collect();
    let orphans: Vec<ShedId> = app
        .running
        .keys()
        .copied()
        .filter(|id| !session_ids.contains(id))
        .collect();
    for id in orphans {
        if let Some(mut cmd) = app.running.remove(&id) {
            let _ = cmd.killer.kill();
            cmd.handle.abort();
        }
    }
}

/// Quit if clean; otherwise show the save-changes confirmation. "Dirty"
/// here means *the pinned sheds have changed since the last save/load*
/// — unpinned-shed edits are scratch work the user can lose silently.
///
/// When more than one tab is open this closes the active tab instead of
/// quitting — Ctrl-D / `exit` / `quit` is shell-style "close this view",
/// and tabs are independent views. The save-changes prompt only fires
/// for the last tab (where there's nowhere else to land); closing an
/// earlier tab is a deliberate discard, like closing a shell session in
/// a multiplexer.
fn request_quit(app: &mut App) {
    if app.tabs.len() > 1 {
        app.close_active_tab();
        return;
    }
    if has_unsaved_pinned_changes(app) {
        app.exit_prompt = Some(ExitPrompt::Confirm);
    } else {
        app.quit = true;
    }
}

/// JSON-serialised snapshot of the *pinned* sheds in `session`. Used to
/// detect whether the user has made changes the notebook would persist
/// to a different on-disk state.
fn pinned_entries_json(session: &Session) -> String {
    let entries: Vec<NotebookEntry> = session
        .sheds()
        .filter(|s| s.name.is_some())
        .map(|s| NotebookEntry::Command {
            argv: s.argv.clone(),
            name: s.name.clone(),
            pipeline: s.pipeline.clone(),
            pre_text: s.pre_text.clone(),
            post_text: s.post_text.clone(),
            outputs: s.outputs.clone(),
        })
        .collect();
    serde_json::to_string(&entries).unwrap_or_default()
}

fn has_unsaved_pinned_changes(app: &App) -> bool {
    pinned_entries_json(&app.session) != app.saved_pinned_json
}

/// Open the save input bar — or, if a notebook path is already bound,
/// save immediately and flash the result.
fn begin_save(app: &mut App) {
    if let Some(path) = app.notebook_path.clone() {
        save_to_path(app, &path);
        return;
    }
    app.open_input(InputKind::Save, String::new());
}

/// Always open the input bar; the user must type or paste a path.
fn begin_open(app: &mut App) {
    let initial = app
        .notebook_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    app.open_input(InputKind::Open, initial);
}

fn save_to_path(app: &mut App, path: &std::path::Path) {
    let nb = Notebook::from_session(&app.session);
    match nb.save(path) {
        Ok(()) => {
            app.notebook_path = Some(path.to_path_buf());
            app.dirty = false;
            app.saved_pinned_json = pinned_entries_json(&app.session);
            app.flash = Some(format!("saved → {}", path.display()));
        }
        Err(e) => {
            app.flash = Some(format!("save failed: {e}"));
        }
    }
}

fn load_from_path(app: &mut App, path: &std::path::Path) {
    match Notebook::load(path) {
        Ok(nb) => {
            // Replace the session entirely. Cancel any running children
            // first so we don't leak handles when their sheds vanish.
            for (_, mut cmd) in app.running.drain() {
                let _ = cmd.killer.kill();
                cmd.handle.abort();
            }
            app.pending_run_chain.clear();
            app.chain_in_flight = None;
            app.session = Session::new();
            nb.apply_to_session(&mut app.session);
            app.notebook_path = Some(path.to_path_buf());
            app.dirty = false;
            app.saved_pinned_json = pinned_entries_json(&app.session);
            app.session.set_cursor(app.newest_shed_id());
            app.reset_pipeline_cursor();
            // Refocus on the loaded sheds if any exist; otherwise stay
            // on the prompt.
            app.focus = if app.session.cursor().is_some() {
                Focus::ShedCursor
            } else {
                Focus::Prompt
            };
            let n = app.session.sheds().count();
            app.flash = Some(format!("loaded {n} shed(s) from {}", path.display()));
        }
        Err(e) => {
            app.flash = Some(format!("open failed: {e}"));
        }
    }
}

fn handle_save_input_key(app: &mut App, key: KeyEvent) {
    let outcome = {
        let (t, c) = app.input_mut().expect("input bar open");
        handle_text_input(t, c, &key)
    };
    match outcome {
        InputOutcome::Cancel => {
            app.close_input();
            // If the save was triggered by the exit prompt and the user
            // bailed out, drop the exit prompt rather than continuing to
            // hold them hostage.
            if app.exit_prompt == Some(ExitPrompt::AwaitingPath) {
                app.exit_prompt = None;
            }
        }
        InputOutcome::Commit => {
            let path_str = app.take_input_text();
            let trimmed = path_str.trim();
            if trimmed.is_empty() {
                app.flash = Some("path required".into());
                return;
            }
            let path = expand_tilde(trimmed);
            save_to_path(app, &path);
            // If we were saving on exit, complete the quit now that the
            // file is on disk (or fall through if save failed).
            if app.exit_prompt == Some(ExitPrompt::AwaitingPath) && !has_unsaved_pinned_changes(app)
            {
                app.exit_prompt = None;
                app.quit = true;
            }
        }
        InputOutcome::Continue => {}
    }
}

fn handle_open_input_key(app: &mut App, key: KeyEvent) {
    let outcome = {
        let (t, c) = app.input_mut().expect("input bar open");
        handle_text_input(t, c, &key)
    };
    match outcome {
        InputOutcome::Cancel => {
            app.close_input();
        }
        InputOutcome::Commit => {
            let path_str = app.take_input_text();
            let trimmed = path_str.trim();
            if trimmed.is_empty() {
                app.flash = Some("path required".into());
                return;
            }
            let path = expand_tilde(trimmed);
            load_from_path(app, &path);
        }
        InputOutcome::Continue => {}
    }
}

fn handle_exit_prompt_key(app: &mut App, key: KeyEvent) {
    let prompt = match app.exit_prompt {
        Some(p) => p,
        None => return,
    };
    if prompt == ExitPrompt::AwaitingPath {
        // Shouldn't normally land here — handle_save_input_key takes over
        // once save_input_mode is true. Treat any key as cancel.
        app.exit_prompt = None;
        return;
    }
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            // Save then quit. If no path is bound, switch to "awaiting path"
            // and reuse the save input bar.
            if let Some(path) = app.notebook_path.clone() {
                save_to_path(app, &path);
                if !has_unsaved_pinned_changes(app) {
                    app.exit_prompt = None;
                    app.quit = true;
                }
            } else {
                app.exit_prompt = Some(ExitPrompt::AwaitingPath);
                app.open_input(InputKind::Save, String::new());
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') => {
            app.exit_prompt = None;
            app.quit = true;
        }
        KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => {
            app.exit_prompt = None;
        }
        _ => {}
    }
}

fn open_palette(app: &mut App) {
    if app.focus == Focus::Palette {
        return;
    }
    app.palette_prev_focus = Some(app.focus);
    app.palette_state = Some(PaletteState::new());
    app.focus = Focus::Palette;
}

fn close_palette_cancelled(app: &mut App) {
    if let Some(prev) = app.palette_prev_focus.take() {
        app.focus = prev;
    }
    app.palette_state = None;
}

fn commit_palette(app: &mut App) {
    let Some(state) = app.palette_state.as_ref() else {
        return;
    };
    let matches = matches_for_input(&state.input, app);
    let cursor = state.cursor.min(matches.len().saturating_sub(1));
    let Some(action) = matches.get(cursor).copied() else {
        // No matches; cancel.
        close_palette_cancelled(app);
        return;
    };
    let handler = action.handler;
    // Tear down palette state but DON'T restore prev focus — handler decides.
    app.palette_state = None;
    app.palette_prev_focus = None;
    handler(app);
}

fn handle_palette_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => close_palette_cancelled(app),
        KeyCode::Up => {
            if let Some(state) = app.palette_state.as_mut() {
                state.cursor = state.cursor.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            let matches_len = app
                .palette_state
                .as_ref()
                .map(|s| matches_for_input(&s.input, app).len())
                .unwrap_or(0);
            if let Some(state) = app.palette_state.as_mut()
                && matches_len > 0
            {
                state.cursor = (state.cursor + 1).min(matches_len - 1);
            }
        }
        KeyCode::Enter => commit_palette(app),
        KeyCode::Char(c) => {
            if let Some(state) = app.palette_state.as_mut() {
                state.input.push(c);
                state.cursor = 0;
            }
        }
        KeyCode::Backspace => {
            if let Some(state) = app.palette_state.as_mut() {
                state.input.pop();
                state.cursor = 0;
            }
        }
        _ => {}
    }
}

fn cancel_at_cursor(app: &mut App) -> bool {
    let Some(id) = app.session.cursor() else {
        return false;
    };
    let Some(mut cmd) = app.running.remove(&id) else {
        return false;
    };
    let _ = cmd.killer.kill();
    cmd.handle.abort();
    app.session
        .set_state(id, ShedState::Failed("cancelled".into()));
    app.flash = Some(format!("cancelled %{}", id.0));
    true
}

/// Dispatch a mouse event by hit-testing the click regions registered
/// during the last draw pass. Left-button-down activates the targeted
/// region; right-button-down opens a context menu for the shed body
/// under the cursor. Scroll and motion events are ignored.
fn handle_mouse(app: &mut App, me: MouseEvent) {
    // If a context menu is open, every click closes it. Clicks on a menu
    // item additionally fire that item; clicks elsewhere just dismiss.
    if app.context_menu.is_some() {
        if let MouseEventKind::Down(_) = me.kind {
            if let Some(idx) = menu_item_at(app, me.column, me.row) {
                if let Some(menu) = app.context_menu.as_mut() {
                    menu.selected = idx;
                }
                activate_menu_item(app);
            } else {
                app.context_menu = None;
            }
        }
        return;
    }
    match me.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // Shift-Left-Click OR Ctrl-Left-Click is a shortcut for "Add
            // to prompt": grab the cell text (or line text if no cell)
            // under the cursor and splice it into the prompt, switching
            // focus to Prompt first if needed. Both modifiers are
            // accepted because most terminals intercept Shift for native
            // text selection (so Shift+click never reaches us), while a
            // few use Ctrl for "open link"; whichever your terminal lets
            // through wins. Bypasses the [×] button and the menu.
            if me.modifiers.contains(KeyModifiers::SHIFT)
                || me.modifiers.contains(KeyModifiers::CONTROL)
            {
                handle_shift_left_click(app, me.column, me.row);
                return;
            }
            let hit = app
                .click_regions
                .iter()
                .find(|r| rect_contains(r.rect, me.column, me.row))
                .map(|r| r.action);
            let Some(action) = hit else {
                return;
            };
            app.flash = None;
            match action {
                ClickAction::DeleteBlock(id) => delete_shed(app, id),
                ClickAction::SwitchTab(idx) => app.switch_to_tab(idx),
                ClickAction::NewTab => {
                    app.new_tab();
                }
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            let hit = app
                .body_regions
                .iter()
                .find(|r| rect_contains(r.rect, me.column, me.row))
                .cloned();
            let Some(region) = hit else {
                return;
            };
            open_body_context_menu(app, &region, me.column, me.row);
        }
        _ => {}
    }
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// Modifier-Left-Click handler (Shift or Ctrl): insert the cell-or-line
/// text under (col, row) into the prompt. Switches focus to Prompt so
/// the shortcut works from anywhere (ShedCursor, EditShed, etc.).
/// No-op when the click lands outside any shed body or on an empty
/// target.
fn handle_shift_left_click(app: &mut App, col: u16, row: u16) {
    let Some(region) = app
        .body_regions
        .iter()
        .find(|r| rect_contains(r.rect, col, row))
        .cloned()
    else {
        return;
    };
    let cell_text = region
        .cells
        .iter()
        .find(|c| rect_contains(c.rect, col, row))
        .map(|c| cell_string(&c.value).trim().to_string())
        .filter(|s| !s.is_empty());
    let text = cell_text.or_else(|| {
        let line_idx = row.saturating_sub(region.rect.y) as usize;
        region
            .lines
            .get(line_idx)
            .map(|s| s.trim_end().to_string())
            .filter(|s| !s.is_empty())
    });
    let Some(text) = text else {
        return;
    };
    if app.focus != Focus::Prompt {
        app.focus = Focus::Prompt;
    }
    insert_at_prompt(app, &text);
    app.flash = Some(format!("added {} chars to prompt", text.len()));
}

/// Build the context menu for a right-clicked shed body. Hit-tests cells
/// first (most specific) — for cell hits the menu leads with cell-level
/// actions plus type-specific items (e.g. "Copy filename" when the cell
/// looks like a path). Falls back to line-level actions when no cell is
/// hit; always offers "Copy whole output" as the catch-all.
fn open_body_context_menu(app: &mut App, region: &BodyRegion, col: u16, row: u16) {
    let line_idx = row.saturating_sub(region.rect.y) as usize;
    let line_text: Option<String> = region
        .lines
        .get(line_idx)
        .map(|s| s.trim_end().to_string())
        .filter(|s| !s.is_empty());

    let cell_hit = region
        .cells
        .iter()
        .find(|c| rect_contains(c.rect, col, row));

    let whole_output = shed_plain_output(&app.session, region.shed_id);
    let mut items: Vec<ContextMenuItem> = Vec::new();

    if let Some(cell) = cell_hit {
        let text = cell_string(&cell.value);
        let trimmed = text.trim().to_string();
        if !trimmed.is_empty() {
            items.push(ContextMenuItem {
                label: "Copy cell".into(),
                action: ContextMenuAction::CopyText(trimmed.clone()),
            });
            if app.focus == Focus::Prompt {
                items.push(ContextMenuItem {
                    label: "Add cell to prompt".into(),
                    action: ContextMenuAction::InsertAtPrompt(trimmed.clone()),
                });
            }
            if let Some(base) = path_basename(&trimmed)
                && base != trimmed
            {
                items.push(ContextMenuItem {
                    label: "Copy filename".into(),
                    action: ContextMenuAction::CopyText(base.clone()),
                });
                if app.focus == Focus::Prompt {
                    items.push(ContextMenuItem {
                        label: "Add filename to prompt".into(),
                        action: ContextMenuAction::InsertAtPrompt(base),
                    });
                }
            }
        }
    } else if let Some(line) = line_text {
        items.push(ContextMenuItem {
            label: "Copy line".into(),
            action: ContextMenuAction::CopyText(line.clone()),
        });
        if app.focus == Focus::Prompt {
            items.push(ContextMenuItem {
                label: "Add line to prompt".into(),
                action: ContextMenuAction::InsertAtPrompt(line),
            });
        }
    }
    if let Some(out) = whole_output {
        items.push(ContextMenuItem {
            label: "Copy whole output".into(),
            action: ContextMenuAction::CopyText(out),
        });
    }
    if items.is_empty() {
        return;
    }
    app.context_menu = Some(ContextMenu {
        pos: (col, row),
        items,
        selected: 0,
    });
}

/// Heuristic: if `text` looks like a filesystem path with a directory
/// component, return its basename (the last `/`-separated segment).
/// Returns `None` for values without a `/`, for values containing
/// whitespace (unlikely to be a usable filename), or when the basename
/// would be empty (trailing `/`).
fn path_basename(text: &str) -> Option<String> {
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return None;
    }
    let looks_pathy = text.starts_with('/')
        || text.starts_with("./")
        || text.starts_with("../")
        || text.starts_with("~/")
        || text.contains('/');
    if !looks_pathy {
        return None;
    }
    let trimmed = text.trim_end_matches('/');
    let base = trimmed.rsplit('/').next()?;
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// Plain text for "Copy whole output". For byte-stream captures this is
/// the raw stdout (pipeline NOT applied — users typically want the
/// captured text, not the filtered view). For structured snapshot
/// captures (no stdout bytes to copy), the inherited structured value
/// is rendered the same way the shed body renders it, so the clipboard
/// gets what the user sees.
fn shed_plain_output(session: &Session, id: ShedId) -> Option<String> {
    let shed = session.shed(id)?;
    let capture = shed.capture.as_ref()?;
    if let Some(value) = &capture.structured {
        let lines = render_pipeline_value_with_max(
            PipelineValue::Structured(value.clone()),
            usize::MAX,
            false,
            &mut Vec::new(),
        );
        let mut out = String::new();
        for line in &lines {
            out.push_str(&line_plain_text(line));
            out.push('\n');
        }
        return Some(out);
    }
    Some(String::from_utf8_lossy(&capture.stdout).into_owned())
}

fn line_plain_text(line: &Line<'_>) -> String {
    let mut s = String::new();
    for span in &line.spans {
        s.push_str(&span.content);
    }
    s
}

async fn handle_prompt_key(app: &mut App, key: KeyEvent) {
    let is_tab = matches!(key.code, KeyCode::Tab | KeyCode::BackTab);
    if !is_tab {
        app.completion = None;
    }
    match key.code {
        KeyCode::Tab => {
            cycle_completion(app, 1);
            return;
        }
        KeyCode::BackTab => {
            cycle_completion(app, -1);
            return;
        }
        KeyCode::Esc => {
            if let Some(id) = app.newest_shed_id() {
                app.session.set_cursor(Some(id));
                app.reset_pipeline_cursor();
                app.focus = Focus::ShedCursor;
            } else {
                app.flash = Some("no sheds yet".into());
            }
            return;
        }
        KeyCode::Up => {
            history_step(app, -1);
            return;
        }
        KeyCode::Down => {
            history_step(app, 1);
            return;
        }
        KeyCode::Enter => {
            spawn_prompt(app).await;
            return;
        }
        _ => {}
    }
    if apply_readline_edit(&mut app.prompt, &mut app.prompt_cursor, &key) {
        app.history_cursor = None;
    }
}

/// Resolve `$XDG_CACHE_HOME/shed/history`, falling back to
/// `$HOME/.cache/shed/history`. Returns `None` if neither env var is set.
fn history_file_path() -> Option<PathBuf> {
    let cache_dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(cache_dir.join("shed").join("history"))
}

/// `$XDG_CONFIG_HOME/shed/aliases.json`, falling back to
/// `$HOME/.config/shed/aliases.json`. Returns `None` if neither env var
/// is set (in which case aliases run in-memory but never persist).
fn aliases_file_path() -> Option<PathBuf> {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config_dir.join("shed").join("aliases.json"))
}

fn load_aliases_from_default_path() -> Option<AliasFile> {
    let path = aliases_file_path()?;
    AliasFile::load(&path).ok()
}

/// Best-effort persist of the in-memory alias set. Failures land in the
/// flash bar so the user knows their change didn't make it to disk, but
/// the in-memory state stays valid for the rest of the session.
fn persist_aliases(app: &mut App) {
    let Some(path) = app.aliases_path.clone() else {
        return;
    };
    if let Err(e) = app.aliases.save(&path) {
        app.flash = Some(format!("aliases save failed: {e}"));
    }
}

fn load_history_from_default_path() -> Option<Vec<String>> {
    let path = history_file_path()?;
    load_history_from(&path)
}

fn load_history_from(path: &std::path::Path) -> Option<Vec<String>> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(
        content
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
    )
}

fn append_history_to_default_path(line: &str) -> io::Result<()> {
    let Some(path) = history_file_path() else {
        return Ok(());
    };
    append_history_to(&path, line)
}

fn append_history_to(path: &std::path::Path, line: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn history_step(app: &mut App, delta: i32) {
    if app.history.is_empty() {
        return;
    }
    let new_idx: Option<usize> = match (app.history_cursor, delta) {
        (None, -1) => Some(app.history.len() - 1),
        (None, _) => None,
        (Some(i), -1) => Some(i.saturating_sub(1)),
        (Some(i), 1) => {
            if i + 1 >= app.history.len() {
                None
            } else {
                Some(i + 1)
            }
        }
        _ => app.history_cursor,
    };
    app.history_cursor = new_idx;
    app.prompt = match new_idx {
        Some(i) => app.history[i].clone(),
        None => String::new(),
    };
    app.prompt_cursor = app.prompt.len();
}

fn handle_cursor_key(app: &mut App, key: KeyEvent) {
    if app.is_input(InputKind::Rerun) {
        let outcome = {
            let (t, c) = app.input_mut().expect("input bar open");
            handle_text_input(t, c, &key)
        };
        match outcome {
            InputOutcome::Commit => commit_rerun(app),
            InputOutcome::Cancel => {
                app.close_input();
                app.rerun_source_id = None;
            }
            InputOutcome::Continue => {}
        }
        return;
    }
    if app.is_input(InputKind::Pin) {
        let outcome = {
            let (t, c) = app.input_mut().expect("input bar open");
            handle_text_input(t, c, &key)
        };
        match outcome {
            InputOutcome::Commit => commit_pin(app),
            InputOutcome::Cancel => {
                app.close_input();
            }
            InputOutcome::Continue => {}
        }
        return;
    }
    if app.is_input(InputKind::Write) {
        let outcome = {
            let (t, c) = app.input_mut().expect("input bar open");
            handle_text_input(t, c, &key)
        };
        match outcome {
            InputOutcome::Commit => commit_write(app),
            InputOutcome::Cancel => {
                app.close_input();
            }
            InputOutcome::Continue => {}
        }
        return;
    }
    if app.is_input(InputKind::CmdEdit) {
        let is_tab = matches!(key.code, KeyCode::Tab | KeyCode::BackTab);
        if !is_tab {
            app.completion = None;
        }
        match key.code {
            KeyCode::Tab => {
                cycle_completion(app, 1);
                return;
            }
            KeyCode::BackTab => {
                cycle_completion(app, -1);
                return;
            }
            _ => {}
        }
        let outcome = {
            let (t, c) = app.input_mut().expect("input bar open");
            handle_text_input(t, c, &key)
        };
        match outcome {
            InputOutcome::Commit => commit_cmd_edit(app),
            InputOutcome::Cancel => {
                app.close_input();
            }
            InputOutcome::Continue => {}
        }
        return;
    }
    if app.alias_overwrite.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => confirm_alias_overwrite(app, true),
            KeyCode::Char('n')
            | KeyCode::Char('N')
            | KeyCode::Char('c')
            | KeyCode::Char('C')
            | KeyCode::Esc => confirm_alias_overwrite(app, false),
            _ => {}
        }
        return;
    }
    if app.is_input(InputKind::AliasName) {
        let outcome = {
            let (t, c) = app.input_mut().expect("input bar open");
            handle_text_input(t, c, &key)
        };
        match outcome {
            InputOutcome::Commit => commit_alias_save(app),
            InputOutcome::Cancel => {
                app.close_input();
            }
            InputOutcome::Continue => {}
        }
        return;
    }
    // Scratch box selected (ShedCursor focus, cursor == None): only
    // ↑↓, Esc, and the explicit "start typing" keys do anything —
    // everything else (delete, pin, etc.) needs a real shed.
    let on_scratch = app.session.cursor().is_none();
    if on_scratch {
        match key.code {
            KeyCode::Up => {
                app.move_cursor(-1);
                return;
            }
            KeyCode::Down => return,
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('e') => {
                app.focus = Focus::Prompt;
                return;
            }
            _ => {}
        }
    }
    match key.code {
        KeyCode::Esc => {
            app.focus = Focus::Prompt;
            app.session.set_cursor(None);
            app.command_focused = false;
        }
        KeyCode::Up => app.move_cursor(-1),
        KeyCode::Down => app.move_cursor(1),
        KeyCode::Char(' ') => run_cursor_shed_in_place(app),
        KeyCode::Char('x') => delete_shed_at_cursor(app),
        KeyCode::Char('n') => open_note_edit(app, NotePosition::Pre),
        KeyCode::Char('N') => open_note_edit(app, NotePosition::Post),
        KeyCode::Char('A') => open_alias_save(app),
        KeyCode::Char('e') => enter_edit_shed(app),
        KeyCode::Char('/') => {
            app.focus = Focus::Prompt;
            app.session.set_cursor(None);
            app.command_focused = false;
            app.prompt.clear();
            app.prompt.push('/');
            app.prompt_cursor = app.prompt.len();
            app.history_cursor = None;
        }
        KeyCode::Char('v') if app.session.cursor().is_some() => {
            app.expand_scroll = 0;
            app.focus = Focus::ShedExpand;
        }
        KeyCode::Char('w') if app.session.cursor().is_some() => {
            app.open_input(InputKind::Write, String::new());
        }
        KeyCode::Char('p') => {
            if let Some(id) = app.session.cursor() {
                let existing = app
                    .session
                    .shed(id)
                    .and_then(|b| b.name.clone())
                    .unwrap_or_default();
                app.open_input(InputKind::Pin, existing);
            }
        }
        KeyCode::Char('u') => {
            if let Some(id) = app.session.cursor() {
                let was_named = app.session.shed(id).and_then(|b| b.name.clone());
                if was_named.is_some() {
                    app.savepoint();
                }
                app.session.unpin(id);
                app.flash = Some(match was_named {
                    Some(name) => format!("unpinned %{} (was @{})", id.0, name),
                    None => format!("%{} was not pinned", id.0),
                });
            }
        }
        KeyCode::Char('r') => {
            if let Some(id) = app.session.cursor()
                && let Some(shed) = app.session.shed(id)
            {
                let joined = shlex::try_join(shed.argv.iter().map(String::as_str))
                    .unwrap_or_else(|_| shed.argv.join(" "));
                app.open_input(InputKind::Rerun, joined);
                app.rerun_source_id = Some(id);
            }
        }
        _ => {}
    }
}

fn enter_edit_shed(app: &mut App) {
    if app.session.cursor().is_none() {
        app.flash = Some("no shed selected".into());
        return;
    }
    app.focus = Focus::EditShed;
    app.reset_pipeline_cursor();
}

/// EditShed owns pipeline navigation and mutation; shed-level actions
/// (run, delete, pin, etc.) live one focus up in ShedCursor. Esc returns
/// to ShedCursor.
fn handle_edit_shed_key(app: &mut App, key: KeyEvent) {
    if app.is_input(InputKind::CmdEdit) {
        let is_tab = matches!(key.code, KeyCode::Tab | KeyCode::BackTab);
        if !is_tab {
            app.completion = None;
        }
        match key.code {
            KeyCode::Tab => {
                cycle_completion(app, 1);
                return;
            }
            KeyCode::BackTab => {
                cycle_completion(app, -1);
                return;
            }
            _ => {}
        }
        let outcome = {
            let (t, c) = app.input_mut().expect("input bar open");
            handle_text_input(t, c, &key)
        };
        match outcome {
            InputOutcome::Commit => commit_cmd_edit(app),
            InputOutcome::Cancel => {
                app.close_input();
            }
            InputOutcome::Continue => {}
        }
        return;
    }
    match key.code {
        KeyCode::Esc => {
            app.focus = Focus::ShedCursor;
            app.command_focused = false;
        }
        // Filters render vertically, so ↑↓ does the navigation that
        // ←→ used to. ←→ are kept as aliases for muscle memory.
        KeyCode::Up | KeyCode::Left => move_filter_cursor(app, -1),
        KeyCode::Down | KeyCode::Right => move_filter_cursor(app, 1),
        KeyCode::Char('f') | KeyCode::Enter => {
            if app.command_focused {
                open_cmd_edit(app);
            } else {
                open_filter_edit(app);
            }
        }
        KeyCode::Char('i') => open_filter_insert(app),
        KeyCode::Char('d') => drop_filter_at_cursor(app),
        KeyCode::Char('<') => move_filter_in_pipeline(app, -1),
        KeyCode::Char('>') => move_filter_in_pipeline(app, 1),
        _ => {}
    }
}

fn commit_rerun(app: &mut App) {
    let input = app.take_input_text();
    let source_id = app.rerun_source_id.take();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        app.flash = Some("command required".into());
        return;
    }
    let (force_fullscreen, command_str) = match trimmed.strip_prefix('!') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, trimmed),
    };
    let Some(argv) = shlex::split(command_str) else {
        app.flash = Some("unmatched quote".into());
        return;
    };
    if argv.is_empty() {
        return;
    }
    let pipeline: Vec<FilterSpec> = source_id
        .and_then(|id| app.session.shed(id))
        .map(|b| b.pipeline.clone())
        .unwrap_or_default();
    app.pending_rerun = Some(RerunRequest {
        argv,
        pipeline,
        force_fullscreen,
    });
}

fn commit_pin(app: &mut App) {
    let name = app.take_input_text().trim().to_string();
    let Some(id) = app.session.cursor() else {
        return;
    };

    if name.is_empty() {
        app.savepoint();
        app.session.unpin(id);
        app.flash = Some(format!("unpinned %{}", id.0));
        return;
    }

    let previous_owner = app.session.lookup_by_name(&name);
    app.savepoint();
    if !app.session.pin(id, name.clone()) {
        // Restore: pop the savepoint we just pushed since nothing changed.
        app.undo_stack.pop();
        app.flash = Some(format!("pin failed: unknown shed %{}", id.0));
        return;
    }

    app.flash = Some(match previous_owner {
        Some(prev) if prev != id => format!("pinned %{} as @{} (was on %{})", id.0, name, prev.0),
        Some(_) => format!("re-pinned %{} as @{}", id.0, name),
        None => format!("pinned %{} as @{}", id.0, name),
    });
}

/// Output format inferred from the path extension at write time.
/// `.csv` → comma-separated, `.tsv` → tab-separated, `.json` → pretty
/// JSON, anything else → plain text (each rendered line joined with
/// `\n`). The format only affects serialization; the underlying
/// pipeline output is the same in all three cases.
#[derive(Debug, Clone, Copy)]
enum WriteFormat {
    Plain,
    Csv(u8),
    Json,
}

impl WriteFormat {
    fn from_path(path: &str) -> Self {
        let lower = path.to_lowercase();
        if lower.ends_with(".csv") {
            WriteFormat::Csv(b',')
        } else if lower.ends_with(".tsv") {
            WriteFormat::Csv(b'\t')
        } else if lower.ends_with(".json") {
            WriteFormat::Json
        } else {
            WriteFormat::Plain
        }
    }

    fn name(self) -> &'static str {
        match self {
            WriteFormat::Plain => "plain",
            WriteFormat::Csv(b'\t') => "tsv",
            WriteFormat::Csv(_) => "csv",
            WriteFormat::Json => "json",
        }
    }
}

fn commit_write(app: &mut App) {
    let path = app.take_input_text();
    let path = path.trim();
    if path.is_empty() {
        app.flash = Some("path required".into());
        return;
    }
    let Some(id) = app.session.cursor() else {
        return;
    };
    let Some(shed) = app.session.shed(id) else {
        return;
    };

    let format = WriteFormat::from_path(path);
    let bytes = match format {
        WriteFormat::Plain => render_plain_bytes(shed),
        WriteFormat::Csv(delim) => render_csv_bytes(shed, delim),
        WriteFormat::Json => render_json_bytes(shed),
    };

    let len = bytes.len();
    match std::fs::write(path, bytes) {
        Ok(()) => {
            app.flash = Some(format!(
                "wrote %{} ({} bytes, {}) to {}",
                id.0,
                len,
                format.name(),
                path
            ));
        }
        Err(e) => {
            app.flash = Some(format!("write failed: {e}"));
        }
    }
}

fn render_plain_bytes(shed: &Shed) -> Vec<u8> {
    let lines = compute_shed_lines(shed);
    let mut text = String::new();
    for line in &lines {
        text.push_str(&line_text(line));
        text.push('\n');
    }
    text.into_bytes()
}

fn render_csv_bytes(shed: &Shed, delim: u8) -> Vec<u8> {
    let Some(capture) = shed.capture.as_ref() else {
        return Vec::new();
    };
    let value = match apply_pipeline(capture, &shed.pipeline) {
        Ok((v, _)) => v,
        Err(e) => return e.into_bytes(),
    };
    let items = match value {
        PipelineValue::Structured(Value::List(items)) => items,
        // Bytes / non-list structured: fall back to plain.
        _ => return render_plain_bytes(shed),
    };

    let mut out = Vec::new();
    {
        let mut writer = csv::WriterBuilder::new()
            .delimiter(delim)
            .from_writer(&mut out);

        let columns: Vec<String> = items
            .iter()
            .find_map(|v| match v {
                Value::Record(r) => Some(r.keys().cloned().collect::<Vec<_>>()),
                _ => None,
            })
            .unwrap_or_default();

        if !columns.is_empty() {
            let _ = writer.write_record(&columns);
        }
        for item in &items {
            match item {
                Value::Record(r) => {
                    let row: Vec<String> = columns
                        .iter()
                        .map(|c| r.get(c).map(value_to_field_string).unwrap_or_default())
                        .collect();
                    let _ = writer.write_record(&row);
                }
                other => {
                    let _ = writer.write_record(&[value_to_field_string(other)]);
                }
            }
        }
        let _ = writer.flush();
    }
    out
}

fn render_json_bytes(shed: &Shed) -> Vec<u8> {
    let Some(capture) = shed.capture.as_ref() else {
        return Vec::new();
    };
    let value = match apply_pipeline(capture, &shed.pipeline) {
        Ok((v, _)) => v,
        Err(e) => return e.into_bytes(),
    };
    let json = pipeline_value_to_json(value);
    let mut out = serde_json::to_vec_pretty(&json).unwrap_or_else(|e| e.to_string().into_bytes());
    out.push(b'\n');
    out
}

fn pipeline_value_to_json(v: PipelineValue) -> serde_json::Value {
    match v {
        PipelineValue::Bytes(b) => {
            serde_json::Value::String(String::from_utf8_lossy(&b).to_string())
        }
        PipelineValue::Structured(val) => value_to_json(val),
    }
}

fn value_to_json(v: Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(b),
        Value::Int(i) => serde_json::Value::Number(serde_json::Number::from(i)),
        Value::Float(f) => serde_json::Number::from_f64(f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => serde_json::Value::String(s),
        Value::Bytes(b) => serde_json::Value::String(String::from_utf8_lossy(&b).to_string()),
        Value::List(items) => {
            serde_json::Value::Array(items.into_iter().map(value_to_json).collect())
        }
        Value::Record(r) => {
            let mut map = serde_json::Map::with_capacity(r.len());
            for (k, val) in r {
                map.insert(k, value_to_json(val));
            }
            serde_json::Value::Object(map)
        }
    }
}

fn value_to_field_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => s.clone(),
        Value::Bytes(b) => format!("<{} bytes>", b.len()),
        _ => format!("{v:?}"),
    }
}

fn handle_shed_expand_key(app: &mut App, key: KeyEvent) {
    if app.is_input(InputKind::Search) {
        let outcome = {
            let (t, c) = app.input_mut().expect("input bar open");
            handle_text_input(t, c, &key)
        };
        match outcome {
            InputOutcome::Cancel => {
                app.close_input();
                app.search_query.clear();
                app.expand_scroll = app.search_anchor_scroll;
            }
            InputOutcome::Commit => {
                app.close_input();
                // search_query is already in sync via update_search; nothing to do.
            }
            InputOutcome::Continue => {
                update_search(app);
            }
        }
        return;
    }

    const PAGE: usize = 20;
    match key.code {
        KeyCode::Esc => {
            if !app.search_query.is_empty() {
                app.search_query.clear();
            } else {
                app.expand_scroll = 0;
                app.focus = Focus::ShedCursor;
            }
        }
        KeyCode::Char('q') => {
            app.search_query.clear();
            app.expand_scroll = 0;
            app.focus = Focus::ShedCursor;
        }
        KeyCode::Char('/') => {
            app.open_input(InputKind::Search, String::new());
            app.search_input_backward = false;
            app.search_query.clear();
            app.search_anchor_scroll = app.expand_scroll;
        }
        KeyCode::Char('?') => {
            app.open_input(InputKind::Search, String::new());
            app.search_input_backward = true;
            app.search_query.clear();
            app.search_anchor_scroll = app.expand_scroll;
        }
        KeyCode::Char('i') => {
            app.search_case_insensitive = !app.search_case_insensitive;
        }
        KeyCode::Char('n') => jump_to_search(app, true),
        KeyCode::Char('N') => jump_to_search(app, false),
        KeyCode::Up | KeyCode::Char('k') => {
            app.expand_scroll = app.expand_scroll.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.expand_scroll = app.expand_scroll.saturating_add(1);
        }
        KeyCode::PageUp | KeyCode::Char('b') => {
            app.expand_scroll = app.expand_scroll.saturating_sub(PAGE);
        }
        KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => {
            app.expand_scroll = app.expand_scroll.saturating_add(PAGE);
        }
        KeyCode::Home | KeyCode::Char('g') => {
            app.expand_scroll = 0;
        }
        KeyCode::End | KeyCode::Char('G') => {
            app.expand_scroll = usize::MAX;
        }
        _ => {}
    }
}

/// Live-update the active search as the user types. Keeps `search_query`
/// in sync with `search_input`, then (if the query compiles as a regex)
/// jumps the scroll: forward search jumps to the first match at-or-after
/// the anchor; backward search jumps to the last match at-or-before.
/// Invalid-regex states are left untouched: scroll doesn't move, no
/// error — the input bar just shows "(invalid regex)" until the user
/// finishes typing.
fn update_search(app: &mut App) {
    app.search_query = app.input_text().to_string();
    if app.search_query.is_empty() {
        app.expand_scroll = app.search_anchor_scroll;
        return;
    }
    let Some(regex) = try_compile(&app.search_query, app.search_case_insensitive) else {
        return;
    };
    let Some(id) = app.session.cursor() else {
        return;
    };
    let Some(shed) = app.session.shed(id) else {
        return;
    };
    let lines = compute_shed_lines(shed);
    let matches = find_matches_regex(&lines, &regex);
    if matches.is_empty() {
        return;
    }
    let anchor = app.search_anchor_scroll;
    let target = if app.search_input_backward {
        matches
            .iter()
            .rev()
            .find(|&&m| m <= anchor)
            .copied()
            .unwrap_or_else(|| *matches.last().unwrap())
    } else {
        matches
            .iter()
            .find(|&&m| m >= anchor)
            .copied()
            .unwrap_or_else(|| matches[0])
    };
    app.expand_scroll = target;
}

fn jump_to_search(app: &mut App, forward: bool) {
    if app.search_query.is_empty() {
        return;
    }
    let Some(regex) = try_compile(&app.search_query, app.search_case_insensitive) else {
        app.flash = Some(format!("invalid regex: {}", app.search_query));
        return;
    };
    let Some(id) = app.session.cursor() else {
        return;
    };
    let Some(shed) = app.session.shed(id) else {
        return;
    };
    let lines = compute_shed_lines(shed);
    let matches = find_matches_regex(&lines, &regex);
    if matches.is_empty() {
        app.flash = Some(format!("no matches for '{}'", app.search_query));
        return;
    }
    let cur = app.expand_scroll;
    let next = if forward {
        matches
            .iter()
            .find(|&&m| m > cur)
            .copied()
            .unwrap_or_else(|| matches[0])
    } else {
        matches
            .iter()
            .rev()
            .find(|&&m| m < cur)
            .copied()
            .unwrap_or_else(|| *matches.last().unwrap())
    };
    app.expand_scroll = next;
}

fn compute_shed_lines(shed: &Shed) -> Vec<Line<'static>> {
    match shed.capture.as_ref() {
        Some(capture) => match apply_pipeline(capture, &shed.pipeline) {
            Ok((value, _drops)) => {
                render_pipeline_value_with_max(value, usize::MAX, false, &mut Vec::new())
            }
            Err(e) => filter_error_lines(&e),
        },
        None => Vec::new(),
    }
}

fn try_compile(query: &str, case_insensitive: bool) -> Option<regex::Regex> {
    if query.is_empty() {
        return None;
    }
    let pattern = if case_insensitive {
        format!("(?i){query}")
    } else {
        query.to_string()
    };
    regex::Regex::new(&pattern).ok()
}

fn find_matches_regex(lines: &[Line<'static>], regex: &regex::Regex) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| regex.is_match(&line_text(l)))
        .map(|(i, _)| i)
        .collect()
}

fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Walk a line's spans, splitting at regex match boundaries so that
/// only the matched substrings get the REVERSED modifier. Non-match
/// portions keep their original style (so ANSI colors from PTY output
/// stay intact). Match positions are byte offsets into the line's
/// concatenated plain text; regex guarantees they fall on UTF-8 char
/// boundaries.
fn highlight_matches_in_line(line: Line<'static>, regex: &regex::Regex) -> Line<'static> {
    let plain = line_text(&line);
    let matches: Vec<(usize, usize)> = regex
        .find_iter(&plain)
        .map(|m| (m.start(), m.end()))
        .collect();
    if matches.is_empty() {
        return line;
    }

    let mut new_spans: Vec<Span<'static>> = Vec::new();
    let mut byte_offset = 0usize;

    for span in line.spans {
        let span_start = byte_offset;
        let span_text: String = span.content.into_owned();
        let span_len = span_text.len();
        let span_end = span_start + span_len;
        let span_style = span.style;

        let mut local_pos = 0usize;
        for &(m_start, m_end) in &matches {
            if m_end <= span_start || m_start >= span_end {
                continue;
            }
            let m_local_start = m_start.saturating_sub(span_start).max(local_pos);
            let m_local_end = (m_end - span_start).min(span_len);
            if m_local_start > local_pos {
                new_spans.push(Span::styled(
                    span_text[local_pos..m_local_start].to_string(),
                    span_style,
                ));
            }
            if m_local_end > m_local_start {
                new_spans.push(Span::styled(
                    span_text[m_local_start..m_local_end].to_string(),
                    span_style.add_modifier(Modifier::REVERSED),
                ));
            }
            local_pos = m_local_end;
        }
        if local_pos < span_len {
            new_spans.push(Span::styled(span_text[local_pos..].to_string(), span_style));
        }
        byte_offset = span_end;
    }

    Line::from(new_spans)
}

fn move_filter_cursor(app: &mut App, delta: i32) {
    let Some(len) = app.cursor_shed_pipeline_len() else {
        return;
    };
    // Pulling left at filter index 0 jumps onto the command itself; the
    // command sits one virtual slot to the left of the first filter.
    if app.command_focused {
        if delta > 0 {
            app.command_focused = false;
            app.pipeline_cursor = 0;
        }
        return;
    }
    if delta < 0 && app.pipeline_cursor == 0 {
        app.command_focused = true;
        return;
    }
    let max = len as i32;
    let new = (app.pipeline_cursor as i32 + delta).clamp(0, max) as usize;
    app.pipeline_cursor = new;
}

fn open_filter_edit(app: &mut App) {
    let Some(id) = app.session.cursor() else {
        return;
    };
    let Some(shed) = app.session.shed(id) else {
        return;
    };
    if shed.capture.is_none() {
        let msg = match shed.state {
            ShedState::Running => "still running — no capture yet",
            _ => "no captured output to filter",
        };
        app.flash = Some(msg.into());
        return;
    }
    let state = if app.pipeline_cursor < shed.pipeline.len() {
        FilterEditState::for_edit(shed, app.pipeline_cursor)
    } else {
        FilterEditState::for_add(shed)
    };
    app.filter_edit = Some(state);
    app.focus = Focus::FilterEdit;
}

fn open_filter_insert(app: &mut App) {
    let Some(id) = app.session.cursor() else {
        return;
    };
    let Some(shed) = app.session.shed(id) else {
        return;
    };
    if shed.capture.is_none() {
        let msg = match shed.state {
            ShedState::Running => "still running — no capture yet",
            _ => "no captured output to filter",
        };
        app.flash = Some(msg.into());
        return;
    }
    // On the `+ add` slot, `i` is functionally the same as `f`.
    let state = if app.pipeline_cursor < shed.pipeline.len() {
        FilterEditState::for_insert(shed, app.pipeline_cursor)
    } else {
        FilterEditState::for_add(shed)
    };
    app.filter_edit = Some(state);
    app.focus = Focus::FilterEdit;
}

fn handle_filter_edit_key(app: &mut App, key: KeyEvent) {
    let Some(state) = app.filter_edit.as_mut() else {
        app.focus = Focus::Prompt;
        return;
    };
    match key.code {
        KeyCode::Esc => {
            app.filter_edit = None;
            app.focus = Focus::ShedCursor;
        }
        KeyCode::Tab => state.cycle_field(1),
        KeyCode::BackTab => state.cycle_field(-1),
        KeyCode::Up if !is_multiline_field(state.field) => state.cycle_field(-1),
        KeyCode::Down if !is_multiline_field(state.field) => state.cycle_field(1),
        KeyCode::Enter => apply_filter_edit(app),
        _ => match state.field {
            FormField::Kind => handle_kind_key(state, key),
            FormField::Column => handle_column_key(state, key),
            FormField::Op => handle_op_key(state, key),
            FormField::Pattern => handle_pattern_key(state, key),
            FormField::N => handle_n_key(state, key),
            FormField::Columns => handle_columns_key(state, key),
            FormField::CsvDelim => handle_csv_delim_key(state, key),
            FormField::CsvHasHeader => handle_csv_has_header_key(state, key),
            FormField::RegexPattern => handle_regex_pattern_key(state, key),
            FormField::SortKeys => handle_sort_keys_key(state, key),
            FormField::RenameMap => handle_rename_map_key(state, key),
            FormField::WhereCombine => handle_where_combine_key(state, key),
            FormField::WhereClauseSelect => handle_where_clause_select_key(state, key),
            FormField::TargetColumn => handle_target_column_key(state, key),
            FormField::DelimText => handle_delim_text_key(state, key),
        },
    }
}

fn is_multiline_field(field: FormField) -> bool {
    matches!(
        field,
        FormField::SortKeys | FormField::RenameMap | FormField::Columns
    )
}

fn filter_edit_field_hints(field: FormField) -> Vec<(&'static str, &'static str)> {
    use FormField::*;
    let mut hints: Vec<(&'static str, &'static str)> = match field {
        Kind => vec![("←→", "kind")],
        Column => vec![("←→", "column")],
        Op => vec![("←→", "op")],
        Pattern => vec![("type", "value")],
        N => vec![("0-9", "digits")],
        Columns => vec![("↑↓", "cursor"), ("Space", "toggle")],
        CsvDelim => vec![("←→", "delim")],
        CsvHasHeader => vec![("←→/Space", "toggle")],
        RegexPattern => vec![("type", "regex")],
        SortKeys => vec![
            ("↑↓", "row"),
            ("←→", "column"),
            ("Space", "asc/desc"),
            ("a", "add"),
            ("x", "remove"),
        ],
        RenameMap => vec![("↑↓", "row"), ("type", "name")],
        WhereCombine => vec![("←→/Space", "AND/OR")],
        WhereClauseSelect => vec![("←→", "clause"), ("a", "add"), ("x", "remove")],
        TargetColumn => vec![("←→", "column")],
        DelimText => vec![("type", "delim")],
    };
    hints.push(("Tab", "next field"));
    hints.push(("Enter", "apply"));
    hints.push(("Esc", "cancel"));
    hints
}

fn handle_target_column_key(state: &mut FilterEditState, key: KeyEvent) {
    if state.available_columns.is_empty() {
        return;
    }
    let len = state.available_columns.len() as i32;
    let delta: i32 = match key.code {
        KeyCode::Left | KeyCode::Up => -1,
        KeyCode::Right | KeyCode::Down => 1,
        _ => return,
    };
    state.target_column = ((state.target_column as i32 + delta).rem_euclid(len)) as usize;
}

fn handle_delim_text_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Char(c) => state.delim_text.push(c),
        KeyCode::Backspace => {
            state.delim_text.pop();
        }
        _ => {}
    }
}

fn handle_rename_map_key(state: &mut FilterEditState, key: KeyEvent) {
    if state.available_columns.is_empty() {
        return;
    }
    let max = state.available_columns.len().saturating_sub(1);
    match key.code {
        KeyCode::Up => {
            state.rename_cursor = state.rename_cursor.saturating_sub(1);
        }
        KeyCode::Down => {
            state.rename_cursor = (state.rename_cursor + 1).min(max);
        }
        KeyCode::Char(c) => {
            if let Some(input) = state.rename_to_inputs.get_mut(state.rename_cursor) {
                input.push(c);
            }
        }
        KeyCode::Backspace => {
            if let Some(input) = state.rename_to_inputs.get_mut(state.rename_cursor) {
                input.pop();
            }
        }
        _ => {}
    }
}

fn handle_sort_keys_key(state: &mut FilterEditState, key: KeyEvent) {
    if state.available_columns.is_empty() {
        return;
    }
    let n = state.sort_keys.len();
    let last_visible = if n < MAX_SORT_KEYS {
        n
    } else {
        n.saturating_sub(1)
    };

    match key.code {
        KeyCode::Up => {
            state.sort_keys_cursor = state.sort_keys_cursor.saturating_sub(1);
        }
        KeyCode::Down => {
            state.sort_keys_cursor = (state.sort_keys_cursor + 1).min(last_visible);
        }
        KeyCode::Left | KeyCode::Right if state.sort_keys_cursor < n => {
            let cols = state.available_columns.len() as i32;
            let delta: i32 = if matches!(key.code, KeyCode::Right) {
                1
            } else {
                -1
            };
            let cur = state.sort_keys[state.sort_keys_cursor].0 as i32;
            let new = (cur + delta).rem_euclid(cols) as usize;
            state.sort_keys[state.sort_keys_cursor].0 = new;
        }
        KeyCode::Char(' ') if state.sort_keys_cursor < n => {
            let cur = state.sort_keys[state.sort_keys_cursor].1;
            state.sort_keys[state.sort_keys_cursor].1 = match cur {
                SortDirection::Asc => SortDirection::Desc,
                SortDirection::Desc => SortDirection::Asc,
            };
        }
        KeyCode::Char('a') if n < MAX_SORT_KEYS => {
            state.sort_keys.push((0, SortDirection::Asc));
            state.sort_keys_cursor = state.sort_keys.len() - 1;
        }
        KeyCode::Char('x') | KeyCode::Backspace | KeyCode::Delete
            if state.sort_keys_cursor < n && state.sort_keys.len() > 1 =>
        {
            state.sort_keys.remove(state.sort_keys_cursor);
            if state.sort_keys_cursor >= state.sort_keys.len() {
                state.sort_keys_cursor = state.sort_keys.len().saturating_sub(1);
            }
        }
        _ => {}
    }
}

fn handle_csv_delim_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Left | KeyCode::Up => state.cycle_delim(-1),
        KeyCode::Right | KeyCode::Down => state.cycle_delim(1),
        _ => {}
    }
}

fn handle_csv_has_header_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Char(' ') => {
            state.csv_has_header = !state.csv_has_header;
        }
        _ => {}
    }
}

fn handle_regex_pattern_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Char(c) => state.regex_pattern.push(c),
        KeyCode::Backspace => {
            state.regex_pattern.pop();
        }
        _ => {}
    }
}

fn handle_op_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Left | KeyCode::Up => state.cycle_clause_op(-1),
        KeyCode::Right | KeyCode::Down => state.cycle_clause_op(1),
        _ => {}
    }
}

fn handle_where_combine_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Char(' ') => {
            state.where_combine = state.where_combine.flip();
        }
        _ => {}
    }
}

fn handle_where_clause_select_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Left | KeyCode::Up if state.where_active_clause > 0 => {
            state.where_active_clause -= 1;
        }
        KeyCode::Right | KeyCode::Down
            if state.where_active_clause + 1 < state.where_clauses.len() =>
        {
            state.where_active_clause += 1;
        }
        KeyCode::Char('a') => {
            state
                .where_clauses
                .push(WhereClause::default_for(&state.available_columns));
            state.where_active_clause = state.where_clauses.len() - 1;
        }
        KeyCode::Char('x') | KeyCode::Backspace | KeyCode::Delete
            if state.where_clauses.len() > 1 =>
        {
            state.where_clauses.remove(state.where_active_clause);
            if state.where_active_clause >= state.where_clauses.len() {
                state.where_active_clause = state.where_clauses.len() - 1;
            }
        }
        _ => {}
    }
}

fn handle_kind_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Left | KeyCode::Up => state.cycle_kind(-1),
        KeyCode::Right | KeyCode::Down => state.cycle_kind(1),
        _ => {}
    }
}

fn handle_column_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Left | KeyCode::Up => state.cycle_column(-1),
        KeyCode::Right | KeyCode::Down => state.cycle_column(1),
        _ => {}
    }
}

fn handle_pattern_key(state: &mut FilterEditState, key: KeyEvent) {
    let Some(clause) = state.active_clause_mut() else {
        return;
    };
    match key.code {
        KeyCode::Char(c) => clause.pattern.push(c),
        KeyCode::Backspace => {
            clause.pattern.pop();
        }
        _ => {}
    }
}

fn handle_n_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Char(c) if c.is_ascii_digit() => state.n_input.push(c),
        KeyCode::Backspace => {
            state.n_input.pop();
        }
        _ => {}
    }
}

fn handle_columns_key(state: &mut FilterEditState, key: KeyEvent) {
    if state.available_columns.is_empty() {
        return;
    }
    let len = state.available_columns.len();
    match key.code {
        KeyCode::Up | KeyCode::Left => {
            state.column_cursor = (state.column_cursor + len - 1) % len;
        }
        KeyCode::Down | KeyCode::Right => {
            state.column_cursor = (state.column_cursor + 1) % len;
        }
        KeyCode::Char(' ') => {
            if let Some(sel) = state.column_selections.get_mut(state.column_cursor) {
                *sel = !*sel;
            }
        }
        _ => {}
    }
}

fn apply_filter_edit(app: &mut App) {
    let Some(state) = app.filter_edit.as_ref() else {
        return;
    };
    let Some(spec) = state.build_filter() else {
        app.flash = Some("pick a column first".into());
        return;
    };
    let id = state.shed_id;
    let mode = state.mode;
    if app.session.shed(id).is_none() {
        app.filter_edit = None;
        app.focus = Focus::ShedCursor;
        return;
    }
    app.savepoint();
    if let Some(shed) = app.session.shed_mut(id) {
        match mode {
            EditMode::Add => shed.pipeline.push(spec),
            EditMode::Edit(i) if i < shed.pipeline.len() => shed.pipeline[i] = spec,
            EditMode::Edit(_) => shed.pipeline.push(spec),
            EditMode::Insert(i) => {
                let pos = i.min(shed.pipeline.len());
                shed.pipeline.insert(pos, spec);
            }
        }
    }
    app.filter_edit = None;
    app.focus = Focus::EditShed;
    app.reset_pipeline_cursor();
}

fn move_filter_in_pipeline(app: &mut App, delta: i32) {
    let Some(id) = app.session.cursor() else {
        return;
    };
    let pos = app.pipeline_cursor;
    let len = match app.session.shed(id) {
        Some(b) => b.pipeline.len(),
        None => return,
    };
    if pos >= len {
        return; // Cursor is on the `+ add` slot, nothing to move.
    }
    let new_pos_signed = pos as i32 + delta;
    if new_pos_signed < 0 || new_pos_signed as usize >= len {
        return;
    }
    let new_pos = new_pos_signed as usize;
    app.savepoint();
    if let Some(shed) = app.session.shed_mut(id) {
        shed.pipeline.swap(pos, new_pos);
    }
    app.pipeline_cursor = new_pos;
}

fn drop_filter_at_cursor(app: &mut App) {
    let Some(id) = app.session.cursor() else {
        return;
    };
    let cursor = app.pipeline_cursor;
    let pipeline_empty = app
        .session
        .shed(id)
        .map(|b| b.pipeline.is_empty())
        .unwrap_or(true);
    if pipeline_empty {
        app.flash = Some("no filters to drop".into());
        return;
    }
    app.savepoint();
    let dropped = if let Some(shed) = app.session.shed_mut(id) {
        if cursor < shed.pipeline.len() {
            shed.pipeline.remove(cursor);
            true
        } else {
            shed.pipeline.pop();
            true
        }
    } else {
        false
    };
    if !dropped {
        // Nothing changed — pop the just-pushed savepoint.
        app.undo_stack.pop();
        app.flash = Some("no filters to drop".into());
    }
    let new_len = app.session.shed(id).map(|b| b.pipeline.len()).unwrap_or(0);
    if app.pipeline_cursor > new_len {
        app.pipeline_cursor = new_len;
    }
}

async fn spawn_prompt(app: &mut App) {
    let trimmed = app.prompt.trim();
    if trimmed.is_empty() {
        return;
    }

    // Slash commands: prompt-level meta-actions that don't run as a
    // command and don't show up in history. Bypass shlex / spawn entirely.
    if let Some(rest) = trimmed.strip_prefix('/') {
        let cmd = rest.trim().to_string();
        app.prompt.clear();
        app.prompt_cursor = 0;
        app.history_cursor = None;
        match cmd.as_str() {
            "aliases" => open_alias_manage(app),
            other => {
                app.flash = Some(format!("unknown slash command: /{other}"));
            }
        }
        return;
    }

    let (force_fullscreen, command_str) = match trimmed.strip_prefix('!') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, trimmed),
    };

    let Some(argv) = shlex::split(command_str) else {
        app.flash = Some("unmatched quote".into());
        return;
    };
    if argv.is_empty() {
        return;
    }

    // Save the original input to history before clearing the prompt.
    // Suppress consecutive duplicates so spamming Up doesn't grow history.
    let original = app.prompt.clone();
    if app.history.last().is_none_or(|last| last != &original) {
        app.history.push(original.clone());
        // Best-effort persist to ~/.cache/shed/history; ignore failures so
        // a missing $HOME / read-only FS / etc. doesn't break the session.
        let _ = append_history_to_default_path(&original);
    }
    app.history_cursor = None;
    app.prompt.clear();
    app.prompt_cursor = 0;

    // Aliases shadow real binaries: a single-token input matching a
    // saved alias materialises a shed with that alias's argv + pipeline
    // and drops the user into in-place command edit so they can append
    // args before running. Multi-token inputs bypass the lookup.
    if !force_fullscreen
        && argv.len() == 1
        && let Some(alias) = app.aliases.lookup(&argv[0]).cloned()
    {
        spawn_alias(app, &alias);
        return;
    }

    if force_fullscreen || needs_fullscreen(&argv) {
        app.pending_handover = Some(HandoverRequest {
            argv,
            reuse_shed: None,
        });
        return;
    }

    if let Some(r) = shed_ref_of(&argv) {
        spawn_ref_snapshot(app, &r);
        return;
    }

    match argv[0].as_str() {
        "cd" => {
            run_cd_builtin(app, &argv);
            return;
        }
        "exit" | "quit" => {
            run_exit_builtin(app);
            return;
        }
        "export" => {
            run_export_builtin(app, &argv);
            return;
        }
        "unset" => {
            run_unset_builtin(app, &argv);
            return;
        }
        _ => {}
    }

    app.savepoint();
    let id = app.session.add_shed(argv.clone());
    match exec::spawn_command(argv, CAPTURE_CAP).await {
        Ok((handle, killer, chunks)) => {
            app.running.insert(
                id,
                RunningCommand {
                    handle,
                    killer,
                    chunks,
                    stream_buf: BytesMut::new(),
                },
            );
        }
        Err(e) => {
            app.session.set_state(id, ShedState::Failed(e.to_string()));
        }
    }
}

/// Handle `cd` as a shell-style builtin: it can't be spawned as an
/// executable because it's a state change in shed itself (the process's
/// current working directory). Subsequent spawns inherit the new cwd
/// because exec::run_blocking calls std::env::current_dir() at spawn
/// time. `cd -` swaps to the previous cwd (tracked in App, not via
/// $OLDPWD env, to avoid the unsafe-set_var dance).
fn run_cd_builtin(app: &mut App, argv: &[String]) {
    app.savepoint();
    let id = app.session.add_shed(argv.to_vec());
    let prev_cwd = std::env::current_dir().ok();

    let target: Result<PathBuf, String> = match argv.get(1).map(String::as_str) {
        Some("-") => app
            .last_cwd
            .clone()
            .ok_or_else(|| "no previous directory".into()),
        Some(p) => Ok(expand_tilde(p)),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "$HOME not set".into()),
    };

    let result = target.and_then(|t| {
        std::env::set_current_dir(&t)
            .map(|_| t)
            .map_err(|e| e.to_string())
    });

    match result {
        Ok(new_path) => {
            app.last_cwd = prev_cwd;
            app.session.set_state(id, ShedState::Done(0));
            app.flash = Some(format!("cd → {}", new_path.display()));
        }
        Err(e) => {
            app.session
                .set_state(id, ShedState::Failed(format!("cd: {e}")));
        }
    }
}

fn run_exit_builtin(app: &mut App) {
    // Route through request_quit so `exit` / `quit` follow the same
    // multi-tab semantics as Ctrl-D: close the active tab when more
    // than one is open, otherwise prompt / quit.
    request_quit(app);
}

/// Open the note editor for the cursor shed at `position`. Pre-fills
/// the buffer with any existing note text.
fn open_note_edit(app: &mut App, position: NotePosition) {
    let Some(id) = app.session.cursor() else {
        app.flash = Some("no shed selected".into());
        return;
    };
    let initial = app.session.shed(id).and_then(|b| match position {
        NotePosition::Pre => b.pre_text.as_deref(),
        NotePosition::Post => b.post_text.as_deref(),
    });
    app.note_edit = Some(NoteEditState::new(id, position, initial));
    app.focus = Focus::NoteEdit;
}

/// Commit the note buffer onto the target shed and exit NoteEdit. An
/// empty buffer clears the note (sets the field to `None`).
fn commit_note_edit(app: &mut App) {
    let Some(state) = app.note_edit.take() else {
        app.focus = Focus::ShedCursor;
        return;
    };
    let text = state.buffer_string();
    let new_value = if text.is_empty() { None } else { Some(text) };
    let prev_value = app
        .session
        .shed(state.shed_id)
        .and_then(|b| match state.position {
            NotePosition::Pre => b.pre_text.clone(),
            NotePosition::Post => b.post_text.clone(),
        });
    if prev_value != new_value {
        app.savepoint();
        if let Some(shed) = app.session.shed_mut(state.shed_id) {
            match state.position {
                NotePosition::Pre => shed.pre_text = new_value,
                NotePosition::Post => shed.post_text = new_value,
            }
        }
    }
    app.focus = Focus::ShedCursor;
}

fn cancel_note_edit(app: &mut App) {
    app.note_edit = None;
    app.focus = Focus::ShedCursor;
}

fn handle_note_edit_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('s') => {
                commit_note_edit(app);
                return;
            }
            KeyCode::Char('c') | KeyCode::Char('d') => {
                cancel_note_edit(app);
                return;
            }
            _ => return,
        }
    }
    let Some(state) = app.note_edit.as_mut() else {
        app.focus = Focus::ShedCursor;
        return;
    };
    match key.code {
        KeyCode::Esc => {
            cancel_note_edit(app);
        }
        KeyCode::Enter => {
            state.buffer.insert(state.cursor, '\n');
            state.cursor += 1;
        }
        KeyCode::Char(c) => {
            state.buffer.insert(state.cursor, c);
            state.cursor += 1;
        }
        KeyCode::Backspace if state.cursor > 0 => {
            state.buffer.remove(state.cursor - 1);
            state.cursor -= 1;
        }
        KeyCode::Delete if state.cursor < state.buffer.len() => {
            state.buffer.remove(state.cursor);
        }
        KeyCode::Left => {
            state.cursor = state.cursor.saturating_sub(1);
        }
        KeyCode::Right => {
            state.cursor = (state.cursor + 1).min(state.buffer.len());
        }
        KeyCode::Home => {
            // Move to start of current line.
            while state.cursor > 0 && state.buffer[state.cursor - 1] != '\n' {
                state.cursor -= 1;
            }
        }
        KeyCode::End => {
            // Move to end of current line.
            while state.cursor < state.buffer.len() && state.buffer[state.cursor] != '\n' {
                state.cursor += 1;
            }
        }
        KeyCode::Up => {
            move_note_cursor_vertically(state, -1);
        }
        KeyCode::Down => {
            move_note_cursor_vertically(state, 1);
        }
        _ => {}
    }
}

/// Move the cursor up (-1) or down (+1) one line, preserving column where
/// possible. Column = chars from the start of the current line.
fn move_note_cursor_vertically(state: &mut NoteEditState, delta: i32) {
    let (line_start, col) = {
        let mut start = 0usize;
        let mut col = 0usize;
        for (i, ch) in state.buffer[..state.cursor].iter().enumerate() {
            if *ch == '\n' {
                start = i + 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (start, col)
    };
    if delta < 0 {
        if line_start == 0 {
            state.cursor = 0;
            return;
        }
        // line_start - 1 is the '\n' ending the previous line.
        let prev_line_end = line_start - 1;
        let mut prev_line_start = 0;
        for (i, ch) in state.buffer[..prev_line_end].iter().enumerate() {
            if *ch == '\n' {
                prev_line_start = i + 1;
            }
        }
        let prev_line_len = prev_line_end - prev_line_start;
        state.cursor = prev_line_start + col.min(prev_line_len);
    } else {
        // Find end of current line.
        let mut line_end = state.cursor;
        while line_end < state.buffer.len() && state.buffer[line_end] != '\n' {
            line_end += 1;
        }
        if line_end >= state.buffer.len() {
            state.cursor = state.buffer.len();
            return;
        }
        // Next line starts at line_end + 1.
        let next_line_start = line_end + 1;
        let mut next_line_end = next_line_start;
        while next_line_end < state.buffer.len() && state.buffer[next_line_end] != '\n' {
            next_line_end += 1;
        }
        let next_line_len = next_line_end - next_line_start;
        state.cursor = next_line_start + col.min(next_line_len);
    }
}

fn handle_env_edit_key(app: &mut App, key: KeyEvent) {
    let Some(state) = app.env_edit.as_mut() else {
        app.focus = Focus::Prompt;
        return;
    };

    match &state.input_mode {
        EnvInputMode::Filter => {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    state.input_mode = EnvInputMode::None;
                }
                KeyCode::Char(c) => {
                    state.filter.push(c);
                    let len = state.entries().len();
                    state.cursor = state.cursor.min(len.saturating_sub(1));
                }
                KeyCode::Backspace => {
                    state.filter.pop();
                }
                _ => {}
            }
            return;
        }
        EnvInputMode::Edit(_) | EnvInputMode::Add => {
            match key.code {
                KeyCode::Esc => {
                    state.input_mode = EnvInputMode::None;
                    state.input_buffer.clear();
                }
                KeyCode::Enter => {
                    commit_env_input(app);
                }
                KeyCode::Char(c) => state.input_buffer.push(c),
                KeyCode::Backspace => {
                    state.input_buffer.pop();
                }
                _ => {}
            }
            return;
        }
        EnvInputMode::None => {}
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.env_edit = None;
            app.focus = Focus::Prompt;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.cursor = state.cursor.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let len = state.entries().len();
            if len > 0 {
                state.cursor = (state.cursor + 1).min(len - 1);
            }
        }
        KeyCode::Char('/') => {
            state.input_mode = EnvInputMode::Filter;
        }
        KeyCode::Char('a') => {
            state.input_mode = EnvInputMode::Add;
            state.input_buffer.clear();
        }
        KeyCode::Char('e') | KeyCode::Enter => {
            let entries = state.entries();
            if let Some((k, v)) = entries.get(state.cursor) {
                state.input_mode = EnvInputMode::Edit(k.clone());
                state.input_buffer = v.clone();
            }
        }
        KeyCode::Char('d') | KeyCode::Delete => {
            let entries = state.entries();
            if let Some((k, _)) = entries.get(state.cursor) {
                let k = k.clone();
                // SAFETY: see export builtin's note.
                unsafe {
                    std::env::remove_var(&k);
                }
                let new_len = state.entries().len();
                state.cursor = state.cursor.min(new_len.saturating_sub(1));
                app.flash = Some(format!("unset {k}"));
            }
        }
        _ => {}
    }
}

fn commit_env_input(app: &mut App) {
    let Some(state) = app.env_edit.as_mut() else {
        return;
    };
    let mode = std::mem::replace(&mut state.input_mode, EnvInputMode::None);
    let buffer = std::mem::take(&mut state.input_buffer);
    let flash = match mode {
        EnvInputMode::Edit(key) => {
            // SAFETY: see export builtin's note.
            unsafe {
                std::env::set_var(&key, &buffer);
            }
            Some(format!("set {key}={buffer}"))
        }
        EnvInputMode::Add => match buffer.split_once('=') {
            Some((k, v)) if !k.is_empty() => {
                unsafe {
                    std::env::set_var(k, v);
                }
                Some(format!("set {k}={v}"))
            }
            _ => {
                state.input_buffer = buffer;
                state.input_mode = EnvInputMode::Add;
                Some("expected KEY=VALUE".into())
            }
        },
        _ => None,
    };
    if let Some(msg) = flash {
        app.flash = Some(msg);
    }
}

/// `export KEY=VALUE [KEY=VALUE ...]` sets one or more environment vars in
/// shed's process; subsequent spawned commands inherit them. Bare `export
/// KEY` (without `=`) is rejected — shed doesn't have POSIX's marked-for-
/// export distinction since every var we hold is automatically inherited.
fn run_export_builtin(app: &mut App, argv: &[String]) {
    app.savepoint();
    let id = app.session.add_shed(argv.to_vec());
    if argv.len() < 2 {
        app.session.set_state(
            id,
            ShedState::Failed("export: usage: export KEY=VALUE [...]".into()),
        );
        return;
    }
    let mut errors: Vec<String> = Vec::new();
    let mut set: Vec<String> = Vec::new();
    for arg in &argv[1..] {
        match arg.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                // SAFETY: env mutation has thread-safety concerns documented in
                // std::env::set_var. shed sets vars only from the main event
                // loop; tokio reader tasks capture cwd/env at spawn time and
                // don't read from std::env afterward.
                unsafe {
                    std::env::set_var(key, value);
                }
                set.push(key.to_string());
            }
            _ => errors.push(format!("invalid arg: {arg}")),
        }
    }
    if errors.is_empty() {
        app.session.set_state(id, ShedState::Done(0));
        if !set.is_empty() {
            app.flash = Some(format!("export {}", set.join(", ")));
        }
    } else {
        app.session.set_state(
            id,
            ShedState::Failed(format!("export: {}", errors.join("; "))),
        );
    }
}

fn run_unset_builtin(app: &mut App, argv: &[String]) {
    app.savepoint();
    let id = app.session.add_shed(argv.to_vec());
    if argv.len() < 2 {
        app.session.set_state(
            id,
            ShedState::Failed("unset: usage: unset NAME [NAME ...]".into()),
        );
        return;
    }
    for name in &argv[1..] {
        // SAFETY: see export above.
        unsafe {
            std::env::remove_var(name);
        }
    }
    app.session.set_state(id, ShedState::Done(0));
    app.flash = Some(format!("unset {}", argv[1..].join(", ")));
}

fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(s)
}

fn open_alias_manage(app: &mut App) {
    app.alias_manage = Some(AliasManageState::default());
    app.focus = Focus::AliasManage;
}

fn handle_alias_manage_key(app: &mut App, key: KeyEvent) {
    let total = app.aliases.aliases.len();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.alias_manage = None;
            app.focus = Focus::Prompt;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(state) = app.alias_manage.as_mut() {
                state.cursor = state.cursor.saturating_sub(1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(state) = app.alias_manage.as_mut()
                && total > 0
            {
                state.cursor = (state.cursor + 1).min(total - 1);
            }
        }
        KeyCode::Char('x') | KeyCode::Char('d') | KeyCode::Delete => {
            let cursor = app.alias_manage.as_ref().map(|s| s.cursor).unwrap_or(0);
            if let Some(alias) = app.aliases.aliases.get(cursor).cloned()
                && app.aliases.delete(&alias.name)
            {
                persist_aliases(app);
                let new_total = app.aliases.aliases.len();
                if let Some(state) = app.alias_manage.as_mut() {
                    if new_total > 0 {
                        state.cursor = state.cursor.min(new_total - 1);
                    } else {
                        state.cursor = 0;
                    }
                }
                app.flash = Some(format!("deleted alias {}", alias.name));
            }
        }
        _ => {}
    }
}

/// Validation for alias names: non-empty, no whitespace, doesn't start
/// with `@` (reserved for pinned-shed snapshots), doesn't start with
/// `/` (reserved for slash commands like `/aliases`), doesn't start
/// with `!` (reserved for force-fullscreen). Returns the trimmed name
/// or an error message.
fn validate_alias_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("name required".into());
    }
    if name.chars().any(char::is_whitespace) {
        return Err("name can't contain whitespace".into());
    }
    if name.starts_with('@') || name.starts_with('/') || name.starts_with('!') {
        return Err(format!(
            "name can't start with `{}`",
            name.chars().next().unwrap()
        ));
    }
    Ok(name.to_string())
}

/// Capture the cursor shed as an Alias — argv + pipeline copied. Used
/// by `A` and the palette action.
fn build_alias_from_cursor(app: &App, name: String) -> Option<Alias> {
    let id = app.session.cursor()?;
    let shed = app.session.shed(id)?;
    if shed.argv.is_empty() {
        return None;
    }
    Some(Alias {
        name,
        argv: shed.argv.clone(),
        pipeline: shed.pipeline.clone(),
    })
}

fn open_alias_save(app: &mut App) {
    if app.session.cursor().is_none() {
        app.flash = Some("no shed selected".into());
        return;
    }
    app.open_input(InputKind::AliasName, String::new());
}

fn commit_alias_save(app: &mut App) {
    let raw = app.take_input_text();
    let name = match validate_alias_name(&raw) {
        Ok(n) => n,
        Err(e) => {
            app.flash = Some(e);
            return;
        }
    };
    let Some(alias) = build_alias_from_cursor(app, name.clone()) else {
        app.flash = Some("nothing to save".into());
        return;
    };
    if app.aliases.lookup(&name).is_some() {
        // Defer to overwrite prompt.
        app.alias_overwrite = Some(alias);
        return;
    }
    app.aliases.upsert(alias);
    persist_aliases(app);
    app.flash = Some(format!("saved alias {name}"));
}

fn confirm_alias_overwrite(app: &mut App, accept: bool) {
    let Some(alias) = app.alias_overwrite.take() else {
        return;
    };
    if accept {
        let name = alias.name.clone();
        app.aliases.upsert(alias);
        persist_aliases(app);
        app.flash = Some(format!("overwrote alias {name}"));
    } else {
        app.flash = Some("alias save cancelled".into());
    }
}

/// Materialise an alias as a fresh Idle shed and drop the user into
/// in-place command edit so they can append args before running.
fn spawn_alias(app: &mut App, alias: &Alias) {
    app.savepoint();
    let id = app.session.add_shed(alias.argv.clone());
    if let Some(shed) = app.session.shed_mut(id) {
        shed.pipeline = alias.pipeline.clone();
    }
    app.session.set_state(id, ShedState::Idle);
    app.session.set_cursor(Some(id));
    app.reset_pipeline_cursor();
    app.command_focused = true;
    app.focus = Focus::ShedCursor;

    let joined = shlex::try_join(alias.argv.iter().map(String::as_str))
        .unwrap_or_else(|_| alias.argv.join(" "));
    app.open_input(InputKind::CmdEdit, format!("{joined} "));
}

/// Return the index of the menu item under (col, row), or `None` if the
/// coordinates fall outside the menu's rendered rows.
fn menu_item_at(app: &App, col: u16, row: u16) -> Option<usize> {
    let menu = app.context_menu.as_ref()?;
    if menu.items.is_empty() {
        return None;
    }
    let frame_x = menu.pos.0;
    let frame_y = menu.pos.1;
    let inner_width: u16 = menu
        .items
        .iter()
        .map(|i| i.label.chars().count() as u16)
        .max()
        .unwrap_or(1);
    let width = inner_width + 4;
    let height = menu.items.len() as u16 + 2;
    // The renderer may shift the menu to keep it in-frame; we don't know
    // the frame size here, so accept any column inside the unshifted box
    // as a hit. The mouse routing checks against the post-render rect for
    // dismissal; this is just a best-effort proximity check.
    if col < frame_x || col >= frame_x + width {
        return None;
    }
    if row <= frame_y || row + 1 > frame_y + height {
        return None;
    }
    let idx = (row - frame_y - 1) as usize;
    if idx < menu.items.len() {
        Some(idx)
    } else {
        None
    }
}

/// Dispatch a keypress while the context menu is open. Up/Down navigate,
/// Enter activates, Esc dismisses. Anything else just dismisses so the
/// menu doesn't trap focus.
fn handle_context_menu_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(menu) = app.context_menu.as_mut() {
                if menu.selected == 0 {
                    menu.selected = menu.items.len().saturating_sub(1);
                } else {
                    menu.selected -= 1;
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(menu) = app.context_menu.as_mut() {
                if menu.items.is_empty() {
                    return;
                }
                menu.selected = (menu.selected + 1) % menu.items.len();
            }
        }
        KeyCode::Enter => activate_menu_item(app),
        KeyCode::Esc => {
            app.context_menu = None;
        }
        _ => {
            app.context_menu = None;
        }
    }
}

/// Apply the currently-selected menu item's action and close the menu.
fn activate_menu_item(app: &mut App) {
    let Some(menu) = app.context_menu.take() else {
        return;
    };
    let Some(item) = menu.items.into_iter().nth(menu.selected) else {
        return;
    };
    match item.action {
        ContextMenuAction::CopyText(text) => match write_clipboard_osc52(&text) {
            Ok(()) => app.flash = Some(format!("copied {} bytes", text.len())),
            Err(e) => app.flash = Some(format!("clipboard write failed: {e}")),
        },
        ContextMenuAction::InsertAtPrompt(text) => {
            insert_at_prompt(app, &text);
        }
    }
}

/// Insert `text` at the current prompt cursor position. Silently does
/// nothing if focus isn't on the prompt — the menu only offers this
/// item when focus is on the prompt.
fn insert_at_prompt(app: &mut App, text: &str) {
    if app.focus != Focus::Prompt {
        return;
    }
    let cur = app.prompt_cursor.min(app.prompt.len());
    let mut new_text = String::with_capacity(app.prompt.len() + text.len());
    new_text.push_str(&app.prompt[..cur]);
    new_text.push_str(text);
    new_text.push_str(&app.prompt[cur..]);
    app.prompt = new_text;
    app.prompt_cursor = cur + text.len();
}

fn collapse_home_in_path(p: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok())
        && let Some(s) = p.to_str()
        && let Some(rest) = s.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    p.display().to_string()
}

/// Apply a pipeline of filters. Returns the final value plus per-filter
/// drop counts (rows silently filtered by a `where` due to type mismatch),
/// indexed by filter position in the pipeline.
fn apply_pipeline(
    capture: &Capture,
    pipeline: &[FilterSpec],
) -> Result<(PipelineValue, Vec<usize>), String> {
    let mut value = match &capture.structured {
        Some(v) => PipelineValue::Structured(v.clone()),
        None => PipelineValue::Bytes(capture.stdout.clone()),
    };
    let mut drops: Vec<usize> = vec![0; pipeline.len()];
    for (i, filter) in pipeline.iter().enumerate() {
        match apply_with_notes(filter, value) {
            Ok((v, n)) => {
                drops[i] = n.error_drops;
                value = v;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok((value, drops))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use shed_core::Capture;
    use std::time::Instant;

    fn shed_with_stdout(bytes: &[u8]) -> Shed {
        Shed {
            id: ShedId(1),
            name: None,
            argv: vec!["test".into()],
            capture: Some(Capture {
                stdout: Bytes::copy_from_slice(bytes),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: false,
                structured: None,
            }),
            pipeline: Vec::new(),
            state: ShedState::Done(0),
            last_touched: Instant::now(),
            pre_text: None,
            post_text: None,
            outputs: indexmap::IndexMap::new(),
            output_values: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn schema_empty_for_bytes_input() {
        let shed = shed_with_stdout(b"a\nb\nc\n");
        assert!(compute_schema_at(&shed, 0).is_empty());
    }

    #[test]
    fn schema_has_line_after_from_lines() {
        let mut shed = shed_with_stdout(b"a\nb\nc\n");
        shed.pipeline.push(FilterSpec::FromLines);
        assert_eq!(compute_schema_at(&shed, 1), vec!["line".to_string()]);
    }

    #[test]
    fn schema_at_index_uses_filters_before_only() {
        let mut shed = shed_with_stdout(b"a\nb\n");
        shed.pipeline.push(FilterSpec::FromLines);
        shed.pipeline.push(FilterSpec::Where {
            predicate: Predicate::Matches {
                column: "line".into(),
                pattern: "a".into(),
            },
        });
        // Schema BEFORE the where filter — only from-lines applied.
        assert_eq!(compute_schema_at(&shed, 1), vec!["line".to_string()]);
        // Schema BEFORE from-lines — bytes, no schema.
        assert!(compute_schema_at(&shed, 0).is_empty());
    }

    #[test]
    fn filter_edit_state_picks_parser_when_input_is_bytes() {
        let shed = shed_with_stdout(b"a\nb\nc\n");
        let state = FilterEditState::for_add(&shed);
        assert_eq!(state.kind, FilterKind::FromLines);
        assert_eq!(state.mode, EditMode::Add);
    }

    #[test]
    fn filter_edit_state_picks_where_when_schema_available() {
        let mut shed = shed_with_stdout(b"a\nb\nc\n");
        shed.pipeline.push(FilterSpec::FromLines);
        let state = FilterEditState::for_add(&shed);
        assert_eq!(state.kind, FilterKind::Where);
        assert_eq!(state.available_columns, vec!["line".to_string()]);
    }

    #[test]
    fn build_filter_for_where_requires_column() {
        let shed = shed_with_stdout(b"a\n");
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::Where;
        assert!(state.build_filter().is_none());
    }

    #[test]
    fn find_matches_regex_returns_indices_of_matching_lines() {
        let lines: Vec<Line<'static>> = vec![
            Line::from("apple"),
            Line::from("banana"),
            Line::from("apple pie"),
            Line::from("cherry"),
        ];
        let r = regex::Regex::new("apple").unwrap();
        assert_eq!(find_matches_regex(&lines, &r), vec![0, 2]);
        let r2 = regex::Regex::new("z").unwrap();
        assert_eq!(find_matches_regex(&lines, &r2), Vec::<usize>::new());
        // Regex semantics: `.` matches any char.
        let r3 = regex::Regex::new("a..le").unwrap();
        assert_eq!(find_matches_regex(&lines, &r3), vec![0, 2]);
        // Anchored regex.
        let r4 = regex::Regex::new("^a").unwrap();
        assert_eq!(find_matches_regex(&lines, &r4), vec![0, 2]);
    }

    #[test]
    fn try_compile_handles_empty_and_invalid() {
        assert!(try_compile("", false).is_none());
        assert!(try_compile("[unclosed", false).is_none());
        assert!(try_compile("valid", false).is_some());
        assert!(try_compile("\\d+", false).is_some());
    }

    #[test]
    fn try_compile_case_insensitive_prefix_works() {
        let r = try_compile("error", true).unwrap();
        assert!(r.is_match("ERROR"));
        assert!(r.is_match("Error"));
        assert!(r.is_match("error"));
        let r2 = try_compile("error", false).unwrap();
        assert!(!r2.is_match("ERROR"));
        assert!(r2.is_match("error"));
    }

    #[test]
    fn highlight_matches_only_styles_matched_substrings() {
        let line = Line::from(vec![
            Span::styled("hello ", Style::default().fg(Color::Red)),
            Span::styled("world", Style::default().fg(Color::Blue)),
        ]);
        let r = regex::Regex::new("ell").unwrap();
        let highlighted = highlight_matches_in_line(line, &r);
        // We expect: "h" (red), "ell" (red+REVERSED), "o " (red), "world" (blue)
        let texts: Vec<String> = highlighted
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(texts.join(""), "hello world");
        // The "ell" span has the REVERSED modifier set.
        let ell = highlighted
            .spans
            .iter()
            .find(|s| s.content == "ell")
            .expect("ell span");
        assert!(ell.style.add_modifier.contains(Modifier::REVERSED));
        // Other spans don't have REVERSED.
        for span in &highlighted.spans {
            if span.content != "ell" {
                assert!(!span.style.add_modifier.contains(Modifier::REVERSED));
            }
        }
    }

    #[test]
    fn highlight_matches_handles_match_spanning_two_spans() {
        let line = Line::from(vec![
            Span::styled("hel", Style::default().fg(Color::Red)),
            Span::styled("low", Style::default().fg(Color::Blue)),
        ]);
        // "ello" spans the boundary between "hel" and "low".
        let r = regex::Regex::new("ello").unwrap();
        let highlighted = highlight_matches_in_line(line, &r);
        let reversed_text: String = highlighted
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(reversed_text, "ello");
    }

    #[test]
    fn highlight_matches_no_match_returns_line_unchanged() {
        let line = Line::from(vec![Span::raw("nothing matches here")]);
        let r = regex::Regex::new("xyz").unwrap();
        let result = highlight_matches_in_line(line, &r);
        assert_eq!(result.spans.len(), 1);
        assert!(
            !result.spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn line_text_concatenates_spans() {
        let line = Line::from(vec![
            Span::raw("hello "),
            Span::styled("world", Style::default().fg(Color::Red)),
        ]);
        assert_eq!(line_text(&line), "hello world");
    }

    fn make_history_app() -> App {
        let mut app = App::new();
        app.history = vec!["one".into(), "two".into(), "three".into()];
        app
    }

    #[test]
    fn history_up_walks_back_from_newest() {
        let mut app = make_history_app();
        history_step(&mut app, -1);
        assert_eq!(app.prompt, "three");
        history_step(&mut app, -1);
        assert_eq!(app.prompt, "two");
        history_step(&mut app, -1);
        assert_eq!(app.prompt, "one");
        // Clamps at the oldest.
        history_step(&mut app, -1);
        assert_eq!(app.prompt, "one");
    }

    #[test]
    fn history_down_returns_to_empty_buffer() {
        let mut app = make_history_app();
        history_step(&mut app, -1); // "three"
        history_step(&mut app, -1); // "two"
        history_step(&mut app, 1); // back to "three"
        assert_eq!(app.prompt, "three");
        history_step(&mut app, 1); // past "three" → empty
        assert_eq!(app.prompt, "");
        assert!(app.history_cursor.is_none());
    }

    #[test]
    fn history_persistence_round_trips_via_tmpfile() {
        let dir = std::env::temp_dir().join(format!(
            "shed-history-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("history");
        // Append three entries.
        append_history_to(&path, "first").unwrap();
        append_history_to(&path, "second").unwrap();
        append_history_to(&path, "third").unwrap();
        // Load them back.
        let loaded = load_history_from(&path).unwrap();
        assert_eq!(loaded, vec!["first", "second", "third"]);
        // Cleanup.
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn history_load_missing_file_returns_none() {
        let path = std::env::temp_dir().join(format!(
            "shed-history-nonexistent-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(load_history_from(&path).is_none());
    }

    #[test]
    fn palette_matches_filter_by_word_substrings() {
        let mut app = App::new();
        app.history.clear();
        // No cursor, no sheds — only always-enabled actions and "any
        // sheds" actions show. Newest-shed one wouldn't (no sheds).
        let all = matches_for_input("", &app);
        assert!(all.iter().any(|a| a.name == "Quit shed"));
        assert!(all.iter().any(|a| a.name == "Open env editor"));
        assert!(!all.iter().any(|a| a.name == "Focus newest shed"));

        // Multi-word substring match.
        let env = matches_for_input("env editor", &app);
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].name, "Open env editor");

        // Case-insensitive.
        let q = matches_for_input("QUIT", &app);
        assert!(q.iter().any(|a| a.name == "Quit shed"));

        // Words can match in any order across the name.
        let m = matches_for_input("editor env", &app);
        assert!(m.iter().any(|a| a.name == "Open env editor"));
    }

    #[test]
    fn write_format_inferred_from_extension() {
        assert!(matches!(
            WriteFormat::from_path("foo.csv"),
            WriteFormat::Csv(b',')
        ));
        assert!(matches!(
            WriteFormat::from_path("foo.CSV"),
            WriteFormat::Csv(b',')
        ));
        assert!(matches!(
            WriteFormat::from_path("foo.tsv"),
            WriteFormat::Csv(b'\t')
        ));
        assert!(matches!(
            WriteFormat::from_path("foo.json"),
            WriteFormat::Json
        ));
        assert!(matches!(
            WriteFormat::from_path("foo.txt"),
            WriteFormat::Plain
        ));
        assert!(matches!(WriteFormat::from_path("foo"), WriteFormat::Plain));
    }

    #[test]
    fn json_render_of_structured_list_of_records() {
        // Build a structured list-of-records by hand and serialize it.
        use shed_core::Value;
        let mut rec1 = indexmap::IndexMap::new();
        rec1.insert("name".to_string(), Value::String("alice".into()));
        rec1.insert("age".to_string(), Value::Int(30));
        let mut rec2 = indexmap::IndexMap::new();
        rec2.insert("name".to_string(), Value::String("bob".into()));
        rec2.insert("age".to_string(), Value::Int(25));
        let v =
            PipelineValue::Structured(Value::List(vec![Value::Record(rec1), Value::Record(rec2)]));
        let json = pipeline_value_to_json(v);
        let s = serde_json::to_string(&json).unwrap();
        // Structure check via round-trip.
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "alice");
        assert_eq!(arr[0]["age"], 30);
        assert_eq!(arr[1]["name"], "bob");
        assert_eq!(arr[1]["age"], 25);
    }

    #[test]
    fn expand_tilde_handles_bare_and_prefix_forms() {
        // Use a known HOME for the test by stashing/restoring.
        let saved = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/tmp/shed-tilde-test");
        }
        assert_eq!(expand_tilde("~"), PathBuf::from("/tmp/shed-tilde-test"));
        assert_eq!(
            expand_tilde("~/foo"),
            PathBuf::from("/tmp/shed-tilde-test/foo")
        );
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_tilde("rel"), PathBuf::from("rel"));
        // Restore.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn history_load_filters_empty_lines() {
        let dir = std::env::temp_dir().join(format!(
            "shed-history-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history");
        std::fs::write(&path, "first\n\nsecond\n\n").unwrap();
        let loaded = load_history_from(&path).unwrap();
        assert_eq!(loaded, vec!["first", "second"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn history_step_with_empty_history_is_a_noop() {
        let mut app = App::new();
        // App::new now loads persisted history; clear for this test.
        app.history.clear();
        app.history_cursor = None;
        app.prompt = "typed".into();
        history_step(&mut app, -1);
        assert_eq!(app.prompt, "typed");
        history_step(&mut app, 1);
        assert_eq!(app.prompt, "typed");
    }

    #[test]
    fn needs_fullscreen_for_blacklisted_program() {
        assert!(needs_fullscreen(&["top".into()]));
        assert!(needs_fullscreen(&["vim".into(), "file.txt".into()]));
        assert!(needs_fullscreen(&["less".into()]));
    }

    #[test]
    fn needs_fullscreen_uses_basename() {
        assert!(needs_fullscreen(&["/usr/bin/vim".into()]));
        assert!(needs_fullscreen(&["/opt/htop".into()]));
    }

    #[test]
    fn needs_fullscreen_false_for_capture_friendly() {
        assert!(!needs_fullscreen(&["ls".into()]));
        assert!(!needs_fullscreen(&["seq".into(), "1".into(), "10".into()]));
        assert!(!needs_fullscreen(&["cargo".into(), "build".into()]));
    }

    #[test]
    fn cycle_kind_walks_through_all_kinds() {
        let mut shed = shed_with_stdout(b"a\n");
        shed.pipeline.push(FilterSpec::FromLines);
        let mut state = FilterEditState::for_add(&shed);
        assert_eq!(state.kind, FilterKind::Where);
        // Walks once around the cycle and lands back on Where.
        for _ in 0..FilterKind::ALL.len() {
            state.cycle_kind(1);
        }
        assert_eq!(state.kind, FilterKind::Where);
    }

    #[test]
    fn build_filter_sort_by_single_key() {
        let mut shed = shed_with_stdout(b"a\n");
        shed.pipeline.push(FilterSpec::FromLines);
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::SortBy;
        state.sort_keys = vec![(0, SortDirection::Desc)];
        match state.build_filter() {
            Some(FilterSpec::SortBy { keys }) => {
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0].column, "line");
                assert_eq!(keys[0].direction, SortDirection::Desc);
            }
            _ => panic!("expected SortBy"),
        }
    }

    #[test]
    fn build_filter_sort_by_multi_key_preserves_order() {
        let mut shed = shed_with_stdout(b"a b c\n");
        shed.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::SortBy;
        state.sort_keys = vec![
            (1, SortDirection::Asc),
            (0, SortDirection::Desc),
            (2, SortDirection::Asc),
        ];
        match state.build_filter() {
            Some(FilterSpec::SortBy { keys }) => {
                assert_eq!(keys.len(), 3);
                assert_eq!(keys[0].column, "_2");
                assert_eq!(keys[0].direction, SortDirection::Asc);
                assert_eq!(keys[1].column, "_1");
                assert_eq!(keys[1].direction, SortDirection::Desc);
                assert_eq!(keys[2].column, "_3");
            }
            _ => panic!("expected SortBy with 3 keys"),
        }
    }

    #[test]
    fn build_filter_sort_by_empty_keys_is_none() {
        let shed = shed_with_stdout(b"x\n");
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::SortBy;
        state.sort_keys.clear();
        assert!(state.build_filter().is_none());
    }

    #[test]
    fn build_filter_uniq_no_columns_means_full_dedupe() {
        let mut shed = shed_with_stdout(b"a b\n");
        shed.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::Uniq;
        match state.build_filter() {
            Some(FilterSpec::Uniq { by }) => assert!(by.is_none()),
            _ => panic!("expected Uniq with by=None"),
        }
        state.column_selections[0] = true;
        match state.build_filter() {
            Some(FilterSpec::Uniq { by: Some(cols) }) => assert_eq!(cols, vec!["_1".to_string()]),
            _ => panic!("expected Uniq with by=Some([_1])"),
        }
    }

    #[test]
    fn build_filter_rename_collects_only_nonempty_rows() {
        let mut shed = shed_with_stdout(b"a b c\n");
        shed.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::Rename;
        // available_columns = [_1, _2, _3]; rename_to_inputs starts as 3 empties.
        state.rename_to_inputs[0] = "file".into();
        state.rename_to_inputs[2] = "owner".into();
        match state.build_filter() {
            Some(FilterSpec::Rename { pairs }) => {
                assert_eq!(pairs.len(), 2);
                assert_eq!(pairs[0], ("_1".into(), "file".into()));
                assert_eq!(pairs[1], ("_3".into(), "owner".into()));
            }
            _ => panic!("expected Rename"),
        }
    }

    #[test]
    fn build_filter_rename_empty_is_none() {
        let mut shed = shed_with_stdout(b"a b\n");
        shed.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::Rename;
        // All inputs empty.
        assert!(state.build_filter().is_none());
    }

    #[test]
    fn for_edit_prepopulates_rename() {
        let mut shed = shed_with_stdout(b"a b c\n");
        shed.pipeline.push(FilterSpec::FromFields);
        shed.pipeline.push(FilterSpec::Rename {
            pairs: vec![("_2".into(), "size".into())],
        });
        let state = FilterEditState::for_edit(&shed, 1);
        assert_eq!(state.kind, FilterKind::Rename);
        assert_eq!(state.available_columns, vec!["_1", "_2", "_3"]);
        assert_eq!(state.rename_to_inputs, vec!["", "size", ""]);
    }

    #[test]
    fn for_edit_prepopulates_multi_key_sort_by() {
        let mut shed = shed_with_stdout(b"a b c\n");
        shed.pipeline.push(FilterSpec::FromFields);
        shed.pipeline.push(FilterSpec::SortBy {
            keys: vec![
                SortKey {
                    column: "_3".into(),
                    direction: SortDirection::Desc,
                },
                SortKey {
                    column: "_1".into(),
                    direction: SortDirection::Asc,
                },
            ],
        });
        let state = FilterEditState::for_edit(&shed, 1);
        assert_eq!(state.kind, FilterKind::SortBy);
        assert_eq!(state.available_columns, vec!["_1", "_2", "_3"]);
        assert_eq!(state.sort_keys.len(), 2);
        assert_eq!(state.sort_keys[0], (2, SortDirection::Desc));
        assert_eq!(state.sort_keys[1], (0, SortDirection::Asc));
    }

    #[test]
    fn for_edit_prepopulates_from_csv() {
        let mut shed = shed_with_stdout(b"a,b\n1,2\n");
        shed.pipeline.push(FilterSpec::FromCsv {
            delim: ';',
            has_header: false,
        });
        let state = FilterEditState::for_edit(&shed, 0);
        assert_eq!(state.kind, FilterKind::FromCsv);
        assert_eq!(state.csv_delim, ';');
        assert!(!state.csv_has_header);
    }

    #[test]
    fn for_edit_prepopulates_from_regex() {
        let mut shed = shed_with_stdout(b"x\n");
        shed.pipeline.push(FilterSpec::FromRegex {
            pattern: r"(?<k>\w+)".into(),
        });
        let state = FilterEditState::for_edit(&shed, 0);
        assert_eq!(state.kind, FilterKind::FromRegex);
        assert_eq!(state.regex_pattern, r"(?<k>\w+)");
    }

    #[test]
    fn build_filter_from_regex_requires_pattern() {
        let shed = shed_with_stdout(b"x\n");
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::FromRegex;
        assert!(state.build_filter().is_none());
        state.regex_pattern = "abc".into();
        match state.build_filter() {
            Some(FilterSpec::FromRegex { pattern }) => assert_eq!(pattern, "abc"),
            _ => panic!("expected FromRegex"),
        }
    }

    #[test]
    fn cycle_delim_walks_choices() {
        let shed = shed_with_stdout(b"x\n");
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::FromCsv;
        assert_eq!(state.csv_delim, ',');
        state.cycle_delim(1);
        assert_eq!(state.csv_delim, '\t');
        state.cycle_delim(1);
        assert_eq!(state.csv_delim, ';');
        state.cycle_delim(-1);
        assert_eq!(state.csv_delim, '\t');
    }

    #[test]
    fn build_filter_take_requires_valid_n() {
        let mut shed = shed_with_stdout(b"a\n");
        shed.pipeline.push(FilterSpec::FromLines);
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::Take;
        assert!(state.build_filter().is_none());
        state.n_input = "5".into();
        match state.build_filter() {
            Some(FilterSpec::Take { n }) => assert_eq!(n, 5),
            _ => panic!("expected Take with n=5"),
        }
        state.n_input = "abc".into();
        assert!(state.build_filter().is_none());
    }

    #[test]
    fn build_filter_select_requires_at_least_one_column() {
        let mut shed = shed_with_stdout(b"a b\n");
        shed.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::Select;
        assert!(state.build_filter().is_none());
        state.column_selections[0] = true;
        match state.build_filter() {
            Some(FilterSpec::Select { columns }) => {
                assert_eq!(columns, vec!["_1".to_string()]);
            }
            _ => panic!("expected Select with [_1]"),
        }
    }

    #[test]
    fn for_edit_prepopulates_take() {
        let mut shed = shed_with_stdout(b"a\nb\nc\n");
        shed.pipeline.push(FilterSpec::FromLines);
        shed.pipeline.push(FilterSpec::Take { n: 2 });
        let state = FilterEditState::for_edit(&shed, 1);
        assert_eq!(state.kind, FilterKind::Take);
        assert_eq!(state.n_input, "2");
    }

    #[test]
    fn build_filter_where_single_clause_uses_op_for_predicate_kind() {
        let mut shed = shed_with_stdout(b"a\n");
        shed.pipeline.push(FilterSpec::FromLines);
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::Where;

        state.where_clauses[0].op = WhereOp::Matches;
        state.where_clauses[0].pattern = "abc".into();
        match state.build_filter() {
            Some(FilterSpec::Where {
                predicate: Predicate::Matches { pattern, .. },
            }) => {
                assert_eq!(pattern, "abc");
            }
            _ => panic!("expected Matches"),
        }

        state.where_clauses[0].op = WhereOp::Contains;
        match state.build_filter() {
            Some(FilterSpec::Where {
                predicate: Predicate::Contains { substring, .. },
            }) => {
                assert_eq!(substring, "abc");
            }
            _ => panic!("expected Contains"),
        }

        state.where_clauses[0].op = WhereOp::Gt;
        state.where_clauses[0].pattern = "42".into();
        match state.build_filter() {
            Some(FilterSpec::Where {
                predicate: Predicate::Compare { op, value, .. },
            }) => {
                assert_eq!(op, CompareOp::Gt);
                assert_eq!(value, Value::Int(42));
            }
            _ => panic!("expected Compare(Gt, Int(42))"),
        }
    }

    #[test]
    fn build_filter_where_two_clauses_chains_with_combine() {
        let mut shed = shed_with_stdout(b"a b\n");
        shed.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&shed);
        state.kind = FilterKind::Where;
        state.where_clauses = vec![
            WhereClause {
                column: 0,
                op: WhereOp::Contains,
                pattern: "x".into(),
            },
            WhereClause {
                column: 1,
                op: WhereOp::Contains,
                pattern: "y".into(),
            },
        ];

        state.where_combine = WhereCombine::And;
        let pred = match state.build_filter() {
            Some(FilterSpec::Where { predicate }) => predicate,
            _ => panic!(),
        };
        match pred {
            Predicate::And(a, b) => {
                assert!(matches!(*a, Predicate::Contains { .. }));
                assert!(matches!(*b, Predicate::Contains { .. }));
            }
            _ => panic!("expected And"),
        }

        state.where_combine = WhereCombine::Or;
        let pred = match state.build_filter() {
            Some(FilterSpec::Where { predicate }) => predicate,
            _ => panic!(),
        };
        assert!(matches!(pred, Predicate::Or(_, _)));
    }

    #[test]
    fn for_edit_flattens_and_chain_into_clauses() {
        let mut shed = shed_with_stdout(b"a b\n");
        shed.pipeline.push(FilterSpec::FromFields);
        let p = Predicate::And(
            Box::new(Predicate::Matches {
                column: "_1".into(),
                pattern: "x".into(),
            }),
            Box::new(Predicate::And(
                Box::new(Predicate::Contains {
                    column: "_2".into(),
                    substring: "y".into(),
                }),
                Box::new(Predicate::Compare {
                    column: "_1".into(),
                    op: CompareOp::Gt,
                    value: Value::Int(0),
                }),
            )),
        );
        shed.pipeline.push(FilterSpec::Where { predicate: p });
        let state = FilterEditState::for_edit(&shed, 1);
        assert_eq!(state.kind, FilterKind::Where);
        assert_eq!(state.where_combine, WhereCombine::And);
        assert_eq!(state.where_clauses.len(), 3);
        assert_eq!(state.where_clauses[0].op, WhereOp::Matches);
        assert_eq!(state.where_clauses[1].op, WhereOp::Contains);
        assert_eq!(state.where_clauses[2].op, WhereOp::Gt);
    }

    #[test]
    fn parse_value_input_heuristics() {
        assert_eq!(parse_value_input("123"), Value::Int(123));
        assert_eq!(parse_value_input("-7"), Value::Int(-7));
        assert_eq!(parse_value_input("2.5"), Value::Float(2.5));
        assert_eq!(parse_value_input("true"), Value::Bool(true));
        assert_eq!(parse_value_input("false"), Value::Bool(false));
        assert_eq!(parse_value_input("hello"), Value::String("hello".into()));
        assert_eq!(parse_value_input("  42  "), Value::Int(42));
    }

    #[test]
    fn for_edit_prepopulates_compare_predicate() {
        let mut shed = shed_with_stdout(b"1\n2\n10\n");
        shed.pipeline.push(FilterSpec::FromLines);
        shed.pipeline.push(FilterSpec::Where {
            predicate: Predicate::Compare {
                column: "line".into(),
                op: CompareOp::Gt,
                value: Value::Int(5),
            },
        });
        let state = FilterEditState::for_edit(&shed, 1);
        assert_eq!(state.kind, FilterKind::Where);
        assert_eq!(state.where_clauses.len(), 1);
        assert_eq!(state.where_clauses[0].op, WhereOp::Gt);
        assert_eq!(state.where_clauses[0].pattern, "5");
    }

    #[test]
    fn for_edit_prepopulates_contains_predicate() {
        let mut shed = shed_with_stdout(b"hello\n");
        shed.pipeline.push(FilterSpec::FromLines);
        shed.pipeline.push(FilterSpec::Where {
            predicate: Predicate::Contains {
                column: "line".into(),
                substring: "ell".into(),
            },
        });
        let state = FilterEditState::for_edit(&shed, 1);
        assert_eq!(state.where_clauses[0].op, WhereOp::Contains);
        assert_eq!(state.where_clauses[0].pattern, "ell");
    }

    #[test]
    fn for_edit_prepopulates_select_columns() {
        let mut shed = shed_with_stdout(b"a b c\n");
        shed.pipeline.push(FilterSpec::FromFields);
        shed.pipeline.push(FilterSpec::Select {
            columns: vec!["_1".into(), "_3".into()],
        });
        let state = FilterEditState::for_edit(&shed, 1);
        assert_eq!(state.kind, FilterKind::Select);
        // available_columns at index 1 = [_1, _2, _3]; selections should mark _1 and _3
        assert_eq!(state.available_columns, vec!["_1", "_2", "_3"]);
        assert_eq!(state.column_selections, vec![true, false, true]);
    }

    #[test]
    fn for_edit_prepopulates_from_existing_where() {
        let mut shed = shed_with_stdout(b"a\nb\nbb\n");
        shed.pipeline.push(FilterSpec::FromLines);
        shed.pipeline.push(FilterSpec::Where {
            predicate: Predicate::Matches {
                column: "line".into(),
                pattern: "bb".into(),
            },
        });
        let state = FilterEditState::for_edit(&shed, 1);
        assert_eq!(state.kind, FilterKind::Where);
        assert_eq!(state.mode, EditMode::Edit(1));
        assert_eq!(state.where_clauses[0].pattern, "bb");
        assert_eq!(state.available_columns, vec!["line".to_string()]);
        assert_eq!(state.selected_column(), Some("line"));
    }

    #[test]
    fn for_edit_prepopulates_from_existing_from_lines() {
        let mut shed = shed_with_stdout(b"x\n");
        shed.pipeline.push(FilterSpec::FromLines);
        let state = FilterEditState::for_edit(&shed, 0);
        assert_eq!(state.kind, FilterKind::FromLines);
        assert_eq!(state.mode, EditMode::Edit(0));
    }

    #[test]
    fn for_insert_inserts_before_existing_filter() {
        let mut shed = shed_with_stdout(b"a\nb\nc\n");
        shed.pipeline.push(FilterSpec::FromLines);
        shed.pipeline.push(FilterSpec::Take { n: 5 });
        let state = FilterEditState::for_insert(&shed, 1);
        assert_eq!(state.mode, EditMode::Insert(1));
        // Schema is computed BEFORE index 1, i.e. after FromLines.
        assert_eq!(state.available_columns, vec!["line".to_string()]);
    }

    #[test]
    fn apply_filter_edit_insert_pushes_existing_right() {
        let mut shed = shed_with_stdout(b"a\n");
        shed.pipeline.push(FilterSpec::FromLines);
        shed.pipeline.push(FilterSpec::Take { n: 5 });
        // Insert a `where` between FromLines and Take.
        let mut state = FilterEditState::for_insert(&shed, 1);
        state.kind = FilterKind::Where;
        state.where_clauses[0].pattern = "x".into();
        // Direct pipeline mutation (mirrors apply_filter_edit's logic).
        let spec = state.build_filter().expect("buildable");
        match state.mode {
            EditMode::Insert(i) => shed.pipeline.insert(i, spec),
            _ => panic!("expected Insert"),
        }
        assert_eq!(shed.pipeline.len(), 3);
        assert!(matches!(shed.pipeline[0], FilterSpec::FromLines));
        assert!(matches!(shed.pipeline[1], FilterSpec::Where { .. }));
        assert!(matches!(shed.pipeline[2], FilterSpec::Take { n: 5 }));
    }

    #[test]
    fn save_and_load_round_trips_through_app() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("shed-tui-test-{}.json", std::process::id()));

        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["echo".into(), "hi".into()]);
        app.session
            .shed_mut(id)
            .unwrap()
            .pipeline
            .push(FilterSpec::FromLines);
        app.dirty = true;
        assert!(app.dirty);

        save_to_path(&mut app, &path);
        assert!(!app.dirty, "save clears dirty");
        assert_eq!(app.notebook_path.as_deref(), Some(path.as_path()));

        // Open into a fresh app: sheds come back as Idle with the pipeline intact.
        let mut other = App::new();
        other.history.clear();
        load_from_path(&mut other, &path);
        assert!(!other.dirty);
        let sheds: Vec<_> = other.session.sheds().collect();
        assert_eq!(sheds.len(), 1);
        assert_eq!(sheds[0].argv, vec!["echo", "hi"]);
        assert!(matches!(sheds[0].state, ShedState::Idle));
        assert_eq!(sheds[0].pipeline.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn run_cursor_shed_in_place_queues_pending_request() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["true".into()]);
        app.session.set_state(id, ShedState::Idle);
        app.session.set_cursor(Some(id));

        run_cursor_shed_in_place(&mut app);
        assert_eq!(app.pending_run_chain.front().copied(), Some(id));
    }

    #[test]
    fn build_run_chain_walks_at_ref_dep_first() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["seq".into(), "1".into(), "3".into()]);
        s.set_state(src, ShedState::Idle);
        s.pin(src, "src".into());

        let dep = s.add_shed(vec!["@src".into()]);
        s.set_state(dep, ShedState::Idle);

        let mut visited = HashSet::new();
        let chain = build_run_chain(&s, dep, &mut visited);
        assert_eq!(chain, vec![src, dep]);
    }

    #[test]
    fn build_run_chain_skips_done_source() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["seq".into(), "1".into(), "3".into()]);
        s.set_state(src, ShedState::Done(0));
        s.pin(src, "src".into());

        let dep = s.add_shed(vec!["@src".into()]);
        s.set_state(dep, ShedState::Idle);

        let mut visited = HashSet::new();
        let chain = build_run_chain(&s, dep, &mut visited);
        // Done sources are not re-run; only the dep itself goes in.
        assert_eq!(chain, vec![dep]);
    }

    #[test]
    fn build_run_chain_chains_two_levels_of_at_refs() {
        let mut s = Session::new();
        let root = s.add_shed(vec!["seq".into(), "1".into(), "3".into()]);
        s.set_state(root, ShedState::Idle);
        s.pin(root, "root".into());

        let mid = s.add_shed(vec!["@root".into()]);
        s.set_state(mid, ShedState::Idle);
        s.pin(mid, "mid".into());

        let leaf = s.add_shed(vec!["@mid".into()]);
        s.set_state(leaf, ShedState::Idle);

        let mut visited = HashSet::new();
        let chain = build_run_chain(&s, leaf, &mut visited);
        assert_eq!(chain, vec![root, mid, leaf]);
    }

    #[test]
    fn build_run_chain_handles_self_cycle() {
        let mut s = Session::new();
        let id = s.add_shed(vec!["@loop".into()]);
        s.set_state(id, ShedState::Idle);
        s.pin(id, "loop".into());

        let mut visited = HashSet::new();
        let chain = build_run_chain(&s, id, &mut visited);
        assert_eq!(chain, vec![id]);
    }

    #[test]
    fn advance_run_chain_aborts_dependents_when_prereq_fails() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["false".into()]);
        let b = app.session.add_shed(vec!["true".into()]);
        app.session.set_state(a, ShedState::Failed("boom".into()));
        app.chain_in_flight = Some(a);
        app.pending_run_chain.push_back(b);

        advance_run_chain(&mut app);
        assert!(app.pending_run_chain.is_empty(), "dependents dropped");
        assert!(app.chain_in_flight.is_none());
        assert!(app.flash.as_deref().unwrap_or("").contains("skipped"));
    }

    #[test]
    fn is_shed_ref_only_for_single_at_or_percent_token() {
        assert!(is_shed_ref(&["@logs".into()]));
        assert!(is_shed_ref(&["%5".into()]));
        assert!(!is_shed_ref(&["@logs".into(), "extra".into()]));
        assert!(!is_shed_ref(&["@".into()]));
        assert!(!is_shed_ref(&["%".into()]));
        assert!(!is_shed_ref(&["%abc".into()]));
        assert!(!is_shed_ref(&["logs".into()]));
        assert!(!is_shed_ref(&[]));
    }

    #[test]
    fn parse_shed_ref_recognizes_both_forms() {
        assert_eq!(parse_shed_ref("@logs"), Some(ShedRef::Name("logs".into())));
        assert_eq!(parse_shed_ref("%7"), Some(ShedRef::Id(ShedId(7))));
        assert_eq!(parse_shed_ref("@"), None);
        assert_eq!(parse_shed_ref("%"), None);
        assert_eq!(parse_shed_ref("%x"), None);
        assert_eq!(parse_shed_ref("logs"), None);
    }

    // === argv interpolation ===

    #[test]
    fn parse_interpolations_handles_each_shape() {
        // Plain literal — single Literal part, no interpolations.
        let parts = parse_interpolations("plain text").unwrap();
        assert_eq!(parts, vec![TokenPart::Literal("plain text".into())]);

        // Own output.
        let parts = parse_interpolations("${plan}").unwrap();
        assert_eq!(
            parts,
            vec![TokenPart::Interp(InterpRef::Own("plan".into()))]
        );

        // Source with field.
        let parts = parse_interpolations("${@src.path}").unwrap();
        assert_eq!(
            parts,
            vec![TokenPart::Interp(InterpRef::Source {
                source: ShedRef::Name("src".into()),
                output: Some("path".into()),
            })]
        );

        // Source no field (implicit stdout).
        let parts = parse_interpolations("${%3}").unwrap();
        assert_eq!(
            parts,
            vec![TokenPart::Interp(InterpRef::Source {
                source: ShedRef::Id(ShedId(3)),
                output: None,
            })]
        );

        // Mixed literal + interp.
        let parts = parse_interpolations("apply ${@p.f} now").unwrap();
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], TokenPart::Literal(s) if s == "apply "));
        assert!(matches!(&parts[2], TokenPart::Literal(s) if s == " now"));
    }

    #[test]
    fn parse_interpolations_rejects_malformed() {
        assert!(parse_interpolations("${").is_err(), "unterminated");
        assert!(parse_interpolations("${}").is_err(), "empty");
        assert!(
            parse_interpolations("${1bad}").is_err(),
            "starts with digit"
        );
        assert!(parse_interpolations("${@}").is_err(), "bare @");
    }

    #[test]
    fn argv_interp_sources_collects_unique_refs() {
        let argv = vec![
            "echo".into(),
            "${@a.x}".into(),
            "${%5}".into(),
            "${@a.y}".into(),  // duplicate source @a — only counted once
            "${plain}".into(), // own output — not a source dep
        ];
        let refs = argv_interp_sources(&argv);
        assert_eq!(
            refs,
            vec![ShedRef::Name("a".into()), ShedRef::Id(ShedId(5))]
        );
    }

    #[test]
    fn resolve_argv_substitutes_own_outputs() {
        let mut own = std::collections::HashMap::new();
        own.insert("plan".into(), "/tmp/plan-123".into());
        let s = Session::new();
        let out = resolve_argv(&["tofu".into(), "-out=${plan}".into()], &own, &s).unwrap();
        assert_eq!(out, vec!["tofu", "-out=/tmp/plan-123"]);
    }

    #[test]
    fn resolve_argv_errors_on_undefined_own_output() {
        let own = std::collections::HashMap::new();
        let s = Session::new();
        let err = resolve_argv(&["${nope}".into()], &own, &s).unwrap_err();
        assert!(err.contains("undefined own output"), "got: {err}");
    }

    #[test]
    fn resolve_argv_reads_source_named_output() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["echo".into()]);
        s.set_state(src, ShedState::Done(0));
        s.pin(src, "p".into());
        if let Some(b) = s.shed_mut(src) {
            b.output_values.insert("path".into(), "/tmp/plan".into());
        }
        let own = std::collections::HashMap::new();
        let out = resolve_argv(&["apply".into(), "${@p.path}".into()], &own, &s).unwrap();
        assert_eq!(out, vec!["apply", "/tmp/plan"]);
    }

    #[test]
    fn resolve_argv_reads_implicit_stdout_trimmed() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["echo".into()]);
        s.pin(src, "p".into());
        s.set_state(src, ShedState::Done(0));
        s.set_capture(
            src,
            Capture {
                stdout: Bytes::from_static(b"   hello world\n   "),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: false,
                structured: None,
            },
        );
        let own = std::collections::HashMap::new();
        let out = resolve_argv(&["echo".into(), "${@p}".into()], &own, &s).unwrap();
        assert_eq!(out, vec!["echo", "hello world"]);
    }

    #[test]
    fn resolve_argv_errors_when_source_not_yet_succeeded() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["echo".into()]);
        s.set_state(src, ShedState::Idle);
        s.pin(src, "p".into());
        let own = std::collections::HashMap::new();
        let err = resolve_argv(&["${@p.x}".into()], &own, &s).unwrap_err();
        assert!(err.contains("hasn't completed successfully"), "got: {err}");
    }

    #[test]
    fn resolve_argv_errors_on_failed_source() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["false".into()]);
        s.set_state(src, ShedState::Done(1));
        s.pin(src, "p".into());
        if let Some(b) = s.shed_mut(src) {
            b.output_values.insert("x".into(), "value".into());
        }
        let own = std::collections::HashMap::new();
        let err = resolve_argv(&["${@p.x}".into()], &own, &s).unwrap_err();
        assert!(err.contains("hasn't completed successfully"), "got: {err}");
    }

    #[test]
    fn resolve_argv_errors_on_undeclared_source_output() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["echo".into()]);
        s.set_state(src, ShedState::Done(0));
        s.pin(src, "p".into());
        let own = std::collections::HashMap::new();
        let err = resolve_argv(&["${@p.missing}".into()], &own, &s).unwrap_err();
        assert!(err.contains("doesn't define output"), "got: {err}");
    }

    #[test]
    fn build_run_chain_includes_interpolation_sources() {
        let mut s = Session::new();
        let plan = s.add_shed(vec![
            "tofu".into(),
            "plan".into(),
            "-out".into(),
            "${file}".into(),
        ]);
        s.shed_mut(plan)
            .unwrap()
            .outputs
            .insert("file".to_string(), OutputSpec::TempPath);
        s.pin(plan, "tfplan".into());
        s.set_state(plan, ShedState::Idle);
        let apply = s.add_shed(vec![
            "tofu".into(),
            "apply".into(),
            "${@tfplan.file}".into(),
        ]);
        s.set_state(apply, ShedState::Idle);
        let mut visited = HashSet::new();
        let chain = build_run_chain(&s, apply, &mut visited);
        assert_eq!(chain, vec![plan, apply], "plan must run before apply");
    }

    #[test]
    fn build_run_chain_skips_interpolation_source_thats_already_done() {
        let mut s = Session::new();
        let plan = s.add_shed(vec!["plan".into()]);
        s.pin(plan, "tfplan".into());
        s.set_state(plan, ShedState::Done(0));
        let apply = s.add_shed(vec!["apply".into(), "${@tfplan.x}".into()]);
        s.set_state(apply, ShedState::Idle);
        let mut visited = HashSet::new();
        let chain = build_run_chain(&s, apply, &mut visited);
        assert_eq!(
            chain,
            vec![apply],
            "Done(0) source is skipped (output already cached)"
        );
    }

    #[test]
    fn temp_path_is_unique_per_call() {
        let p1 = generate_temp_path(ShedId(1), "file");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let p2 = generate_temp_path(ShedId(1), "file");
        assert_ne!(p1, p2, "two calls should yield distinct paths");
        assert!(p1.contains("shed-1-file"));
    }

    #[test]
    fn snapshot_ref_returns_structured_when_source_pipeline_yields_rows() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["seq".into(), "1".into(), "3".into()]);
        s.set_capture(
            src,
            Capture {
                stdout: Bytes::from_static(b"1\n2\n3\n"),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: false,
                structured: None,
            },
        );
        s.shed_mut(src)
            .unwrap()
            .pipeline
            .push(FilterSpec::FromLines);
        s.pin(src, "nums".into());

        let out = snapshot_ref(&s, &ShedRef::Name("nums".into())).expect("snapshot");
        match out {
            SnapshotOutput::Structured(Value::List(rows)) => {
                assert_eq!(rows.len(), 3);
                // Each row is a record with key "line".
                for row in &rows {
                    let Value::Record(rec) = row else {
                        panic!("expected record, got {row:?}");
                    };
                    assert!(rec.contains_key("line"));
                }
            }
            other => panic!("expected Structured(List), got {other:?}"),
        }
    }

    #[test]
    fn snapshot_ref_by_id_passes_bytes_through_when_source_has_no_pipeline() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["echo".into(), "hi".into()]);
        s.set_capture(
            src,
            Capture {
                stdout: Bytes::from_static(b"hello\n"),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: false,
                structured: None,
            },
        );
        // Unpinned: only the %N form can reach it.
        let out = snapshot_ref(&s, &ShedRef::Id(src)).expect("snapshot");
        match out {
            SnapshotOutput::Bytes(b) => assert_eq!(b, b"hello\n"),
            other => panic!("expected Bytes, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_ref_errors_on_missing_id_or_name() {
        let s = Session::new();
        let err = snapshot_ref(&s, &ShedRef::Name("nope".into())).unwrap_err();
        assert!(err.contains("@nope"), "got: {err}");
        let err = snapshot_ref(&s, &ShedRef::Id(ShedId(99))).unwrap_err();
        assert!(err.contains("%99"), "got: {err}");
    }

    /// Build a source shed with `from-json` of a deliberately
    /// non-alphabetical key order, then verify that a downstream
    /// snapshot shed inherits the structured value, NOT a JSON
    /// round-trip — column order must match the source exactly.
    #[test]
    fn snapshot_carries_structured_value_with_column_order_preserved() {
        use indexmap::IndexMap;
        let mut s = Session::new();
        // Two records with columns in z/a/m order — alphabetic sorting
        // (or naive JSON object reordering) would shuffle them.
        let mut row1 = IndexMap::new();
        row1.insert("z".to_string(), Value::Int(1));
        row1.insert("a".to_string(), Value::Int(2));
        row1.insert("m".to_string(), Value::Int(3));
        let src = s.add_shed(vec!["mksrc".into()]);
        s.set_capture(
            src,
            Capture {
                stdout: Bytes::new(),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: false,
                structured: Some(Value::List(vec![Value::Record(row1)])),
            },
        );

        let out = snapshot_ref(&s, &ShedRef::Id(src)).expect("snapshot");
        match out {
            SnapshotOutput::Structured(Value::List(rows)) => {
                let Value::Record(rec) = &rows[0] else {
                    panic!("expected record")
                };
                let cols: Vec<&str> = rec.keys().map(|s| s.as_str()).collect();
                assert_eq!(cols, vec!["z", "a", "m"], "column order preserved");
            }
            other => panic!("expected Structured(List), got {other:?}"),
        }
    }

    #[test]
    fn populate_snapshot_writes_structured_when_source_yields_rows() {
        let mut app = App::new();
        app.history.clear();
        let src = app.session.add_shed(vec!["seq".into()]);
        app.session.set_capture(
            src,
            Capture {
                stdout: Bytes::from_static(b"1\n2\n"),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: false,
                structured: None,
            },
        );
        app.session
            .shed_mut(src)
            .unwrap()
            .pipeline
            .push(FilterSpec::FromLines);

        let dst = app.session.add_shed(vec![format!("%{}", src.0)]);
        populate_snapshot(&mut app, dst, &ShedRef::Id(src));

        let cap = app.session.shed(dst).unwrap().capture.as_ref().unwrap();
        assert!(
            cap.stdout.is_empty(),
            "structured snapshot leaves stdout empty"
        );
        let structured = cap.structured.as_ref().expect("structured populated");
        let Value::List(rows) = structured else {
            panic!("expected list")
        };
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn populate_snapshot_writes_bytes_when_source_has_no_parser() {
        let mut app = App::new();
        app.history.clear();
        let src = app.session.add_shed(vec!["echo".into()]);
        app.session.set_capture(
            src,
            Capture {
                stdout: Bytes::from_static(b"hello\n"),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: false,
                structured: None,
            },
        );
        let dst = app.session.add_shed(vec![format!("%{}", src.0)]);
        populate_snapshot(&mut app, dst, &ShedRef::Id(src));

        let cap = app.session.shed(dst).unwrap().capture.as_ref().unwrap();
        assert_eq!(cap.stdout.as_ref(), b"hello\n");
        assert!(cap.structured.is_none());
    }

    #[test]
    fn compute_schema_at_zero_reads_inherited_structured_columns() {
        let mut s = Session::new();
        let id = s.add_shed(vec!["dst".into()]);
        use indexmap::IndexMap;
        let mut row = IndexMap::new();
        row.insert("foo".to_string(), Value::Int(1));
        row.insert("bar".to_string(), Value::Int(2));
        s.set_capture(
            id,
            Capture {
                stdout: Bytes::new(),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: true,
                structured: Some(Value::List(vec![Value::Record(row)])),
            },
        );
        let shed = s.shed(id).unwrap();
        // No filters: schema should reflect the inherited structured value.
        assert_eq!(compute_schema_at(shed, 0), vec!["foo", "bar"]);
    }

    #[test]
    fn apply_pipeline_starts_from_structured_when_capture_has_it() {
        // Source-of-truth check: a capture with structured + empty stdout
        // + empty pipeline produces PipelineValue::Structured(...), not
        // PipelineValue::Bytes(empty).
        use indexmap::IndexMap;
        let mut row = IndexMap::new();
        row.insert("x".to_string(), Value::Int(42));
        let cap = Capture {
            stdout: Bytes::new(),
            stderr: Bytes::new(),
            exit_code: Some(0),
            started_at: Instant::now(),
            finished_at: Some(Instant::now()),
            truncated: false,
            snapshotted: true,
            structured: Some(Value::List(vec![Value::Record(row)])),
        };
        let (out, _) = apply_pipeline(&cap, &[]).expect("apply");
        match out {
            PipelineValue::Structured(Value::List(rows)) => assert_eq!(rows.len(), 1),
            other => panic!("expected structured list, got {other:?}"),
        }
    }

    #[test]
    fn spawn_ref_snapshot_adds_an_idle_shed_with_percent_id_argv() {
        let mut app = App::new();
        app.history.clear();
        let src = app.session.add_shed(vec!["echo".into(), "hi".into()]);
        spawn_ref_snapshot(&mut app, &ShedRef::Id(src));
        // The new shed should have argv ["%N"] and be Idle.
        let new_id = app.newest_shed_id().unwrap();
        assert_ne!(new_id, src);
        let new_shed = app.session.shed(new_id).unwrap();
        assert_eq!(new_shed.argv, vec![format!("%{}", src.0)]);
        assert!(matches!(new_shed.state, ShedState::Idle));
        // And the new shed should be queued on the run chain.
        assert!(app.pending_run_chain.contains(&new_id));
    }

    #[test]
    fn collect_dependents_finds_both_name_and_id_refs() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["src".into()]);
        s.pin(src, "the_src".into());
        let dep_by_name = s.add_shed(vec!["@the_src".into()]);
        let dep_by_id = s.add_shed(vec![format!("%{}", src.0)]);
        let _unrelated = s.add_shed(vec!["echo".into(), "x".into()]);

        let mut out = Vec::new();
        let mut visited = HashSet::new();
        collect_dependents_recursive(&s, src, &mut out, &mut visited);
        assert!(out.contains(&dep_by_name), "by-name dep missing: {out:?}");
        assert!(out.contains(&dep_by_id), "by-id dep missing: {out:?}");
    }

    #[test]
    fn delete_shed_at_cursor_removes_and_advances() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        let b = app.session.add_shed(vec!["b".into()]);
        let c = app.session.add_shed(vec!["c".into()]);
        app.session.set_cursor(Some(b));
        delete_shed_at_cursor(&mut app);
        assert!(app.session.shed(b).is_none());
        // Cursor advances to next sibling (c).
        assert_eq!(app.session.cursor(), Some(c));
        assert!(app.dirty);
        // Sanity: a still exists.
        assert!(app.session.shed(a).is_some());
    }

    #[test]
    fn delete_last_shed_returns_to_prompt_focus() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        app.session.set_cursor(Some(a));
        app.focus = Focus::ShedCursor;
        delete_shed_at_cursor(&mut app);
        assert!(app.session.cursor().is_none());
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[test]
    fn note_edit_inserts_chars_and_newlines_at_cursor() {
        let mut state = NoteEditState::new(ShedId(1), NotePosition::Pre, None);
        // Synthesize key events through handle_note_edit_key by running it
        // via a minimal app shim. Easier to test the helpers directly.
        for c in "abc".chars() {
            state.buffer.insert(state.cursor, c);
            state.cursor += 1;
        }
        state.buffer.insert(state.cursor, '\n');
        state.cursor += 1;
        state.buffer.insert(state.cursor, 'd');
        state.cursor += 1;
        assert_eq!(state.buffer_string(), "abc\nd");
        assert_eq!(state.cursor, 5);
    }

    #[test]
    fn note_edit_vertical_move_preserves_column() {
        let mut state = NoteEditState::new(ShedId(1), NotePosition::Pre, Some("abcdef\nghij\nkl"));
        // Cursor on second line at column 4 (right after "ghij").
        state.cursor = 11;
        // Up: should go to column 4 of "abcdef" → index 4.
        move_note_cursor_vertically(&mut state, -1);
        assert_eq!(state.cursor, 4);
        // Down twice: back to col 4 of "ghij" then col 2 of "kl" (length 2,
        // clamped to len).
        move_note_cursor_vertically(&mut state, 1);
        assert_eq!(state.cursor, 11);
        move_note_cursor_vertically(&mut state, 1);
        assert_eq!(state.cursor, 14); // len("abcdef\nghij\nkl") = 14
    }

    #[test]
    fn commit_note_edit_writes_to_shed_and_marks_dirty() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["echo".into()]);
        app.session.set_cursor(Some(id));
        open_note_edit(&mut app, NotePosition::Pre);
        let st = app.note_edit.as_mut().expect("opened");
        for c in "hello".chars() {
            st.buffer.insert(st.cursor, c);
            st.cursor += 1;
        }
        // Reset dirty so we can confirm commit flips it.
        app.dirty = false;
        commit_note_edit(&mut app);
        let shed = app.session.shed(id).unwrap();
        assert_eq!(shed.pre_text.as_deref(), Some("hello"));
        assert!(app.dirty);
        assert_eq!(app.focus, Focus::ShedCursor);
        assert!(app.note_edit.is_none());
    }

    #[test]
    fn empty_note_buffer_clears_existing_text() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["echo".into()]);
        if let Some(b) = app.session.shed_mut(id) {
            b.pre_text = Some("old text".into());
        }
        app.session.set_cursor(Some(id));
        open_note_edit(&mut app, NotePosition::Pre);
        // Clear the buffer.
        if let Some(st) = app.note_edit.as_mut() {
            st.buffer.clear();
            st.cursor = 0;
        }
        commit_note_edit(&mut app);
        assert!(app.session.shed(id).unwrap().pre_text.is_none());
    }

    #[test]
    fn savepoint_pushes_clone_and_clears_redo() {
        let mut app = App::new();
        app.history.clear();
        app.aliases_path = None;
        let _ = app.session.add_shed(vec!["a".into()]);
        // Pretend a previous redo path exists.
        app.redo_stack.push(app.session.clone());
        app.savepoint();
        assert_eq!(app.undo_stack.len(), 1);
        assert!(app.redo_stack.is_empty(), "savepoint clears redo");
        assert!(app.dirty);
    }

    #[test]
    fn undo_redo_round_trip_on_filter_add() {
        let mut app = App::new();
        app.history.clear();
        app.aliases_path = None;
        let id = app
            .session
            .add_shed(vec!["seq".into(), "1".into(), "3".into()]);
        app.session.set_capture(
            id,
            Capture {
                stdout: Bytes::from_static(b"1\n2\n3\n"),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: false,
                structured: None,
            },
        );
        app.session.set_cursor(Some(id));

        // Simulate adding a filter via savepoint + mutation.
        app.savepoint();
        app.session
            .shed_mut(id)
            .unwrap()
            .pipeline
            .push(FilterSpec::FromLines);

        assert_eq!(app.session.shed(id).unwrap().pipeline.len(), 1);

        undo(&mut app);
        assert_eq!(app.session.shed(id).unwrap().pipeline.len(), 0);
        // Capture preserved on undo (not lost when reverting structure).
        assert!(app.session.shed(id).unwrap().capture.is_some());

        redo(&mut app);
        assert_eq!(app.session.shed(id).unwrap().pipeline.len(), 1);
        assert!(app.session.shed(id).unwrap().capture.is_some());
    }

    #[test]
    fn undo_resurrects_a_deleted_shed() {
        let mut app = App::new();
        app.history.clear();
        app.aliases_path = None;
        let id = app.session.add_shed(vec!["echo".into(), "hi".into()]);
        app.session.set_cursor(Some(id));

        app.savepoint();
        app.session.remove_shed(id);
        assert!(app.session.shed(id).is_none());

        undo(&mut app);
        let resurrected = app.session.shed(id).expect("shed restored");
        assert_eq!(resurrected.argv, vec!["echo", "hi"]);
    }

    #[test]
    fn undo_with_empty_stack_flashes_and_no_panic() {
        let mut app = App::new();
        app.history.clear();
        app.aliases_path = None;
        undo(&mut app);
        assert!(app.flash.as_deref().unwrap_or("").contains("nothing"));
    }

    #[test]
    fn savepoint_caps_at_max_depth() {
        let mut app = App::new();
        app.history.clear();
        app.aliases_path = None;
        for _ in 0..(MAX_UNDO_DEPTH + 5) {
            app.savepoint();
        }
        assert_eq!(app.undo_stack.len(), MAX_UNDO_DEPTH);
    }

    #[test]
    fn move_filter_cursor_jumps_left_to_command() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["ls".into()]);
        app.session
            .shed_mut(id)
            .unwrap()
            .pipeline
            .push(FilterSpec::FromLines);
        app.session.set_cursor(Some(id));
        app.pipeline_cursor = 0;
        app.command_focused = false;

        // Pull left at index 0 → command_focused becomes true.
        move_filter_cursor(&mut app, -1);
        assert!(app.command_focused);

        // Push right → returns to filter index 0.
        move_filter_cursor(&mut app, 1);
        assert!(!app.command_focused);
        assert_eq!(app.pipeline_cursor, 0);
    }

    #[test]
    fn collect_dependents_finds_recursive_at_refs() {
        let mut s = Session::new();
        let root = s.add_shed(vec!["seq".into(), "1".into(), "3".into()]);
        s.pin(root, "root".into());

        let a = s.add_shed(vec!["@root".into()]);
        s.pin(a, "a".into());

        let b = s.add_shed(vec!["@a".into()]);

        // Unrelated:
        let _other = s.add_shed(vec!["echo".into()]);

        let mut out = Vec::new();
        let mut visited = HashSet::new();
        collect_dependents_recursive(&s, root, &mut out, &mut visited);
        assert_eq!(out, vec![a, b]);
    }

    #[test]
    fn commit_cmd_edit_updates_argv_and_queues_self_plus_dependents() {
        let mut app = App::new();
        app.history.clear();
        let src = app
            .session
            .add_shed(vec!["seq".into(), "1".into(), "3".into()]);
        app.session.set_state(src, ShedState::Done(0));
        app.session.pin(src, "src".into());

        let dep = app.session.add_shed(vec!["@src".into()]);
        app.session.set_state(dep, ShedState::Done(0));

        app.session.set_cursor(Some(src));
        app.open_input(InputKind::CmdEdit, "seq 1 5".into());
        commit_cmd_edit(&mut app);

        assert!(!app.is_input(InputKind::CmdEdit));
        assert_eq!(app.session.shed(src).unwrap().argv, vec!["seq", "1", "5"]);
        // Both source (re-run) and dependent (re-snapshot) queued.
        let queued: Vec<_> = app.pending_run_chain.iter().copied().collect();
        assert_eq!(queued, vec![src, dep]);
    }

    #[test]
    fn commit_cmd_edit_rejects_unmatched_quote() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["echo".into()]);
        app.session.set_cursor(Some(id));
        app.open_input(InputKind::CmdEdit, r#"echo "unclosed"#.into());
        commit_cmd_edit(&mut app);
        // argv unchanged, flash set, mode cleared.
        assert_eq!(app.session.shed(id).unwrap().argv, vec!["echo"]);
        assert!(app.flash.as_deref().unwrap_or("").contains("unmatched"));
    }

    #[test]
    fn validate_alias_name_rejects_bad_inputs() {
        assert!(validate_alias_name("").is_err());
        assert!(validate_alias_name("   ").is_err());
        assert!(validate_alias_name("with space").is_err());
        assert!(validate_alias_name("@bad").is_err());
        assert!(validate_alias_name("/bad").is_err());
        assert!(validate_alias_name("!bad").is_err());
        assert_eq!(validate_alias_name("list").unwrap(), "list");
        assert_eq!(validate_alias_name("  ls-l  ").unwrap(), "ls-l");
        assert_eq!(
            validate_alias_name("name_with_underscore").unwrap(),
            "name_with_underscore"
        );
    }

    #[test]
    fn build_alias_from_cursor_copies_argv_and_pipeline() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["ls".into(), "-lat".into()]);
        app.session
            .shed_mut(id)
            .unwrap()
            .pipeline
            .push(FilterSpec::FromFields);
        app.session.set_cursor(Some(id));

        let alias = build_alias_from_cursor(&app, "list".into()).expect("built");
        assert_eq!(alias.name, "list");
        assert_eq!(alias.argv, vec!["ls", "-lat"]);
        assert_eq!(alias.pipeline.len(), 1);
    }

    #[test]
    fn commit_alias_save_inserts_when_no_collision() {
        let mut app = App::new();
        app.history.clear();
        // Avoid actually writing to the user's real config dir.
        app.aliases_path = None;
        let id = app.session.add_shed(vec!["ls".into()]);
        app.session.set_cursor(Some(id));
        app.open_input(InputKind::AliasName, "list".into());

        commit_alias_save(&mut app);
        assert!(!app.is_input(InputKind::AliasName));
        assert!(app.aliases.lookup("list").is_some());
        assert!(app.alias_overwrite.is_none());
    }

    #[test]
    fn commit_alias_save_defers_to_overwrite_prompt_on_collision() {
        let mut app = App::new();
        app.history.clear();
        app.aliases_path = None;
        let id = app.session.add_shed(vec!["ls".into()]);
        app.session.set_cursor(Some(id));
        app.aliases.upsert(Alias {
            name: "list".into(),
            argv: vec!["echo".into()],
            pipeline: Vec::new(),
        });
        app.open_input(InputKind::AliasName, "list".into());

        commit_alias_save(&mut app);
        assert!(app.alias_overwrite.is_some());
        // Existing entry not yet overwritten.
        assert_eq!(app.aliases.lookup("list").unwrap().argv, vec!["echo"]);

        confirm_alias_overwrite(&mut app, true);
        assert!(app.alias_overwrite.is_none());
        assert_eq!(app.aliases.lookup("list").unwrap().argv, vec!["ls"]);
    }

    #[test]
    fn confirm_alias_overwrite_no_keeps_old_entry() {
        let mut app = App::new();
        app.history.clear();
        app.aliases_path = None;
        app.aliases.upsert(Alias {
            name: "list".into(),
            argv: vec!["echo".into()],
            pipeline: Vec::new(),
        });
        app.alias_overwrite = Some(Alias {
            name: "list".into(),
            argv: vec!["ls".into()],
            pipeline: Vec::new(),
        });
        confirm_alias_overwrite(&mut app, false);
        assert_eq!(app.aliases.lookup("list").unwrap().argv, vec!["echo"]);
    }

    #[test]
    fn spawn_alias_creates_idle_shed_and_opens_cmd_edit() {
        let mut app = App::new();
        app.history.clear();
        app.aliases_path = None;
        let alias = Alias {
            name: "list".into(),
            argv: vec!["ls".into(), "-lat".into()],
            pipeline: vec![FilterSpec::FromFields],
        };
        spawn_alias(&mut app, &alias);
        let id = app.session.cursor().expect("cursor set");
        let shed = app.session.shed(id).unwrap();
        assert_eq!(shed.argv, vec!["ls", "-lat"]);
        assert_eq!(shed.pipeline.len(), 1);
        assert!(matches!(shed.state, ShedState::Idle));
        assert!(app.command_focused);
        assert!(app.is_input(InputKind::CmdEdit));
        // Pre-fill ends with a trailing space for easy arg appending.
        assert!(app.input_text().ends_with(' '));
        assert!(app.input_text().starts_with("ls"));
    }

    #[test]
    fn run_cursor_shed_in_place_queues_when_no_running_entry() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["sleep".into(), "5".into()]);
        app.session.set_cursor(Some(id));
        // No RunningCommand entry → queues normally.
        run_cursor_shed_in_place(&mut app);
        assert_eq!(app.pending_run_chain.front().copied(), Some(id));
    }

    #[test]
    fn cycle_completion_advances_and_wraps() {
        let mut s = Session::new();
        let a = s.add_shed(vec!["a".into()]);
        let b = s.add_shed(vec!["b".into()]);
        s.pin(a, "alpha".into());
        s.pin(b, "alphabet".into());
        let mut app = App::new();
        app.session = s;
        app.history.clear();
        app.prompt = "cat @alp".into();
        app.prompt_cursor = app.prompt.len();

        cycle_completion(&mut app, 1);
        let first = app.prompt.clone();
        assert!(
            first == "cat @alpha" || first == "cat @alphabet",
            "first={first}"
        );
        assert!(app.completion.is_some());

        cycle_completion(&mut app, 1);
        let second = app.prompt.clone();
        assert_ne!(first, second);

        // Wrap back to first
        cycle_completion(&mut app, 1);
        assert_eq!(app.prompt, first);

        // Backwards cycles in reverse
        cycle_completion(&mut app, -1);
        assert_eq!(app.prompt, second);
    }

    #[test]
    fn cycle_completion_with_no_matches_flashes() {
        let mut app = App::new();
        app.history.clear();
        app.prompt = "@thiswillnevermatchanypin".into();
        app.prompt_cursor = app.prompt.len();
        cycle_completion(&mut app, 1);
        assert!(app.completion.is_none());
        assert!(app.flash.as_deref() == Some("no completions"));
    }

    #[test]
    fn non_tab_key_resets_completion_state() {
        // Simulate the `if !is_tab { app.completion = None }` guard.
        let mut app = App::new();
        app.history.clear();
        app.completion = Some(CompletionState {
            base_text: "x ".into(),
            suffix: String::new(),
            matches: vec!["y".into()],
            idx: 0,
        });
        // Any key handler starts with this reset for non-tab keys.
        let is_tab = false;
        if !is_tab {
            app.completion = None;
        }
        assert!(app.completion.is_none());
    }

    // === readline-style editing ===

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // === scratch box navigation ===

    #[test]
    fn down_past_last_shed_parks_on_scratch_in_shed_cursor() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        let _ = app.session.add_shed(vec!["b".into()]);
        app.session.set_cursor(Some(a));
        app.focus = Focus::ShedCursor;
        app.move_cursor(1); // onto b
        app.move_cursor(1); // off the end — scratch box
        assert_eq!(app.session.cursor(), None, "cursor cleared");
        assert_eq!(app.focus, Focus::ShedCursor, "focus stays on cursor");
    }

    #[test]
    fn up_from_scratch_returns_to_last_shed() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.session.add_shed(vec!["a".into()]);
        let b = app.session.add_shed(vec!["b".into()]);
        app.focus = Focus::ShedCursor;
        app.session.set_cursor(None); // simulate parked on scratch
        app.move_cursor(-1);
        assert_eq!(app.session.cursor(), Some(b), "cursor jumps to last shed");
        assert_eq!(app.focus, Focus::ShedCursor);
    }

    #[test]
    fn down_from_scratch_is_noop() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.session.add_shed(vec!["a".into()]);
        app.focus = Focus::ShedCursor;
        app.session.set_cursor(None);
        app.move_cursor(1);
        assert_eq!(app.session.cursor(), None);
        assert_eq!(app.focus, Focus::ShedCursor);
    }

    #[test]
    fn enter_on_scratch_activates_prompt() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.session.add_shed(vec!["a".into()]);
        app.focus = Focus::ShedCursor;
        app.session.set_cursor(None);
        handle_cursor_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[test]
    fn space_on_scratch_activates_prompt_without_running_anything() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.session.add_shed(vec!["a".into()]);
        app.focus = Focus::ShedCursor;
        app.session.set_cursor(None);
        handle_cursor_key(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        assert_eq!(app.focus, Focus::Prompt);
        assert!(app.pending_run_chain.is_empty());
    }

    #[test]
    fn e_on_scratch_activates_prompt_not_edit_shed() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.session.add_shed(vec!["a".into()]);
        app.focus = Focus::ShedCursor;
        app.session.set_cursor(None);
        handle_cursor_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[test]
    fn delete_shed_via_id_removes_shed_without_moving_cursor_if_not_on_it() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        let b = app.session.add_shed(vec!["b".into()]);
        let c = app.session.add_shed(vec!["c".into()]);
        app.session.set_cursor(Some(a));
        delete_shed(&mut app, c);
        assert!(app.session.shed(c).is_none());
        assert_eq!(app.session.cursor(), Some(a), "cursor unaffected");
        assert!(app.session.shed(b).is_some());
    }

    #[test]
    fn delete_shed_via_id_moves_cursor_if_it_was_on_the_target() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        let b = app.session.add_shed(vec!["b".into()]);
        app.session.set_cursor(Some(a));
        delete_shed(&mut app, a);
        assert_eq!(app.session.cursor(), Some(b), "cursor advances to next");
    }

    #[test]
    fn handle_mouse_click_in_delete_region_removes_that_shed() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        let b = app.session.add_shed(vec!["b".into()]);
        // Pretend the renderer placed a delete region for shed b at
        // (10, 5) - 3 cells wide, 1 cell tall.
        app.click_regions.push(ClickRegion {
            rect: Rect {
                x: 10,
                y: 5,
                width: 3,
                height: 1,
            },
            action: ClickAction::DeleteBlock(b),
        });
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, me);
        assert!(app.session.shed(b).is_none(), "shed b deleted");
        assert!(app.session.shed(a).is_some(), "shed a kept");
    }

    #[test]
    fn handle_mouse_click_outside_regions_does_nothing() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        app.click_regions.push(ClickRegion {
            rect: Rect {
                x: 10,
                y: 5,
                width: 3,
                height: 1,
            },
            action: ClickAction::DeleteBlock(a),
        });
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, me);
        assert!(app.session.shed(a).is_some(), "shed a still present");
    }

    #[test]
    fn handle_mouse_ignores_non_click_events() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        app.click_regions.push(ClickRegion {
            rect: Rect {
                x: 10,
                y: 5,
                width: 3,
                height: 1,
            },
            action: ClickAction::DeleteBlock(a),
        });
        // A scroll event right inside the region — must not delete.
        let me = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 11,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, me);
        assert!(app.session.shed(a).is_some());
    }
    #[tokio::test]
    async fn drain_streams_mirrors_chunks_into_shed_capture() {
        // Spawn a real `printf` so the reader task actually streams
        // bytes through the channel. Awaiting the handle guarantees
        // the sender closed, so try_recv after will yield every chunk
        // synchronously when drain_streams runs.
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["printf".into(), "hello".into()]);
        let (handle, killer, chunks) =
            crate::exec::spawn_command(vec!["printf".into(), "hello".into()], 1024)
                .await
                .unwrap();
        app.running.insert(
            id,
            RunningCommand {
                handle,
                killer,
                chunks,
                stream_buf: BytesMut::new(),
            },
        );
        // Wait for the child to finish so all chunks are sent.
        while !app.running.get(&id).unwrap().handle.is_finished() {
            tokio::task::yield_now().await;
        }

        drain_streams(&mut app);
        let cap = app.session.shed(id).unwrap().capture.as_ref().unwrap();
        assert_eq!(cap.stdout.as_ref(), b"hello");
        // Streaming sets a partial capture: no exit code, no finish ts.
        assert!(cap.exit_code.is_none());
        assert!(cap.finished_at.is_none());
    }

    #[test]
    fn tab_completion_respects_cursor_mid_string() {
        // Cursor mid-token: completion should replace just the token
        // under cursor, preserving the suffix.
        let mut app = App::new();
        app.history.clear();
        app.prompt = "ex foo".into();
        // Cursor right after "ex".
        app.prompt_cursor = 2;
        cycle_completion(&mut app, 1);
        // After tab, prompt is "<match> foo"; cursor sits at end of match.
        assert!(app.prompt.ends_with(" foo"), "got: {:?}", app.prompt);
        assert!(app.completion.is_some());
        let state = app.completion.as_ref().unwrap();
        assert_eq!(state.suffix, " foo");
        assert_eq!(app.prompt_cursor, app.prompt.len() - " foo".len());
    }

    // === right-click context menu ===

    fn make_body_region(shed_id: ShedId, lines: Vec<&str>) -> BodyRegion {
        BodyRegion {
            rect: Rect {
                x: 2,
                y: 3,
                width: 40,
                height: lines.len() as u16,
            },
            shed_id,
            lines: lines.into_iter().map(|s| s.to_string()).collect(),
            cells: Vec::new(),
        }
    }

    #[test]
    fn right_click_in_shed_body_opens_menu_with_line_item() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        // Give the shed a capture so "copy whole output" has content.
        let id_marker = a;
        if let Some(b) = app.session.shed_mut(id_marker) {
            b.capture = Some(Capture {
                stdout: Bytes::from_static(b"hello\nworld\n"),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: false,
                structured: None,
            });
        }
        app.body_regions
            .push(make_body_region(a, vec!["hello", "world"]));
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 5, // inside rect.x=2..42
            row: 3,    // first line
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, me);
        let menu = app.context_menu.as_ref().expect("menu opened");
        let labels: Vec<&str> = menu.items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"Copy line"));
        assert!(labels.contains(&"Copy whole output"));
        // Targeted line text should be the first body row.
        match &menu
            .items
            .iter()
            .find(|i| i.label == "Copy line")
            .unwrap()
            .action
        {
            ContextMenuAction::CopyText(t) => assert_eq!(t, "hello"),
            _ => panic!("expected CopyText"),
        }
    }

    #[test]
    fn right_click_on_blank_line_still_offers_whole_output() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        if let Some(b) = app.session.shed_mut(a) {
            b.capture = Some(Capture {
                stdout: Bytes::from_static(b"only output"),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: false,
                structured: None,
            });
        }
        // Empty line at row 3.
        app.body_regions
            .push(make_body_region(a, vec!["", "world"]));
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, me);
        let menu = app.context_menu.as_ref().expect("menu opened");
        let labels: Vec<&str> = menu.items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            !labels.contains(&"Copy line"),
            "blank line shouldn't offer copy"
        );
        assert!(labels.contains(&"Copy whole output"));
    }

    #[test]
    fn right_click_outside_body_does_nothing() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        app.body_regions.push(make_body_region(a, vec!["x"]));
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 99, // outside rect
            row: 99,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, me);
        assert!(app.context_menu.is_none());
    }

    #[test]
    fn menu_key_arrows_cycle_through_items() {
        let mut app = App::new();
        app.context_menu = Some(ContextMenu {
            pos: (0, 0),
            items: vec![
                ContextMenuItem {
                    label: "a".into(),
                    action: ContextMenuAction::CopyText("a".into()),
                },
                ContextMenuItem {
                    label: "b".into(),
                    action: ContextMenuAction::CopyText("b".into()),
                },
                ContextMenuItem {
                    label: "c".into(),
                    action: ContextMenuAction::CopyText("c".into()),
                },
            ],
            selected: 0,
        });
        handle_context_menu_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.context_menu.as_ref().unwrap().selected, 1);
        handle_context_menu_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.context_menu.as_ref().unwrap().selected, 2);
        // Down past end wraps to 0.
        handle_context_menu_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.context_menu.as_ref().unwrap().selected, 0);
        // Up from 0 wraps to last.
        handle_context_menu_key(&mut app, key(KeyCode::Up));
        assert_eq!(app.context_menu.as_ref().unwrap().selected, 2);
    }

    #[test]
    fn menu_enter_on_insert_action_closes_menu_and_splices_prompt() {
        // Using InsertAtPrompt avoids touching stdout (OSC 52) during the
        // test, which would otherwise pollute test output.
        let mut app = App::new();
        app.history.clear();
        app.focus = Focus::Prompt;
        app.prompt = "echo ".into();
        app.prompt_cursor = 5;
        app.context_menu = Some(ContextMenu {
            pos: (0, 0),
            items: vec![ContextMenuItem {
                label: "Add line to prompt".into(),
                action: ContextMenuAction::InsertAtPrompt("hello".into()),
            }],
            selected: 0,
        });
        handle_context_menu_key(&mut app, key(KeyCode::Enter));
        assert!(app.context_menu.is_none());
        assert_eq!(app.prompt, "echo hello");
        assert_eq!(app.prompt_cursor, 10);
    }

    #[test]
    fn menu_key_esc_dismisses_without_acting() {
        let mut app = App::new();
        app.context_menu = Some(ContextMenu {
            pos: (0, 0),
            items: vec![ContextMenuItem {
                label: "x".into(),
                action: ContextMenuAction::CopyText("x".into()),
            }],
            selected: 0,
        });
        handle_context_menu_key(&mut app, key(KeyCode::Esc));
        assert!(app.context_menu.is_none());
        assert!(app.flash.is_none(), "esc should not trigger copy");
    }

    #[test]
    fn insert_at_prompt_splices_text_at_cursor() {
        let mut app = App::new();
        app.history.clear();
        app.focus = Focus::Prompt;
        app.prompt = "echo  bar".into();
        app.prompt_cursor = 5; // between "echo " and " bar"
        insert_at_prompt(&mut app, "foo");
        assert_eq!(app.prompt, "echo foo bar");
        assert_eq!(app.prompt_cursor, 8);
    }

    #[test]
    fn add_to_prompt_only_offered_when_focus_is_prompt() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        if let Some(b) = app.session.shed_mut(a) {
            b.capture = Some(Capture {
                stdout: Bytes::from_static(b"hello\n"),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: false,
                structured: None,
            });
        }
        app.body_regions.push(make_body_region(a, vec!["hello"]));
        // Focus is ShedCursor (not Prompt) — no Add line to prompt.
        app.focus = Focus::ShedCursor;
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, me);
        let labels: Vec<String> = app
            .context_menu
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|i| i.label.clone())
            .collect();
        assert!(!labels.contains(&"Add line to prompt".to_string()));

        // Now with Focus::Prompt the item appears.
        app.context_menu = None;
        app.focus = Focus::Prompt;
        handle_mouse(&mut app, me);
        let labels: Vec<String> = app
            .context_menu
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|i| i.label.clone())
            .collect();
        assert!(labels.contains(&"Add line to prompt".to_string()));
    }

    #[test]
    fn click_outside_menu_dismisses_it() {
        let mut app = App::new();
        app.context_menu = Some(ContextMenu {
            pos: (10, 10),
            items: vec![ContextMenuItem {
                label: "x".into(),
                action: ContextMenuAction::CopyText("x".into()),
            }],
            selected: 0,
        });
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, me);
        assert!(app.context_menu.is_none());
    }

    // === cell-aware right-click (#2) ===

    #[test]
    fn path_basename_recognises_typical_paths() {
        assert_eq!(path_basename("/etc/passwd"), Some("passwd".into()));
        assert_eq!(path_basename("./foo.txt"), Some("foo.txt".into()));
        assert_eq!(path_basename("../bar/baz.rs"), Some("baz.rs".into()));
        assert_eq!(path_basename("~/devel/shed"), Some("shed".into()));
        assert_eq!(path_basename("dir/file"), Some("file".into()));
        // Trailing slashes are stripped before taking basename.
        assert_eq!(path_basename("/var/log/"), Some("log".into()));
    }

    #[test]
    fn path_basename_rejects_non_paths_and_text_with_spaces() {
        assert!(path_basename("").is_none());
        assert!(path_basename("nope").is_none(), "no slash");
        assert!(path_basename("42").is_none());
        assert!(
            path_basename("hello world").is_none(),
            "contains whitespace"
        );
        assert!(
            path_basename("/path with space/file").is_none(),
            "spaces disqualify"
        );
        assert!(path_basename("/").is_none(), "trim leaves empty");
    }

    #[test]
    fn render_table_records_cell_layouts_per_row_per_column() {
        use crate::tui::render::render_table;
        use shed_core::Value;
        let cols = vec!["a".to_string(), "b".to_string()];
        let mut row1 = indexmap::IndexMap::new();
        row1.insert("a".into(), Value::String("x".into()));
        row1.insert("b".into(), Value::String("yy".into()));
        let mut row2 = indexmap::IndexMap::new();
        row2.insert("a".into(), Value::String("zz".into()));
        row2.insert("b".into(), Value::String("w".into()));
        let items = vec![Value::Record(row1), Value::Record(row2)];
        let mut cells = Vec::new();
        let lines = render_table(&items, &cols, 10, false, &mut cells);
        // 2 cols x 2 rows = 4 cells.
        assert_eq!(cells.len(), 4);
        // Header + separator = lines 0..1; data rows start at line 2.
        assert_eq!(cells[0].line_idx, 2);
        assert_eq!(cells[1].line_idx, 2);
        assert_eq!(cells[2].line_idx, 3);
        assert_eq!(cells[3].line_idx, 3);
        // First column starts at the body's 6-char indent.
        assert_eq!(cells[0].x_offset, 6);
        // Second column starts after first col width (2) + separator (3) = 11.
        assert_eq!(cells[1].x_offset, 11);
        // Cell value preserved.
        assert_eq!(cells[0].value, Value::String("x".into()));
        // Sanity: lines actually rendered.
        assert!(!lines.is_empty());
    }

    fn body_with_one_cell(shed_id: ShedId, value: Value) -> BodyRegion {
        // A 1x1 cell located at (col 10, row 5) so tests can right-click
        // precisely. Width 8 lets us hit columns 10..18.
        BodyRegion {
            rect: Rect {
                x: 6,
                y: 3,
                width: 40,
                height: 6,
            },
            shed_id,
            lines: vec![String::new(); 6],
            cells: vec![CellRegion {
                rect: Rect {
                    x: 10,
                    y: 5,
                    width: 8,
                    height: 1,
                },
                value,
            }],
        }
    }

    #[test]
    fn right_click_on_cell_offers_copy_cell_and_copy_filename_for_paths() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["ls".into()]);
        app.body_regions
            .push(body_with_one_cell(a, Value::String("/etc/passwd".into())));
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 12,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, me);
        let menu = app.context_menu.as_ref().expect("menu opened");
        let labels: Vec<&str> = menu.items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"Copy cell"));
        assert!(labels.contains(&"Copy filename"));
        // Verify the filename payload is just the basename.
        let filename = menu
            .items
            .iter()
            .find(|i| i.label == "Copy filename")
            .unwrap();
        match &filename.action {
            ContextMenuAction::CopyText(s) => assert_eq!(s, "passwd"),
            _ => panic!("expected CopyText"),
        }
    }

    #[test]
    fn right_click_on_non_path_cell_omits_copy_filename() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["echo".into()]);
        app.body_regions.push(body_with_one_cell(a, Value::Int(42)));
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 12,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, me);
        let labels: Vec<String> = app
            .context_menu
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|i| i.label.clone())
            .collect();
        assert!(labels.contains(&"Copy cell".to_string()));
        assert!(!labels.contains(&"Copy filename".to_string()));
    }

    #[test]
    fn shift_left_click_on_cell_adds_value_to_prompt_and_focuses_it() {
        let mut app = App::new();
        app.history.clear();
        app.focus = Focus::ShedCursor; // start somewhere other than Prompt
        app.prompt = "echo ".into();
        app.prompt_cursor = 5;
        let a = app.session.add_shed(vec!["ls".into()]);
        app.body_regions
            .push(body_with_one_cell(a, Value::String("/etc/passwd".into())));
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 5,
            modifiers: KeyModifiers::SHIFT,
        };
        handle_mouse(&mut app, me);
        assert_eq!(app.focus, Focus::Prompt, "focus jumps to Prompt");
        assert_eq!(app.prompt, "echo /etc/passwd");
        assert_eq!(app.prompt_cursor, "echo /etc/passwd".len());
        assert!(app.context_menu.is_none(), "menu must not open");
    }

    #[test]
    fn ctrl_left_click_works_the_same_as_shift_for_terminals_that_eat_shift() {
        let mut app = App::new();
        app.history.clear();
        app.focus = Focus::ShedCursor;
        app.prompt = "cat ".into();
        app.prompt_cursor = 4;
        let a = app.session.add_shed(vec!["ls".into()]);
        app.body_regions
            .push(body_with_one_cell(a, Value::String("/var/log".into())));
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 5,
            modifiers: KeyModifiers::CONTROL,
        };
        handle_mouse(&mut app, me);
        assert_eq!(app.focus, Focus::Prompt);
        assert_eq!(app.prompt, "cat /var/log");
    }

    #[test]
    fn shift_left_click_outside_cell_inserts_the_line_text() {
        let mut app = App::new();
        app.history.clear();
        app.focus = Focus::Prompt;
        let a = app.session.add_shed(vec!["a".into()]);
        let mut br = body_with_one_cell(a, Value::String("/etc/passwd".into()));
        br.lines[0] = "hello world".into();
        app.body_regions.push(br);
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 3, // first body line, no cell here
            modifiers: KeyModifiers::SHIFT,
        };
        handle_mouse(&mut app, me);
        assert_eq!(app.prompt, "hello world");
    }

    #[test]
    fn shift_left_click_outside_any_body_is_a_noop() {
        let mut app = App::new();
        app.history.clear();
        app.focus = Focus::ShedCursor;
        app.prompt = "untouched".into();
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 99,
            row: 99,
            modifiers: KeyModifiers::SHIFT,
        };
        handle_mouse(&mut app, me);
        assert_eq!(app.prompt, "untouched");
        // No body region matched → focus shouldn't have changed either.
        assert_eq!(app.focus, Focus::ShedCursor);
    }

    #[test]
    fn shift_left_click_bypasses_delete_button() {
        // Without Shift, clicking the [×] region would delete the shed.
        // With Shift, the click should not delete — it should hit the
        // body cell/line behind it instead (here: nothing to add, so no-op).
        let mut app = App::new();
        app.history.clear();
        app.focus = Focus::Prompt;
        let a = app.session.add_shed(vec!["a".into()]);
        // Register a delete click region overlapping (10, 0); also a body
        // region whose lines are all empty so no-op is expected.
        app.click_regions.push(ClickRegion {
            rect: Rect {
                x: 10,
                y: 0,
                width: 3,
                height: 1,
            },
            action: ClickAction::DeleteBlock(a),
        });
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 0,
            modifiers: KeyModifiers::SHIFT,
        };
        handle_mouse(&mut app, me);
        assert!(app.session.shed(a).is_some(), "shed must not be deleted");
    }

    #[test]
    fn right_click_on_line_outside_any_cell_uses_line_actions() {
        let mut app = App::new();
        app.history.clear();
        let a = app.session.add_shed(vec!["a".into()]);
        // Body has a cell at row 5; clicking on row 3 hits the line, not
        // any cell.
        let mut br = body_with_one_cell(a, Value::String("/etc/passwd".into()));
        br.lines[0] = "some text".into();
        app.body_regions.push(br);
        let me = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 12,
            row: 3, // first line of body, no cell here
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, me);
        let labels: Vec<String> = app
            .context_menu
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|i| i.label.clone())
            .collect();
        assert!(labels.contains(&"Copy line".to_string()));
        assert!(!labels.contains(&"Copy cell".to_string()));
    }

    // === pinned-only exit-prompt gating ===

    #[test]
    fn fresh_app_has_no_unsaved_pinned_changes() {
        let app = App::new();
        assert!(!has_unsaved_pinned_changes(&app));
    }

    #[test]
    fn adding_unpinned_sheds_does_not_trigger_save_prompt() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.session.add_shed(vec!["ls".into()]);
        let _ = app.session.add_shed(vec!["pwd".into()]);
        // Sheds exist but none are pinned — exit should be silent.
        assert!(!has_unsaved_pinned_changes(&app));
    }

    #[test]
    fn pinning_a_shed_triggers_save_prompt() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["ls".into()]);
        assert!(!has_unsaved_pinned_changes(&app));
        app.session.pin(id, "listing".into());
        assert!(has_unsaved_pinned_changes(&app));
    }

    #[test]
    fn unpinning_a_shed_triggers_save_prompt() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["ls".into()]);
        app.session.pin(id, "listing".into());
        // Pretend we just saved this state.
        app.saved_pinned_json = pinned_entries_json(&app.session);
        assert!(!has_unsaved_pinned_changes(&app));
        // Unpin (set name back to None).
        if let Some(b) = app.session.shed_mut(id) {
            b.name = None;
        }
        assert!(has_unsaved_pinned_changes(&app));
    }

    #[test]
    fn editing_argv_of_pinned_shed_triggers_save_prompt() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["ls".into()]);
        app.session.pin(id, "listing".into());
        app.saved_pinned_json = pinned_entries_json(&app.session);
        // Edit argv.
        if let Some(b) = app.session.shed_mut(id) {
            b.argv = vec!["ls".into(), "-la".into()];
        }
        assert!(has_unsaved_pinned_changes(&app));
    }

    #[test]
    fn editing_argv_of_unpinned_shed_does_not_trigger_save_prompt() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["ls".into()]);
        // Unpinned — edit doesn't matter.
        if let Some(b) = app.session.shed_mut(id) {
            b.argv = vec!["ls".into(), "-la".into()];
        }
        assert!(!has_unsaved_pinned_changes(&app));
    }

    #[test]
    fn request_quit_skips_prompt_when_only_unpinned_changes() {
        let mut app = App::new();
        app.history.clear();
        // Add unpinned sheds (and even set the old dirty flag to verify
        // it's no longer consulted).
        let _ = app.session.add_shed(vec!["foo".into()]);
        app.dirty = true;
        request_quit(&mut app);
        assert!(app.exit_prompt.is_none(), "exit prompt should not fire");
        assert!(app.quit, "should quit immediately");
    }

    #[test]
    fn request_quit_shows_prompt_when_a_pinned_shed_changed() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["ls".into()]);
        app.session.pin(id, "listing".into());
        request_quit(&mut app);
        assert_eq!(app.exit_prompt, Some(ExitPrompt::Confirm));
        assert!(!app.quit);
    }

    #[test]
    fn request_quit_with_multiple_tabs_closes_active_tab_not_app() {
        let mut app = App::new();
        app.history.clear();
        // Pin a shed in tab 0 so the prompt would fire if we hit the
        // single-tab path — this test verifies that path is skipped.
        let id = app.session.add_shed(vec!["ls".into()]);
        app.session.pin(id, "logs".into());
        let _ = app.new_tab(); // now on tab 1
        assert_eq!(app.tabs.len(), 2);
        request_quit(&mut app);
        // App must NOT quit and must NOT prompt; the active tab closes.
        assert!(!app.quit);
        assert!(app.exit_prompt.is_none());
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab, 0);
    }

    #[test]
    fn request_quit_on_last_tab_falls_through_to_quit_prompt_logic() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["ls".into()]);
        app.session.pin(id, "logs".into());
        // Only one tab — should prompt for save.
        assert_eq!(app.tabs.len(), 1);
        request_quit(&mut app);
        assert_eq!(app.exit_prompt, Some(ExitPrompt::Confirm));
        assert!(!app.quit);
    }

    #[test]
    fn run_exit_builtin_closes_active_tab_when_multi_tab() {
        let mut app = App::new();
        app.history.clear();
        let _ = app.new_tab();
        let _ = app.new_tab();
        assert_eq!(app.tabs.len(), 3);
        run_exit_builtin(&mut app);
        // Closed one tab, didn't quit.
        assert!(!app.quit);
        assert_eq!(app.tabs.len(), 2);
    }
}
