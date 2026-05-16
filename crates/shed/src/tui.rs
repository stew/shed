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
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as TuiBlock, Borders, Paragraph, Wrap},
};
use shed_core::{
    Alias, AliasFile, Shed, ShedId, ShedState, Capture, CompareOp, Filter, FilterSpec,
    Notebook, PipelineValue, Predicate, Session, SortDirection, SortKey, Value, apply_with_notes,
};
use tokio::task::JoinHandle;

use crate::ansi;
use crate::exec::{self, CaptureOutcome, ExecError, Killer};

type CommandTask = JoinHandle<Result<CaptureOutcome, ExecError>>;

/// A clickable region of the screen registered by a draw pass.
/// Rebuilt every frame; hit-tested in [`handle_mouse_click`].
#[derive(Debug, Clone)]
struct ClickRegion {
    rect: Rect,
    action: ClickAction,
}

#[derive(Debug, Clone, Copy)]
enum ClickAction {
    /// Click on the `×` button on a shed — delete the shed.
    DeleteBlock(ShedId),
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
    "top", "htop", "btop", "atop", "glances", "iotop", "iftop", "ncdu",
    "vi", "vim", "nvim", "emacs", "emacsclient", "nano", "pico",
    "helix", "hx", "micro", "kak",
    "less", "more", "most", "view",
    "man", "info", "pinfo",
    "tmux", "screen", "byobu", "zellij",
    "ssh", "mosh", "telnet", "rlogin",
    "tig", "lazygit", "gitui",
    "ranger", "nnn", "lf",
    "fzf", "sk",
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
    cursor: usize,
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
    shed_id: ShedId,
    position: NotePosition,
    buffer: Vec<char>,
    cursor: usize,
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
    input: String,
    cursor: usize,
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
    name: &'static str,
    description: &'static str,
    enabled: fn(&App) -> bool,
    handler: fn(&mut App),
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
            app.write_input_mode = true;
            app.write_input.clear();
            app.write_cursor = 0;
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
                app.pin_cursor = existing.len();
                app.pin_input = existing;
                app.pin_input_mode = true;
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
            if let Some(id) = app.session.cursor() {
                if let Some(shed) = app.session.shed(id) {
                    let joined = shlex::try_join(shed.argv.iter().map(String::as_str))
                        .unwrap_or_else(|_| shed.argv.join(" "));
                    app.rerun_cursor = joined.len();
                    app.rerun_input = joined;
                    app.rerun_input_mode = true;
                    app.rerun_source_id = Some(id);
                }
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
    cursor: usize,
    filter: String,
    input_mode: EnvInputMode,
    input_buffer: String,
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
    fn entries(&self) -> Vec<(String, String)> {
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

    fn name(self) -> &'static str {
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

    fn description(self) -> &'static str {
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
            FilterKind::Join => "concatenate every row's column value with a delimiter into one row",
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

    fn name(self) -> &'static str {
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

    fn description(self) -> &'static str {
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

    fn value_label(self) -> &'static str {
        match self {
            WhereOp::Matches => "pattern",
            WhereOp::Contains => "substring",
            _ => "value",
        }
    }

    fn to_compare_op(self) -> Option<CompareOp> {
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
    column: usize,
    op: WhereOp,
    pattern: String,
}

impl WhereClause {
    fn default_for(available_columns: &[String]) -> Self {
        Self {
            column: if available_columns.is_empty() { 0 } else { 0 },
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
    fn name(self) -> &'static str {
        match self {
            WhereCombine::And => "AND",
            WhereCombine::Or => "OR",
        }
    }

    fn description(self) -> &'static str {
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
            collect_chain(a, clauses, columns, is_and)
                && collect_chain(b, clauses, columns, is_and)
        }
        Predicate::Or(a, b) if !is_and => {
            collect_chain(a, clauses, columns, is_and)
                && collect_chain(b, clauses, columns, is_and)
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
    shed_id: ShedId,
    kind: FilterKind,
    where_clauses: Vec<WhereClause>,
    where_active_clause: usize,
    where_combine: WhereCombine,
    n_input: String,
    column_selections: Vec<bool>,
    column_cursor: usize,
    csv_delim: char,
    csv_has_header: bool,
    regex_pattern: String,
    sort_keys: Vec<(usize, SortDirection)>,
    sort_keys_cursor: usize,
    rename_to_inputs: Vec<String>,
    rename_cursor: usize,
    target_column: usize,
    delim_text: String,
    available_columns: Vec<String>,
    field: FormField,
    mode: EditMode,
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
                    if let Some(i) = state.available_columns.iter().position(|c| c == from) {
                        if let Some(slot) = state.rename_to_inputs.get_mut(i) {
                            *slot = to.clone();
                        }
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

    fn fields(&self) -> &'static [FormField] {
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

    fn form_lines(&self) -> u16 {
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
        let i = FilterKind::ALL.iter().position(|k| *k == self.kind).unwrap_or(0) as i32;
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
            let i = WhereOp::ALL.iter().position(|o| *o == clause.op).unwrap_or(0) as i32;
            let new_i = (i + delta).rem_euclid(WhereOp::ALL.len() as i32) as usize;
            clause.op = WhereOp::ALL[new_i];
        }
    }

    fn selected_column(&self) -> Option<&str> {
        let idx = self.active_clause().map(|c| c.column)?;
        self.available_columns.get(idx).map(|s| s.as_str())
    }

    fn active_op(&self) -> WhereOp {
        self.active_clause().map(|c| c.op).unwrap_or(WhereOp::Matches)
    }

    fn active_pattern(&self) -> &str {
        self.active_clause().map(|c| c.pattern.as_str()).unwrap_or("")
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

    fn build_filter(&self) -> Option<FilterSpec> {
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
    let mut value = PipelineValue::Bytes(capture.stdout.clone());
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
    session: Session,
    prompt: String,
    /// Byte offset of the insertion caret in [`App::prompt`]. Always a
    /// char boundary in `[0, prompt.len()]`.
    prompt_cursor: usize,
    history: Vec<String>,
    history_cursor: Option<usize>,
    /// Active tab-completion cycle, or `None` if Tab hasn't been pressed
    /// since the last edit. Cleared on any non-Tab key in a completion
    /// context. See [`cycle_completion`].
    completion: Option<CompletionState>,
    write_input_mode: bool,
    write_input: String,
    write_cursor: usize,
    pin_input_mode: bool,
    pin_input: String,
    pin_cursor: usize,
    rerun_input_mode: bool,
    rerun_input: String,
    rerun_cursor: usize,
    rerun_source_id: Option<ShedId>,
    pending_rerun: Option<RerunRequest>,
    /// True when the ShedCursor's "filter cursor" has been pulled left
    /// past the first filter onto the command itself. Visually highlights
    /// the argv span; Enter opens the in-place command editor.
    command_focused: bool,
    cmd_edit_input_mode: bool,
    cmd_edit_input: String,
    cmd_edit_cursor: usize,
    last_cwd: Option<PathBuf>,
    env_edit: Option<EnvEditState>,
    note_edit: Option<NoteEditState>,
    palette_state: Option<PaletteState>,
    palette_prev_focus: Option<Focus>,
    focus: Focus,
    filter_edit: Option<FilterEditState>,
    pipeline_cursor: usize,
    expand_scroll: usize,
    search_query: String,
    search_input: String,
    search_cursor: usize,
    search_input_mode: bool,
    search_anchor_scroll: usize,
    search_input_backward: bool,
    search_case_insensitive: bool,
    flash: Option<String>,
    quit: bool,
    running: HashMap<ShedId, RunningCommand>,
    pending_handover: Option<HandoverRequest>,
    /// Path the notebook is bound to (set by `--open`, by Ctrl-O, or by
    /// the first Ctrl-S that prompted for a path). Subsequent Ctrl-S
    /// saves silently to this path.
    notebook_path: Option<PathBuf>,
    /// Cross-session aliases: typing the alias name at the prompt
    /// materialises a shed with the saved argv + pipeline. Loaded once
    /// from `aliases_path` on startup, rewritten on every change.
    aliases: AliasFile,
    aliases_path: Option<PathBuf>,
    alias_name_input_mode: bool,
    alias_name_input: String,
    alias_name_cursor: usize,
    /// Pending overwrite confirmation when `A` collides with an existing
    /// alias name. Holds the would-be entry; user resolves with y/n/c.
    alias_overwrite: Option<Alias>,
    /// Manage view state (Focus::AliasManage). `None` outside the view.
    alias_manage: Option<AliasManageState>,
    /// `true` when the session has unsaved changes. Set whenever a shed
    /// is added, edited, pinned/unpinned, re-run, or its pipeline mutated.
    /// Cleared on save/load.
    dirty: bool,
    /// Queue of sheds to run in sequence (head first). Built by walking
    /// `@-ref` deps so a snapshot shed runs its source before itself.
    /// The event loop kicks off one at a time and gates on terminal state.
    pending_run_chain: VecDeque<ShedId>,
    /// Shed currently being processed by the run-in-place machinery.
    /// While `Some`, the next chain item won't start. Cleared once the
    /// shed reaches a terminal state.
    chain_in_flight: Option<ShedId>,
    /// Snapshots taken before each structural mutation. Bounded; oldest
    /// drops first when full. Captures are shared via `bytes::Bytes`
    /// refcounting so the memory cost is roughly one BTreeMap clone per
    /// entry.
    undo_stack: Vec<Session>,
    /// Snapshots that were undone past. Cleared on every fresh
    /// structural mutation so redo only chains forward through actual
    /// undos.
    redo_stack: Vec<Session>,
    /// Save-as input bar (Ctrl-S without a bound path).
    save_input_mode: bool,
    save_input: String,
    save_cursor: usize,
    /// Open input bar (Ctrl-O).
    open_input_mode: bool,
    open_input: String,
    open_cursor: usize,
    /// "Save before quitting?" exit prompt. Showing while non-None;
    /// keys map to y / n / c (cancel).
    exit_prompt: Option<ExitPrompt>,
    /// Clickable screen regions registered by the last draw pass.
    /// Rebuilt every frame; hit-tested when a mouse click arrives.
    click_regions: Vec<ClickRegion>,
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
            write_input_mode: false,
            write_input: String::new(),
            write_cursor: 0,
            pin_input_mode: false,
            pin_input: String::new(),
            pin_cursor: 0,
            rerun_input_mode: false,
            rerun_input: String::new(),
            rerun_cursor: 0,
            rerun_source_id: None,
            pending_rerun: None,
            command_focused: false,
            cmd_edit_input_mode: false,
            cmd_edit_input: String::new(),
            cmd_edit_cursor: 0,
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
            search_input: String::new(),
            search_cursor: 0,
            search_input_mode: false,
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
            alias_name_input_mode: false,
            alias_name_input: String::new(),
            alias_name_cursor: 0,
            alias_overwrite: None,
            alias_manage: None,
            dirty: false,
            pending_run_chain: VecDeque::new(),
            chain_in_flight: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            save_input_mode: false,
            save_input: String::new(),
            save_cursor: 0,
            open_input_mode: false,
            open_input: String::new(),
            open_cursor: 0,
            exit_prompt: None,
            click_regions: Vec::new(),
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
        if app.chain_in_flight.is_none() {
            if let Some(next) = app.pending_run_chain.pop_front() {
                app.chain_in_flight = Some(next);
                perform_run_in_place(&mut app, next).await;
            }
        }
        drain_streams(&mut app);
        reap_completed(&mut app).await;
        advance_run_chain(&mut app);
        let mut regions: Vec<ClickRegion> = Vec::new();
        terminal.draw(|f| draw(f, &app, &mut regions))?;
        app.click_regions = regions;
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
    if !req.pipeline.is_empty() {
        if let Some(shed) = app.session.shed_mut(id) {
            shed.pipeline = req.pipeline;
        }
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

/// True if `argv` is a single token starting with `@` — shed's syntax for
/// "snapshot the output of pinned shed @name". The bare prefix `@`
/// (length 1) is rejected as nameless.
fn is_pinned_ref(argv: &[String]) -> bool {
    argv.len() == 1 && argv[0].len() > 1 && argv[0].starts_with('@')
}

/// `Done`/`Failed`/`Snapshotted` are terminal — the run-chain machinery
/// uses this to decide when to advance. `Idle` and `Running` are not.
fn is_terminal_state(state: &ShedState) -> bool {
    matches!(
        state,
        ShedState::Done(_) | ShedState::Failed(_) | ShedState::Snapshotted
    )
}

/// Walk `@-ref` deps to compute the run order for `target`. Sources that
/// are `Idle` or `Running` get added before the target so the chain
/// either runs them first or simply waits on them. `Done`/`Failed`
/// sources are not re-run — the snapshot will use the existing capture
/// or fail with a clear error. Cycles are guarded via `visited`.
fn build_run_chain(session: &Session, target: ShedId, visited: &mut HashSet<ShedId>) -> Vec<ShedId> {
    if !visited.insert(target) {
        return Vec::new();
    }
    let mut chain = Vec::new();
    let shed = match session.shed(target) {
        Some(b) => b,
        None => return chain,
    };
    if is_pinned_ref(&shed.argv) {
        let name = &shed.argv[0][1..];
        if let Some(src_id) = session.lookup_by_name(name) {
            if let Some(src) = session.shed(src_id) {
                if matches!(src.state, ShedState::Idle | ShedState::Running) {
                    let mut sub = build_run_chain(session, src_id, visited);
                    chain.append(&mut sub);
                }
            }
        }
    }
    chain.push(target);
    chain
}

/// Walk *downward* from `source` to find every shed whose argv is
/// `@<source's name>` (recursively, so dependents-of-dependents are
/// included). Output is in BFS order so a downstream rebuild runs
/// closest first. Source must be pinned for the search to find anything.
fn collect_dependents_recursive(
    session: &Session,
    source: ShedId,
    out: &mut Vec<ShedId>,
    visited: &mut HashSet<ShedId>,
) {
    if !visited.insert(source) {
        return;
    }
    let Some(name) = session.shed(source).and_then(|b| b.name.clone()) else {
        return;
    };
    let target = format!("@{name}");
    let direct: Vec<ShedId> = session
        .sheds()
        .filter(|b| b.argv.len() == 1 && b.argv[0] == target)
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
    let Some(shed) = app.session.shed(id) else { return };
    if shed.argv.is_empty() {
        app.flash = Some("nothing to edit".into());
        return;
    }
    let joined = shlex::try_join(shed.argv.iter().map(String::as_str))
        .unwrap_or_else(|_| shed.argv.join(" "));
    app.cmd_edit_cursor = joined.len();
    app.cmd_edit_input = joined;
    app.cmd_edit_input_mode = true;
}

/// Apply the edited command to the cursor shed in place, then queue
/// the shed plus any pinned-name dependents for re-run.
fn commit_cmd_edit(app: &mut App) {
    let input = std::mem::take(&mut app.cmd_edit_input);
    app.cmd_edit_input_mode = false;
    app.cmd_edit_cursor = 0;
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
    let Some(id) = app.session.cursor() else { return };
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
        app.flash = Some(format!("running {dep_count} {label} first, then %{}", target.0));
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
    let Some(id) = app.chain_in_flight else { return };
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

/// Apply `name`'s pipeline to its current capture and serialize the
/// result to bytes — JSON-pretty for structured values, raw passthrough
/// for byte streams. Errors describe what went wrong so the caller can
/// route them into a `Failed` shed state.
fn snapshot_pinned(session: &Session, name: &str) -> Result<Vec<u8>, String> {
    let id = session
        .lookup_by_name(name)
        .ok_or_else(|| format!("no pinned shed named @{name}"))?;
    let shed = session
        .shed(id)
        .ok_or_else(|| format!("@{name} missing from session"))?;
    let capture = shed
        .capture
        .as_ref()
        .ok_or_else(|| format!("@{name} has no captured output yet"))?;

    let value = match apply_pipeline(&capture.stdout, &shed.pipeline) {
        Ok((v, _)) => v,
        Err(e) => return Err(format!("@{name} pipeline error: {e}")),
    };

    let bytes = match value {
        PipelineValue::Bytes(b) => b.to_vec(),
        other @ PipelineValue::Structured(_) => {
            let json = pipeline_value_to_json(other);
            let mut out = serde_json::to_vec_pretty(&json)
                .unwrap_or_else(|e| e.to_string().into_bytes());
            out.push(b'\n');
            out
        }
    };
    Ok(bytes)
}

/// Run `snapshot_pinned` and write the result onto `id` as a synthetic
/// capture. Used both at create time (typing `@name`) and on re-run
/// (Space on a snapshot shed).
fn populate_snapshot(app: &mut App, id: ShedId, name: &str) {
    let started_at = Instant::now();
    match snapshot_pinned(&app.session, name) {
        Ok(bytes) => {
            let capture = Capture {
                stdout: Bytes::from(bytes),
                stderr: Bytes::new(),
                exit_code: Some(0),
                started_at,
                finished_at: Some(Instant::now()),
                truncated: false,
                snapshotted: true,
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

/// Append a new snapshot shed referencing pinned `@name` and queue it
/// (along with any deps) on the run chain. The actual snapshot runs from
/// `perform_run_in_place` so the source's pipeline output is fresh.
fn spawn_pinned_snapshot(app: &mut App, name: &str) {
    app.savepoint();
    let argv = vec![format!("@{name}")];
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
    let next_cursor = ids
        .iter()
        .position(|x| *x == id)
        .and_then(|i| ids.get(i + 1).copied().or_else(|| {
            if i == 0 { None } else { ids.get(i - 1).copied() }
        }));
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
    let Some(shed) = app.session.shed(id) else { return };
    let argv = shed.argv.clone();
    if argv.is_empty() {
        return;
    }

    if is_pinned_ref(&argv) {
        let name = argv[0][1..].to_string();
        populate_snapshot(app, id, &name);
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
fn drain_streams(app: &mut App) {
    let ids: Vec<ShedId> = app.running.keys().copied().collect();
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
        };
        app.session.set_capture(id, partial);
    }
}

async fn reap_completed(app: &mut App) {
    let finished_ids: Vec<ShedId> = app
        .running
        .iter()
        .filter(|(_, c)| c.handle.is_finished())
        .map(|(id, _)| *id)
        .collect();
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
}

async fn handle_key(app: &mut App, key: KeyEvent) {
    // Exit confirmation takes priority over everything: y / n / c (cancel).
    if app.exit_prompt.is_some() {
        handle_exit_prompt_key(app, key);
        return;
    }
    // Notebook save/open input bars are overlaid on top of any focus.
    if app.save_input_mode {
        handle_save_input_key(app, key);
        return;
    }
    if app.open_input_mode {
        handle_open_input_key(app, key);
        return;
    }
    // NoteEdit consumes its own keys so Ctrl-S commits the note rather
    // than triggering the global save-notebook binding.
    if app.focus == Focus::NoteEdit {
        handle_note_edit_key(app, key);
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
    if let Some(id) = app.session.cursor() {
        if app.session.shed(id).is_none() {
            app.session.set_cursor(app.newest_shed_id());
        }
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

/// Quit if clean; otherwise show the save-changes confirmation.
fn request_quit(app: &mut App) {
    if app.dirty {
        app.exit_prompt = Some(ExitPrompt::Confirm);
    } else {
        app.quit = true;
    }
}

/// Open the save input bar — or, if a notebook path is already bound,
/// save immediately and flash the result.
fn begin_save(app: &mut App) {
    if let Some(path) = app.notebook_path.clone() {
        save_to_path(app, &path);
        return;
    }
    app.save_input.clear();
    app.save_cursor = 0;
    app.save_input_mode = true;
}

/// Always open the input bar; the user must type or paste a path.
fn begin_open(app: &mut App) {
    app.open_input = app
        .notebook_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    app.open_cursor = app.open_input.len();
    app.open_input_mode = true;
}

fn save_to_path(app: &mut App, path: &std::path::Path) {
    let nb = Notebook::from_session(&app.session);
    match nb.save(path) {
        Ok(()) => {
            app.notebook_path = Some(path.to_path_buf());
            app.dirty = false;
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
    match handle_text_input(&mut app.save_input, &mut app.save_cursor, &key) {
        InputOutcome::Cancel => {
            app.save_input_mode = false;
            app.save_input.clear();
            app.save_cursor = 0;
            // If the save was triggered by the exit prompt and the user
            // bailed out, drop the exit prompt rather than continuing to
            // hold them hostage.
            if app.exit_prompt == Some(ExitPrompt::AwaitingPath) {
                app.exit_prompt = None;
            }
        }
        InputOutcome::Commit => {
            let path_str = std::mem::take(&mut app.save_input);
            app.save_input_mode = false;
            app.save_cursor = 0;
            let trimmed = path_str.trim();
            if trimmed.is_empty() {
                app.flash = Some("path required".into());
                return;
            }
            let path = PathBuf::from(expand_tilde(trimmed));
            save_to_path(app, &path);
            // If we were saving on exit, complete the quit now that the
            // file is on disk (or fall through if save failed).
            if app.exit_prompt == Some(ExitPrompt::AwaitingPath) && !app.dirty {
                app.exit_prompt = None;
                app.quit = true;
            }
        }
        InputOutcome::Continue => {}
    }
}

fn handle_open_input_key(app: &mut App, key: KeyEvent) {
    match handle_text_input(&mut app.open_input, &mut app.open_cursor, &key) {
        InputOutcome::Cancel => {
            app.open_input_mode = false;
            app.open_input.clear();
            app.open_cursor = 0;
        }
        InputOutcome::Commit => {
            let path_str = std::mem::take(&mut app.open_input);
            app.open_input_mode = false;
            app.open_cursor = 0;
            let trimmed = path_str.trim();
            if trimmed.is_empty() {
                app.flash = Some("path required".into());
                return;
            }
            let path = PathBuf::from(expand_tilde(trimmed));
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
                if !app.dirty {
                    app.exit_prompt = None;
                    app.quit = true;
                }
            } else {
                app.exit_prompt = Some(ExitPrompt::AwaitingPath);
                app.save_input.clear();
                app.save_cursor = 0;
                app.save_input_mode = true;
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
            if let Some(state) = app.palette_state.as_mut() {
                if matches_len > 0 {
                    state.cursor = (state.cursor + 1).min(matches_len - 1);
                }
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

// === Readline-style line editing ===========================================
//
// Pure helpers operating on a `(text: &mut String, cursor: &mut usize)`
// pair. Cursor is a byte offset into `text`, always at a char boundary
// in `[0, text.len()]`. `apply_readline_edit` dispatches readline-style
// keys (Ctrl-A/E/U/K/W, arrows, Home/End, Delete/Backspace, plain
// chars) to the right helper and returns `true` if the key was consumed.

fn tf_insert_char(text: &mut String, cursor: &mut usize, c: char) {
    text.insert(*cursor, c);
    *cursor += c.len_utf8();
}

fn tf_backspace(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let prev = text[..*cursor].chars().next_back().expect("non-empty prefix").len_utf8();
    *cursor -= prev;
    text.replace_range(*cursor..*cursor + prev, "");
}

fn tf_delete(text: &mut String, cursor: &mut usize) {
    if *cursor >= text.len() {
        return;
    }
    let next = text[*cursor..].chars().next().expect("non-empty suffix").len_utf8();
    text.replace_range(*cursor..*cursor + next, "");
}

fn tf_left(text: &str, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let prev = text[..*cursor].chars().next_back().expect("non-empty prefix").len_utf8();
    *cursor -= prev;
}

fn tf_right(text: &str, cursor: &mut usize) {
    if *cursor >= text.len() {
        return;
    }
    let next = text[*cursor..].chars().next().expect("non-empty suffix").len_utf8();
    *cursor += next;
}

fn tf_home(cursor: &mut usize) {
    *cursor = 0;
}

fn tf_end(text: &str, cursor: &mut usize) {
    *cursor = text.len();
}

fn tf_kill_to_beginning(text: &mut String, cursor: &mut usize) {
    text.replace_range(..*cursor, "");
    *cursor = 0;
}

fn tf_kill_to_end(text: &mut String, cursor: &mut usize) {
    text.replace_range(*cursor.., "");
}

/// Word-back boundary: skip whitespace immediately before `cursor`,
/// then skip the run of non-whitespace before that. Returns the byte
/// index of the boundary.
fn tf_word_back_index(text: &str, cursor: usize) -> usize {
    let mut end = cursor;
    while end > 0 {
        let c = text[..end].chars().next_back().expect("non-empty");
        if !c.is_whitespace() {
            break;
        }
        end -= c.len_utf8();
    }
    while end > 0 {
        let c = text[..end].chars().next_back().expect("non-empty");
        if c.is_whitespace() {
            break;
        }
        end -= c.len_utf8();
    }
    end
}

fn tf_word_forward_index(text: &str, cursor: usize) -> usize {
    let mut pos = cursor;
    while pos < text.len() {
        let c = text[pos..].chars().next().expect("non-empty");
        if !c.is_whitespace() {
            break;
        }
        pos += c.len_utf8();
    }
    while pos < text.len() {
        let c = text[pos..].chars().next().expect("non-empty");
        if c.is_whitespace() {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

fn tf_kill_word_back(text: &mut String, cursor: &mut usize) {
    let new_pos = tf_word_back_index(text, *cursor);
    text.replace_range(new_pos..*cursor, "");
    *cursor = new_pos;
}

fn tf_word_left(text: &str, cursor: &mut usize) {
    *cursor = tf_word_back_index(text, *cursor);
}

fn tf_word_right(text: &str, cursor: &mut usize) {
    *cursor = tf_word_forward_index(text, *cursor);
}

/// Outcome of [`handle_text_input`] applied to a single-line input bar:
/// `Commit` if Enter was pressed, `Cancel` for Esc, `Continue` for an
/// editing keystroke (any non-terminal key, even one that didn't
/// actually mutate the buffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputOutcome {
    Commit,
    Cancel,
    Continue,
}

/// Single-line input bar dispatch: handle Enter / Esc as commit /
/// cancel, route everything else through [`apply_readline_edit`].
fn handle_text_input(text: &mut String, cursor: &mut usize, key: &KeyEvent) -> InputOutcome {
    match key.code {
        KeyCode::Enter => InputOutcome::Commit,
        KeyCode::Esc => InputOutcome::Cancel,
        _ => {
            apply_readline_edit(text, cursor, key);
            InputOutcome::Continue
        }
    }
}

/// Apply a readline-style key edit. Returns `true` if the key was
/// consumed (the caller should not run its own key logic). Returns
/// `false` for keys this layer doesn't handle (Enter, Esc, Tab, Up/Down,
/// etc.) — the caller handles those.
fn apply_readline_edit(text: &mut String, cursor: &mut usize, key: &KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) if !ctrl && !alt => {
            tf_insert_char(text, cursor, c);
            true
        }
        KeyCode::Char('a') if ctrl => {
            tf_home(cursor);
            true
        }
        KeyCode::Char('e') if ctrl => {
            tf_end(text, cursor);
            true
        }
        KeyCode::Char('u') if ctrl => {
            tf_kill_to_beginning(text, cursor);
            true
        }
        KeyCode::Char('k') if ctrl => {
            tf_kill_to_end(text, cursor);
            true
        }
        KeyCode::Char('w') if ctrl => {
            tf_kill_word_back(text, cursor);
            true
        }
        KeyCode::Char('b') if ctrl => {
            tf_left(text, cursor);
            true
        }
        KeyCode::Char('f') if ctrl => {
            tf_right(text, cursor);
            true
        }
        KeyCode::Char('b') if alt => {
            tf_word_left(text, cursor);
            true
        }
        KeyCode::Char('f') if alt => {
            tf_word_right(text, cursor);
            true
        }
        KeyCode::Backspace => {
            tf_backspace(text, cursor);
            true
        }
        KeyCode::Delete => {
            tf_delete(text, cursor);
            true
        }
        KeyCode::Home => {
            tf_home(cursor);
            true
        }
        KeyCode::End => {
            tf_end(text, cursor);
            true
        }
        KeyCode::Left if alt => {
            tf_word_left(text, cursor);
            true
        }
        KeyCode::Right if alt => {
            tf_word_right(text, cursor);
            true
        }
        KeyCode::Left => {
            tf_left(text, cursor);
            true
        }
        KeyCode::Right => {
            tf_right(text, cursor);
            true
        }
        _ => false,
    }
}

/// Render a status-bar style input prompt as a [`Paragraph`]: a label
/// in `accent`, then `text` with the cursor visualised inline. Used for
/// every single-line input bar in the bottom status row.
fn render_input_bar(
    label: &str,
    accent: Color,
    text: &str,
    cursor: usize,
) -> Paragraph<'static> {
    let mut spans = vec![
        Span::raw(" ".to_string()),
        Span::styled(
            label.to_string(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(input_spans_with_cursor(text, cursor, accent));
    Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray))
}

/// Render `text` with the cursor visualised as an inverted shed.
/// `accent` is the colour the cursor shed uses for its background;
/// the text under it renders in black.
fn input_spans_with_cursor(text: &str, cursor: usize, accent: Color) -> Vec<Span<'static>> {
    let cursor = cursor.min(text.len());
    let before = text[..cursor].to_string();
    let (at, after) = if cursor >= text.len() {
        (" ".to_string(), String::new())
    } else {
        let c_len = text[cursor..].chars().next().expect("non-empty").len_utf8();
        (
            text[cursor..cursor + c_len].to_string(),
            text[cursor + c_len..].to_string(),
        )
    };
    let cursor_style = Style::default()
        .fg(Color::Black)
        .bg(accent)
        .add_modifier(Modifier::BOLD);
    vec![
        Span::raw(before),
        Span::styled(at, cursor_style),
        Span::raw(after),
    ]
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

// === Tab completion =========================================================
//
// Two surfaces: the main Prompt and the in-place cmd-edit input bar.
// Both append-only Strings (no mid-string cursor), so completion always
// operates on the final whitespace-separated token. Tab cycles forward,
// Shift-Tab cycles backward; any non-Tab key clears the cycle.
//
// Completion source by token shape (in order):
//   $...   env var names
//   @...   pinned shed names  (only when the token starts with @)
//   /...   slash commands       (Prompt focus, argv0 only)
//   .../...    path completion  (anywhere with a path-shape)
//   <argv0>    commands ∪ aliases ∪ builtins
//   <argv1+>   path completion

const COMPLETION_BUILTINS: &[&str] = &["cd", "exit", "quit", "export", "unset"];
const COMPLETION_SLASH: &[&str] = &["/aliases"];

#[derive(Debug, Clone)]
struct CompletionState {
    /// The unchanged prefix of the input (before the token being
    /// completed). Each cycle re-renders the input as
    /// `base_text + matches[idx] + suffix`.
    base_text: String,
    /// The unchanged suffix of the input (everything after the cursor
    /// at the moment Tab was first pressed).
    suffix: String,
    matches: Vec<String>,
    idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionContext {
    EnvVar,
    Pinned,
    Slash,
    Argv0,
    Path,
}

/// Split `text` into (everything before the final token, the final
/// token). The final token starts after the last whitespace char.
fn split_last_token(text: &str) -> (&str, &str) {
    match text.rfind(char::is_whitespace) {
        Some(idx) => {
            let after = idx + text[idx..].chars().next().unwrap().len_utf8();
            (&text[..after], &text[after..])
        }
        None => ("", text),
    }
}

fn classify_completion(focus: Focus, base: &str, token: &str) -> CompletionContext {
    if token.starts_with('$') {
        return CompletionContext::EnvVar;
    }
    if token.starts_with('@') {
        return CompletionContext::Pinned;
    }
    let argv0 = base.trim().is_empty();
    if argv0 {
        if focus == Focus::Prompt && token.starts_with('/') {
            return CompletionContext::Slash;
        }
        if token.starts_with('/') || token.starts_with("./") || token.starts_with("../")
            || token.starts_with("~/")
        {
            return CompletionContext::Path;
        }
        return CompletionContext::Argv0;
    }
    CompletionContext::Path
}

fn env_completions(token: &str) -> Vec<String> {
    let prefix = &token[1..]; // strip $
    let mut names: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with(prefix))
        .collect();
    names.sort();
    names.dedup();
    names.into_iter().map(|n| format!("${n}")).collect()
}

fn pinned_completions(session: &Session, token: &str) -> Vec<String> {
    let prefix = &token[1..]; // strip @
    let mut names: Vec<String> = session
        .sheds()
        .filter_map(|b| b.name.clone())
        .filter(|n| n.starts_with(prefix))
        .collect();
    names.sort();
    names.dedup();
    names.into_iter().map(|n| format!("@{n}")).collect()
}

fn slash_completions(token: &str) -> Vec<String> {
    COMPLETION_SLASH
        .iter()
        .filter(|cmd| cmd.starts_with(token))
        .map(|s| (*s).to_string())
        .collect()
}

fn argv0_completions(aliases: &AliasFile, token: &str) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut all: BTreeSet<String> = BTreeSet::new();
    for b in COMPLETION_BUILTINS {
        if b.starts_with(token) {
            all.insert((*b).to_string());
        }
    }
    for alias in &aliases.aliases {
        if alias.name.starts_with(token) {
            all.insert(alias.name.clone());
        }
    }
    for cmd in path_executables(token) {
        all.insert(cmd);
    }
    all.into_iter().collect()
}

/// Shell out to `carapace <argv0> export <argv0> <tokens...>` to get
/// rich completions for the current argument position. Returns `None`
/// when carapace isn't installed, when the spawn fails, or when its
/// output isn't valid export JSON — in all of those cases the caller
/// falls back to filesystem path completion.
///
/// Carapace's `export` format is a JSON object whose top-level
/// `values` array holds `{value, display, description, style, tag}`
/// records; we surface just `value` strings here. The carapace docs
/// claim sub-10ms response on warm cache, which is well within
/// interactive Tab-press latency.
fn run_carapace_export(argv0: &str, tokens: &[String]) -> Option<Vec<u8>> {
    let output = std::process::Command::new("carapace")
        .arg(argv0)
        .arg("export")
        .arg(argv0)
        .args(tokens)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// Parse a carapace `export`-format JSON document into a sorted, deduped
/// list of completion values that start with `token`. Carapace's own
/// completers usually return only matching values for the partial, but
/// we filter defensively so we behave the same regardless.
fn carapace_completions_from_export(json: &[u8], token: &str) -> Vec<String> {
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(values) = root.get("values").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = values
        .iter()
        .filter_map(|item| item.get("value").and_then(|v| v.as_str()).map(String::from))
        .filter(|v| v.starts_with(token))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn carapace_completions(argv0: &str, tokens: &[String], token: &str) -> Vec<String> {
    let Some(json) = run_carapace_export(argv0, tokens) else {
        return Vec::new();
    };
    carapace_completions_from_export(&json, token)
}

fn path_executables(prefix: &str) -> Vec<String> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for dir in std::env::split_paths(&path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !name.starts_with(prefix) {
                continue;
            }
            if !is_executable_entry(&entry) {
                continue;
            }
            if seen.insert(name.clone()) {
                out.push(name);
            }
        }
    }
    out
}

#[cfg(unix)]
fn is_executable_entry(entry: &std::fs::DirEntry) -> bool {
    use std::os::unix::fs::PermissionsExt;
    entry
        .metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_entry(_entry: &std::fs::DirEntry) -> bool {
    true
}

fn path_completions(token: &str) -> Vec<String> {
    // dir_str is what we keep in the displayed match (so `~/` stays
    // `~/`), dir_lookup is the actual filesystem path.
    let (dir_str, prefix) = match token.rfind('/') {
        Some(idx) => (&token[..=idx], &token[idx + 1..]),
        None => ("", token),
    };
    let dir_lookup: PathBuf = if dir_str.is_empty() {
        PathBuf::from(".")
    } else if let Some(rest) = dir_str.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(home) => {
                let mut p = PathBuf::from(home);
                p.push(rest);
                p
            }
            None => PathBuf::from(dir_str),
        }
    } else {
        PathBuf::from(dir_str)
    };

    let Ok(entries) = std::fs::read_dir(&dir_lookup) else {
        return Vec::new();
    };
    let show_hidden = prefix.starts_with('.');
    let mut rows: Vec<(String, bool)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(prefix) {
                return None;
            }
            if !show_hidden && name.starts_with('.') {
                return None;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            Some((name, is_dir))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.into_iter()
        .map(|(name, is_dir)| {
            let suffix = if is_dir { "/" } else { "" };
            format!("{dir_str}{name}{suffix}")
        })
        .collect()
}

/// Compute the (base_text, matches) pair for completing `text` at the
/// final token.
fn compute_completions(
    session: &Session,
    aliases: &AliasFile,
    focus: Focus,
    text: &str,
) -> (String, Vec<String>) {
    let (base, token) = split_last_token(text);
    let ctx = classify_completion(focus, base, token);
    let matches = match ctx {
        CompletionContext::EnvVar => env_completions(token),
        CompletionContext::Pinned => pinned_completions(session, token),
        CompletionContext::Slash => slash_completions(token),
        CompletionContext::Argv0 => argv0_completions(aliases, token),
        CompletionContext::Path => path_or_carapace_completions(base, token),
    };
    (base.to_string(), matches)
}

/// For an argv1+ position, ask carapace first (if installed) for
/// command-aware completions like git branch names, kubectl flags,
/// docker image tags, etc. Fall back to filesystem path completion if
/// carapace isn't available or returns no matches.
fn path_or_carapace_completions(base: &str, token: &str) -> Vec<String> {
    let prior: Vec<String> = base.split_whitespace().map(String::from).collect();
    if let Some(argv0) = prior.first() {
        let mut spans = prior.clone();
        spans.push(token.to_string());
        let from_carapace = carapace_completions(argv0, &spans, token);
        if !from_carapace.is_empty() {
            return from_carapace;
        }
    }
    path_completions(token)
}

fn current_input_state(app: &App) -> Option<(String, usize)> {
    match app.focus {
        Focus::Prompt => Some((app.prompt.clone(), app.prompt_cursor)),
        Focus::EditShed if app.cmd_edit_input_mode => {
            Some((app.cmd_edit_input.clone(), app.cmd_edit_cursor))
        }
        _ => None,
    }
}

fn set_current_input(app: &mut App, text: String, cursor: usize) {
    match app.focus {
        Focus::Prompt => {
            app.prompt = text;
            app.prompt_cursor = cursor;
        }
        Focus::EditShed if app.cmd_edit_input_mode => {
            app.cmd_edit_input = text;
            app.cmd_edit_cursor = cursor;
        }
        _ => {}
    }
}

/// Handle a Tab (dir = +1) or Shift-Tab (dir = -1) press in a
/// completion context. On the first press, builds a fresh match list
/// and applies the first match. Subsequent presses cycle through
/// `app.completion.matches`.
fn cycle_completion(app: &mut App, dir: i32) {
    if app.completion.is_none() {
        let Some((text, cursor)) = current_input_state(app) else {
            return;
        };
        let prefix = &text[..cursor];
        let suffix = text[cursor..].to_string();
        let focus = app.focus;
        let (base, matches) = compute_completions(&app.session, &app.aliases, focus, prefix);
        if matches.is_empty() {
            app.flash = Some("no completions".into());
            return;
        }
        let new_cursor = base.len() + matches[0].len();
        set_current_input(
            app,
            format!("{base}{}{suffix}", matches[0]),
            new_cursor,
        );
        app.completion = Some(CompletionState {
            base_text: base,
            suffix,
            matches,
            idx: 0,
        });
        return;
    }
    let state = app.completion.as_mut().unwrap();
    let n = state.matches.len() as isize;
    state.idx = ((state.idx as isize + dir as isize).rem_euclid(n)) as usize;
    let new_cursor = state.base_text.len() + state.matches[state.idx].len();
    let new_text = format!(
        "{}{}{}",
        state.base_text, state.matches[state.idx], state.suffix
    );
    set_current_input(app, new_text, new_cursor);
}

/// Dispatch a mouse event by hit-testing the click regions registered
/// during the last draw pass. Only left-button-down counts as a
/// "click" today; scroll and motion events are ignored.
fn handle_mouse(app: &mut App, me: MouseEvent) {
    if !matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
        return;
    }
    let hit = app
        .click_regions
        .iter()
        .find(|r| {
            me.column >= r.rect.x
                && me.column < r.rect.x + r.rect.width
                && me.row >= r.rect.y
                && me.row < r.rect.y + r.rect.height
        })
        .map(|r| r.action);
    let Some(action) = hit else {
        return;
    };
    app.flash = None;
    match action {
        ClickAction::DeleteBlock(id) => delete_shed(app, id),
    }
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
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache"))
        })?;
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
    if app.rerun_input_mode {
        match handle_text_input(&mut app.rerun_input, &mut app.rerun_cursor, &key) {
            InputOutcome::Commit => commit_rerun(app),
            InputOutcome::Cancel => {
                app.rerun_input_mode = false;
                app.rerun_input.clear();
                app.rerun_cursor = 0;
                app.rerun_source_id = None;
            }
            InputOutcome::Continue => {}
        }
        return;
    }
    if app.pin_input_mode {
        match handle_text_input(&mut app.pin_input, &mut app.pin_cursor, &key) {
            InputOutcome::Commit => commit_pin(app),
            InputOutcome::Cancel => {
                app.pin_input_mode = false;
                app.pin_input.clear();
                app.pin_cursor = 0;
            }
            InputOutcome::Continue => {}
        }
        return;
    }
    if app.write_input_mode {
        match handle_text_input(&mut app.write_input, &mut app.write_cursor, &key) {
            InputOutcome::Commit => commit_write(app),
            InputOutcome::Cancel => {
                app.write_input_mode = false;
                app.write_input.clear();
                app.write_cursor = 0;
            }
            InputOutcome::Continue => {}
        }
        return;
    }
    if app.cmd_edit_input_mode {
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
        match handle_text_input(&mut app.cmd_edit_input, &mut app.cmd_edit_cursor, &key) {
            InputOutcome::Commit => commit_cmd_edit(app),
            InputOutcome::Cancel => {
                app.cmd_edit_input_mode = false;
                app.cmd_edit_input.clear();
                app.cmd_edit_cursor = 0;
            }
            InputOutcome::Continue => {}
        }
        return;
    }
    if app.alias_overwrite.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => confirm_alias_overwrite(app, true),
            KeyCode::Char('n') | KeyCode::Char('N')
            | KeyCode::Char('c') | KeyCode::Char('C')
            | KeyCode::Esc => confirm_alias_overwrite(app, false),
            _ => {}
        }
        return;
    }
    if app.alias_name_input_mode {
        match handle_text_input(&mut app.alias_name_input, &mut app.alias_name_cursor, &key) {
            InputOutcome::Commit => commit_alias_save(app),
            InputOutcome::Cancel => {
                app.alias_name_input_mode = false;
                app.alias_name_input.clear();
                app.alias_name_cursor = 0;
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
        KeyCode::Char('v') => {
            if app.session.cursor().is_some() {
                app.expand_scroll = 0;
                app.focus = Focus::ShedExpand;
            }
        }
        KeyCode::Char('w') => {
            if app.session.cursor().is_some() {
                app.write_input_mode = true;
                app.write_input.clear();
                app.write_cursor = 0;
            }
        }
        KeyCode::Char('p') => {
            if let Some(id) = app.session.cursor() {
                let existing = app
                    .session
                    .shed(id)
                    .and_then(|b| b.name.clone())
                    .unwrap_or_default();
                app.pin_cursor = existing.len();
                app.pin_input = existing;
                app.pin_input_mode = true;
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
            if let Some(id) = app.session.cursor() {
                if let Some(shed) = app.session.shed(id) {
                    let joined = shlex::try_join(shed.argv.iter().map(String::as_str))
                        .unwrap_or_else(|_| shed.argv.join(" "));
                    app.rerun_cursor = joined.len();
                    app.rerun_input = joined;
                    app.rerun_input_mode = true;
                    app.rerun_source_id = Some(id);
                }
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
    if app.cmd_edit_input_mode {
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
        match handle_text_input(&mut app.cmd_edit_input, &mut app.cmd_edit_cursor, &key) {
            InputOutcome::Commit => commit_cmd_edit(app),
            InputOutcome::Cancel => {
                app.cmd_edit_input_mode = false;
                app.cmd_edit_input.clear();
                app.cmd_edit_cursor = 0;
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
    let input = std::mem::take(&mut app.rerun_input);
    app.rerun_input_mode = false;
    app.rerun_cursor = 0;
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
    let name = std::mem::take(&mut app.pin_input).trim().to_string();
    app.pin_input_mode = false;
    app.pin_cursor = 0;
    let Some(id) = app.session.cursor() else { return };

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
    let path = std::mem::take(&mut app.write_input);
    app.write_input_mode = false;
    app.write_cursor = 0;
    let path = path.trim();
    if path.is_empty() {
        app.flash = Some("path required".into());
        return;
    }
    let Some(id) = app.session.cursor() else { return };
    let Some(shed) = app.session.shed(id) else { return };

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
    let value = match apply_pipeline(&capture.stdout, &shed.pipeline) {
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
    let value = match apply_pipeline(&capture.stdout, &shed.pipeline) {
        Ok((v, _)) => v,
        Err(e) => return e.into_bytes(),
    };
    let json = pipeline_value_to_json(value);
    let mut out =
        serde_json::to_vec_pretty(&json).unwrap_or_else(|e| e.to_string().into_bytes());
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
        Value::Bytes(b) => {
            serde_json::Value::String(String::from_utf8_lossy(&b).to_string())
        }
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
    if app.search_input_mode {
        match handle_text_input(&mut app.search_input, &mut app.search_cursor, &key) {
            InputOutcome::Cancel => {
                app.search_input_mode = false;
                app.search_input.clear();
                app.search_cursor = 0;
                app.search_query.clear();
                app.expand_scroll = app.search_anchor_scroll;
            }
            InputOutcome::Commit => {
                app.search_input_mode = false;
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
            app.search_input_mode = true;
            app.search_input_backward = false;
            app.search_input.clear();
            app.search_cursor = 0;
            app.search_query.clear();
            app.search_anchor_scroll = app.expand_scroll;
        }
        KeyCode::Char('?') => {
            app.search_input_mode = true;
            app.search_input_backward = true;
            app.search_input.clear();
            app.search_cursor = 0;
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
    app.search_query = app.search_input.clone();
    if app.search_query.is_empty() {
        app.expand_scroll = app.search_anchor_scroll;
        return;
    }
    let Some(regex) = try_compile(&app.search_query, app.search_case_insensitive) else {
        return;
    };
    let Some(id) = app.session.cursor() else { return };
    let Some(shed) = app.session.shed(id) else { return };
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
    let Some(id) = app.session.cursor() else { return };
    let Some(shed) = app.session.shed(id) else { return };
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
        Some(capture) => match apply_pipeline(&capture.stdout, &shed.pipeline) {
            Ok((value, _drops)) => render_pipeline_value_with_max(value, usize::MAX, false),
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
            new_spans.push(Span::styled(
                span_text[local_pos..].to_string(),
                span_style,
            ));
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
    let Some(id) = app.session.cursor() else { return };
    let Some(shed) = app.session.shed(id) else { return };
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
    let Some(id) = app.session.cursor() else { return };
    let Some(shed) = app.session.shed(id) else { return };
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
    let last_visible = if n < MAX_SORT_KEYS { n } else { n.saturating_sub(1) };

    match key.code {
        KeyCode::Up => {
            state.sort_keys_cursor = state.sort_keys_cursor.saturating_sub(1);
        }
        KeyCode::Down => {
            state.sort_keys_cursor = (state.sort_keys_cursor + 1).min(last_visible);
        }
        KeyCode::Left | KeyCode::Right => {
            if state.sort_keys_cursor < n {
                let cols = state.available_columns.len() as i32;
                let delta: i32 = if matches!(key.code, KeyCode::Right) { 1 } else { -1 };
                let cur = state.sort_keys[state.sort_keys_cursor].0 as i32;
                let new = (cur + delta).rem_euclid(cols) as usize;
                state.sort_keys[state.sort_keys_cursor].0 = new;
            }
        }
        KeyCode::Char(' ') => {
            if state.sort_keys_cursor < n {
                let cur = state.sort_keys[state.sort_keys_cursor].1;
                state.sort_keys[state.sort_keys_cursor].1 = match cur {
                    SortDirection::Asc => SortDirection::Desc,
                    SortDirection::Desc => SortDirection::Asc,
                };
            }
        }
        KeyCode::Char('a') => {
            if n < MAX_SORT_KEYS {
                state.sort_keys.push((0, SortDirection::Asc));
                state.sort_keys_cursor = state.sort_keys.len() - 1;
            }
        }
        KeyCode::Char('x') | KeyCode::Backspace | KeyCode::Delete => {
            if state.sort_keys_cursor < n && state.sort_keys.len() > 1 {
                state.sort_keys.remove(state.sort_keys_cursor);
                if state.sort_keys_cursor >= state.sort_keys.len() {
                    state.sort_keys_cursor = state.sort_keys.len().saturating_sub(1);
                }
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
        KeyCode::Left | KeyCode::Up => {
            if state.where_active_clause > 0 {
                state.where_active_clause -= 1;
            }
        }
        KeyCode::Right | KeyCode::Down => {
            if state.where_active_clause + 1 < state.where_clauses.len() {
                state.where_active_clause += 1;
            }
        }
        KeyCode::Char('a') => {
            state
                .where_clauses
                .push(WhereClause::default_for(&state.available_columns));
            state.where_active_clause = state.where_clauses.len() - 1;
        }
        KeyCode::Char('x') | KeyCode::Backspace | KeyCode::Delete => {
            if state.where_clauses.len() > 1 {
                state.where_clauses.remove(state.where_active_clause);
                if state.where_active_clause >= state.where_clauses.len() {
                    state.where_active_clause = state.where_clauses.len() - 1;
                }
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
    let Some(id) = app.session.cursor() else { return };
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
    let Some(id) = app.session.cursor() else { return };
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
    let new_len = app
        .session
        .shed(id)
        .map(|b| b.pipeline.len())
        .unwrap_or(0);
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
    if !app.history.last().map_or(false, |last| last == &original) {
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
    if !force_fullscreen && argv.len() == 1 {
        if let Some(alias) = app.aliases.lookup(&argv[0]).cloned() {
            spawn_alias(app, &alias);
            return;
        }
    }

    if force_fullscreen || needs_fullscreen(&argv) {
        app.pending_handover = Some(HandoverRequest {
            argv,
            reuse_shed: None,
        });
        return;
    }

    if is_pinned_ref(&argv) {
        let name = argv[0][1..].to_string();
        spawn_pinned_snapshot(app, &name);
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
            app.session
                .set_state(id, ShedState::Failed(e.to_string()));
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

    let result = target.and_then(|t| std::env::set_current_dir(&t).map(|_| t).map_err(|e| e.to_string()));

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
    app.quit = true;
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
    let prev_value = app.session.shed(state.shed_id).and_then(|b| match state.position {
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
        KeyCode::Backspace => {
            if state.cursor > 0 {
                state.buffer.remove(state.cursor - 1);
                state.cursor -= 1;
            }
        }
        KeyCode::Delete => {
            if state.cursor < state.buffer.len() {
                state.buffer.remove(state.cursor);
            }
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
        app.session
            .set_state(id, ShedState::Failed(format!("export: {}", errors.join("; "))));
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
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

fn draw(f: &mut Frame, app: &App, regions: &mut Vec<ClickRegion>) {
    match app.focus {
        Focus::FilterEdit => draw_filter_edit(f, app),
        Focus::ShedExpand => draw_shed_expand(f, app),
        Focus::EnvEdit => draw_env_edit(f, app),
        Focus::Palette => draw_palette(f, app),
        Focus::NoteEdit => draw_note_edit(f, app),
        Focus::AliasManage => draw_alias_manage(f, app),
        _ => draw_repl(f, app, regions),
    }
}

fn draw_alias_manage(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    let title = format!("aliases  ({} entries)", app.aliases.aliases.len());
    draw_header(f, chunks[0], &title);

    let body_area = chunks[1];
    let visible = body_area.height as usize;
    let total = app.aliases.aliases.len();
    let cursor = app
        .alias_manage
        .as_ref()
        .map(|s| s.cursor.min(total.saturating_sub(1)))
        .unwrap_or(0);
    let scroll_offset = if total > visible && cursor + 1 > visible {
        cursor + 1 - visible
    } else {
        0
    };

    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let name_style_unselected = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if app.aliases.aliases.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no aliases — press A on a shed to save one)",
            dim,
        )));
    } else {
        for (i, alias) in app
            .aliases
            .aliases
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible)
        {
            let selected = i == cursor;
            let prefix = if selected { "▸ " } else { "  " };
            let name_style = if selected {
                highlight
            } else {
                name_style_unselected
            };
            let argv_style = if selected {
                highlight
            } else {
                Style::default().fg(Color::White)
            };
            let pipeline_summary = if alias.pipeline.is_empty() {
                String::new()
            } else {
                let mut s = String::from(" │ ");
                for (j, f) in alias.pipeline.iter().enumerate() {
                    if j > 0 {
                        s.push_str(" │ ");
                    }
                    s.push_str(&describe_filter(f));
                }
                s
            };
            let argv_str = alias.argv.join(" ");
            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(alias.name.clone(), name_style),
                Span::raw("  "),
                Span::styled(argv_str, argv_style),
                Span::styled(pipeline_summary, dim),
            ]));
        }
    }
    let widget = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(widget, body_area);

    draw_status(f, chunks[2], app);
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
            if let Some(state) = app.alias_manage.as_mut() {
                if total > 0 {
                    state.cursor = (state.cursor + 1).min(total - 1);
                }
            }
        }
        KeyCode::Char('x') | KeyCode::Char('d') | KeyCode::Delete => {
            let cursor = app.alias_manage.as_ref().map(|s| s.cursor).unwrap_or(0);
            if let Some(alias) = app.aliases.aliases.get(cursor).cloned() {
                if app.aliases.delete(&alias.name) {
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
        return Err(format!("name can't start with `{}`", name.chars().next().unwrap()));
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
    app.alias_name_input.clear();
    app.alias_name_cursor = 0;
    app.alias_name_input_mode = true;
}

fn commit_alias_save(app: &mut App) {
    let raw = std::mem::take(&mut app.alias_name_input);
    app.alias_name_input_mode = false;
    app.alias_name_cursor = 0;
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
    let Some(alias) = app.alias_overwrite.take() else { return };
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
    app.cmd_edit_input = format!("{joined} ");
    app.cmd_edit_cursor = app.cmd_edit_input.len();
    app.cmd_edit_input_mode = true;
}

fn draw_note_edit(f: &mut Frame, app: &App) {
    let Some(state) = app.note_edit.as_ref() else {
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    let title = match state.position {
        NotePosition::Pre => format!("note before %{}", state.shed_id.0),
        NotePosition::Post => format!("note after %{}", state.shed_id.0),
    };
    draw_header(f, chunks[0], &title);

    // Render buffer with an inline cursor marker. Walk the chars and
    // emit lines split on '\n'; the cursor sits between two characters
    // (or at start / end) and shows as a colored "▏".
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let cursor_span = Span::styled(
        "▏",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let mut current_line_text = String::new();
    let mut cursor_emitted = false;

    let push_line = |lines: &mut Vec<Line<'static>>, spans: Vec<Span<'static>>| {
        let mut prefixed = vec![Span::styled(
            "▎ ",
            Style::default().fg(Color::DarkGray),
        )];
        prefixed.extend(spans);
        lines.push(Line::from(prefixed));
    };

    for (i, &c) in state.buffer.iter().enumerate() {
        if i == state.cursor && !cursor_emitted {
            if !current_line_text.is_empty() {
                current.push(Span::raw(current_line_text.clone()));
                current_line_text.clear();
            }
            current.push(cursor_span.clone());
            cursor_emitted = true;
        }
        if c == '\n' {
            if !current_line_text.is_empty() {
                current.push(Span::raw(current_line_text.clone()));
                current_line_text.clear();
            }
            push_line(&mut lines, std::mem::take(&mut current));
        } else {
            current_line_text.push(c);
        }
    }
    if !cursor_emitted && state.cursor == state.buffer.len() {
        if !current_line_text.is_empty() {
            current.push(Span::raw(current_line_text.clone()));
            current_line_text.clear();
        }
        current.push(cursor_span.clone());
    } else if !current_line_text.is_empty() {
        current.push(Span::raw(current_line_text.clone()));
    }
    push_line(&mut lines, current);

    let widget = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(widget, chunks[1]);

    draw_status(f, chunks[2], app);
}

fn draw_palette(f: &mut Frame, app: &App) {
    let Some(state) = app.palette_state.as_ref() else {
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, chunks[0], "command palette");

    // Input bar with bottom border separator.
    let input_box = TuiBlock::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(Color::DarkGray));
    let input_widget = Paragraph::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "› ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(state.input.clone()),
        Span::styled("▏", Style::default().fg(Color::Cyan)),
    ]))
    .block(input_box);
    f.render_widget(input_widget, chunks[1]);

    // Filtered list.
    let matches = matches_for_input(&state.input, app);
    let body_area = chunks[2];
    let visible = body_area.height as usize;
    let cursor = state.cursor.min(matches.len().saturating_sub(1));
    let scroll_offset = if cursor >= visible {
        cursor + 1 - visible
    } else {
        0
    };

    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let mut lines: Vec<Line<'static>> = Vec::new();
    if matches.is_empty() {
        lines.push(Line::from(Span::styled("  (no matches)", dim)));
    } else {
        for (i, action) in matches.iter().enumerate().skip(scroll_offset).take(visible) {
            let selected = i == cursor;
            let prefix = if selected { "▸ " } else { "  " };
            let name_style = if selected {
                highlight
            } else {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            };
            let desc_style = if selected { highlight } else { dim };
            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(action.name, name_style),
                Span::raw("  "),
                Span::styled(action.description, desc_style),
            ]));
        }
    }
    let list_widget = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(list_widget, body_area);

    draw_status(f, chunks[3], app);
}

fn draw_env_edit(f: &mut Frame, app: &App) {
    let Some(state) = app.env_edit.as_ref() else {
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    let entries = state.entries();
    let total = entries.len();
    let title = if state.filter.is_empty() {
        format!("env  ({total} vars)")
    } else {
        format!("env  ({total} vars · filter \"{}\")", state.filter)
    };
    draw_header(f, chunks[0], &title);

    // Body: scrollable list with cursor following
    let body_area = chunks[1];
    let visible = body_area.height as usize;
    let cursor = state.cursor.min(total.saturating_sub(1));
    let scroll_offset = if cursor >= visible {
        cursor + 1 - visible
    } else {
        0
    };

    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let key_style = Style::default().fg(Color::LightCyan);
    let val_style = Style::default().fg(Color::Gray);

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, (k, v)) in entries.iter().enumerate().skip(scroll_offset).take(visible) {
        let selected = i == cursor;
        let prefix = if selected { "▸ " } else { "  " };
        let mut spans = vec![
            Span::styled(prefix, if selected { highlight } else { dim }),
            Span::styled(k.clone(), if selected { highlight } else { key_style }),
            Span::styled(" = ", dim),
            Span::styled(v.clone(), if selected { highlight } else { val_style }),
        ];
        if selected {
            spans.insert(0, Span::raw(""));
        }
        lines.push(Line::from(spans));
    }
    if total == 0 {
        lines.push(Line::from(Span::styled(
            "  (no matching vars)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, body_area);

    // Status bar / input prompt
    match &state.input_mode {
        EnvInputMode::Filter => {
            let widget = Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "/",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(state.filter.clone()),
                Span::styled("▏", Style::default().fg(Color::Yellow)),
            ]))
            .style(Style::default().bg(Color::DarkGray));
            f.render_widget(widget, chunks[2]);
        }
        EnvInputMode::Edit(key) => {
            let widget = Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("edit {key}: "),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(state.input_buffer.clone()),
                Span::styled("▏", Style::default().fg(Color::Yellow)),
            ]))
            .style(Style::default().bg(Color::DarkGray));
            f.render_widget(widget, chunks[2]);
        }
        EnvInputMode::Add => {
            let widget = Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "add KEY=VALUE: ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(state.input_buffer.clone()),
                Span::styled("▏", Style::default().fg(Color::Yellow)),
            ]))
            .style(Style::default().bg(Color::DarkGray));
            f.render_widget(widget, chunks[2]);
        }
        EnvInputMode::None => draw_status(f, chunks[2], app),
    }
}

fn draw_shed_expand(f: &mut Frame, app: &App) {
    let Some(id) = app.session.cursor() else {
        return;
    };
    let Some(shed) = app.session.shed(id) else {
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    // Compute the full pipeline output (no row cap) and the lines to render.
    let all_lines: Vec<Line<'static>> = match shed.capture.as_ref() {
        Some(capture) => match apply_pipeline(&capture.stdout, &shed.pipeline) {
            Ok((value, _drops)) => render_pipeline_value_with_max(value, usize::MAX, false),
            Err(e) => filter_error_lines(&e),
        },
        None => vec![Line::from(Span::styled(
            "      (no capture)",
            Style::default().fg(Color::DarkGray),
        ))],
    };

    let total = all_lines.len();
    let body_area = chunks[1];
    let visible = body_area.height as usize;
    let max_scroll = total.saturating_sub(visible);
    let scroll = app.expand_scroll.min(max_scroll);
    let visible_end = (scroll + visible).min(total);

    let mut title = if total == 0 {
        format!("inspect  %{}  {}", shed.id.0, shed.argv.join(" "))
    } else {
        format!(
            "inspect  %{}  {}    lines {}-{} of {}",
            shed.id.0,
            shed.argv.join(" "),
            scroll + 1,
            visible_end,
            total,
        )
    };
    let regex = try_compile(&app.search_query, app.search_case_insensitive);
    if !app.search_query.is_empty() {
        let flags = if app.search_case_insensitive { " (i)" } else { "" };
        let suffix = match &regex {
            Some(r) => {
                let count = find_matches_regex(&all_lines, r).len();
                format!(
                    "    /{}{flags}  ({} match{})",
                    app.search_query,
                    count,
                    if count == 1 { "" } else { "es" },
                )
            }
            None => format!("    /{}{flags}  (invalid regex)", app.search_query),
        };
        title.push_str(&suffix);
    }
    draw_header(f, chunks[0], &title);

    let visible_lines: Vec<Line<'static>> = all_lines
        .into_iter()
        .skip(scroll)
        .take(visible)
        .map(|line| match &regex {
            Some(r) => highlight_matches_in_line(line, r),
            None => line,
        })
        .collect();
    let para = Paragraph::new(visible_lines).wrap(Wrap { trim: false });
    f.render_widget(para, body_area);

    draw_status(f, chunks[2], app);
}

fn draw_repl(f: &mut Frame, app: &App, regions: &mut Vec<ClickRegion>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    let cwd = std::env::current_dir()
        .ok()
        .map(|p| collapse_home_in_path(&p))
        .unwrap_or_else(|| "?".into());
    draw_header(f, chunks[0], &cwd);
    draw_sheds(f, chunks[1], app, regions);
    draw_status(f, chunks[2], app);
}

fn collapse_home_in_path(p: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok()) {
        if let Some(s) = p.to_str() {
            if let Some(rest) = s.strip_prefix(&home) {
                return format!("~{rest}");
            }
        }
    }
    p.display().to_string()
}

fn draw_filter_edit(f: &mut Frame, app: &App) {
    let Some(state) = app.filter_edit.as_ref() else {
        return;
    };
    let Some(shed) = app.session.shed(state.shed_id) else {
        return;
    };

    let stack_rows = match state.mode {
        EditMode::Add | EditMode::Insert(_) => shed.pipeline.len() + 1,
        EditMode::Edit(_) => shed.pipeline.len(),
    };
    let stack_height = stack_rows.max(1) as u16 + 2;
    let form_height = state.form_lines() + 2;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(stack_height),
            Constraint::Length(form_height),
            Constraint::Length(1),
        ])
        .split(f.area());

    let outcome = hypothetical_outcome(shed, state);
    let drops: Vec<usize> = outcome
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|(_, d)| d.clone())
        .unwrap_or_default();

    let title = format!("editing  %{}  {}", shed.id.0, shed.argv.join(" "));
    draw_header(f, chunks[0], &title);
    draw_preview_pane(f, chunks[1], &outcome);
    draw_stack_pane(f, chunks[2], shed, state, &drops);
    draw_form_pane(f, chunks[3], state);
    draw_status(f, chunks[4], app);
}

fn hypothetical_outcome(
    shed: &Shed,
    state: &FilterEditState,
) -> Option<Result<(PipelineValue, Vec<usize>), String>> {
    let capture = shed.capture.as_ref()?;
    let mut hypothetical: Vec<FilterSpec> = shed.pipeline.clone();
    match (state.mode, state.build_filter()) {
        (EditMode::Add, Some(spec)) => hypothetical.push(spec),
        (EditMode::Edit(i), Some(spec)) if i < hypothetical.len() => hypothetical[i] = spec,
        (EditMode::Insert(i), Some(spec)) => {
            let pos = i.min(hypothetical.len());
            hypothetical.insert(pos, spec);
        }
        _ => {}
    }
    Some(apply_pipeline(&capture.stdout, &hypothetical))
}

fn draw_header(f: &mut Frame, area: Rect, title: &str) {
    let header = Paragraph::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "shed",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ·  {title}"),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    f.render_widget(header, area);
}

fn draw_sheds(f: &mut Frame, area: Rect, app: &App, regions: &mut Vec<ClickRegion>) {
    let cursor_id = app.session.cursor();
    let cursor_visible = matches!(app.focus, Focus::ShedCursor | Focus::EditShed);
    let sheds: Vec<&shed_core::Shed> = app.session.sheds().collect();

    // Render each shed's interior content + record selection state.
    let mut renders: Vec<(Vec<Line<'static>>, bool, bool)> = Vec::with_capacity(sheds.len());
    for shed in &sheds {
        let selected = cursor_visible && cursor_id == Some(shed.id);
        let editing = selected && app.focus == Focus::EditShed;
        let pipeline_cursor = if editing {
            Some(app.pipeline_cursor)
        } else {
            None
        };
        let command_focused = editing && app.command_focused;
        let lines = render_shed(shed, selected, editing, pipeline_cursor, command_focused);
        renders.push((lines, selected, editing));
    }

    // Box height = content lines + 2 (top + bottom border).
    let heights: Vec<u16> = renders
        .iter()
        .map(|(l, _, _)| (l.len() as u16).saturating_add(2))
        .collect();

    // Scratch box at the end: 3 lines (border + 1 content row + border).
    // Reserve its height up front so shed top-clipping accounts for it.
    let scratch_height: u16 = 3;
    let avail = area.height.saturating_sub(scratch_height);

    // Top-clip oldest sheds if total exceeds available height — newest
    // sheds always stay visible. Walk from end backwards, accumulating.
    let mut total: u16 = 0;
    let mut start = renders.len();
    for i in (0..renders.len()).rev() {
        if total.saturating_add(heights[i]) > avail {
            break;
        }
        total = total.saturating_add(heights[i]);
        start = i;
    }

    let visible = &renders[start..];
    let mut constraints: Vec<Constraint> = Vec::with_capacity(visible.len() + 2);
    for h in &heights[start..] {
        constraints.push(Constraint::Length(*h));
    }
    // Leftover space pushes the scratch to the bottom.
    constraints.push(Constraint::Min(0));
    constraints.push(Constraint::Length(scratch_height));

    let rects = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    for (i, (lines, selected, editing)) in visible.iter().enumerate() {
        draw_one_shed(
            f,
            rects[i],
            sheds[start + i],
            lines,
            *selected,
            *editing,
            regions,
        );
    }
    let scratch_rect = rects[rects.len() - 1];
    draw_scratch_box(f, scratch_rect, app);
}

/// Render the always-present "scratch" / prompt box at the end of the
/// shed list. When focus is `Prompt`, the buffer is rendered inside the
/// box with a cursor; otherwise the box shows a hint inviting the user to
/// press `/` (or scroll down) to start typing.
fn draw_scratch_box(f: &mut Frame, area: Rect, app: &App) {
    let active = app.focus == Focus::Prompt;
    // "Selected" means the scratch box has been navigated to via ↓ from
    // ShedCursor but the user hasn't activated it yet (cyan, matching
    // the ShedCursor selection look on real sheds). Distinct from
    // active (green) so it's clear what mode keystrokes go to.
    let selected = !active
        && app.focus == Focus::ShedCursor
        && app.session.cursor().is_none();

    let title_style = if active {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    };
    let next_id = app.session.sheds().last().map(|b| b.id.0 + 1).unwrap_or(1);
    let title = Line::from(vec![
        Span::styled(format!(" %{next_id} "), title_style),
        Span::raw("  new "),
    ]);
    let border_style = if active {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let widget = TuiBlock::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    let inner = widget.inner(area);
    f.render_widget(widget, area);

    let body = if active {
        let mut spans = vec![Span::styled(
            "› ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )];
        spans.extend(input_spans_with_cursor(
            &app.prompt,
            app.prompt_cursor,
            Color::Green,
        ));
        Line::from(spans)
    } else if selected {
        Line::from(Span::styled(
            "  press Enter / Space / e to start typing  (↑ to go back)",
            Style::default().fg(Color::Cyan),
        ))
    } else {
        Line::from(Span::styled(
            "  press / or ↓ to start typing a command",
            Style::default().fg(Color::DarkGray),
        ))
    };
    let para = Paragraph::new(body).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

/// Render one bordered shed. The box title carries the id (`%5`) or
/// pinned name (`@list`) plus the run-state glyph; the body is the
/// caller-supplied content (output, notes, edit details, etc.).
fn draw_one_shed(
    f: &mut Frame,
    area: Rect,
    shed: &shed_core::Shed,
    lines: &[Line<'static>],
    selected: bool,
    editing: bool,
    regions: &mut Vec<ClickRegion>,
) {
    let pinned = shed.name.is_some();
    let id_text = match &shed.name {
        Some(name) => format!(" @{name} "),
        None => format!(" %{} ", shed.id.0),
    };
    let id_style = match (selected, pinned) {
        (true, true) => Style::default()
            .fg(Color::Black)
            .bg(Color::LightMagenta)
            .add_modifier(Modifier::BOLD),
        (true, false) => Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        (false, true) => Style::default()
            .fg(Color::LightMagenta)
            .add_modifier(Modifier::BOLD),
        (false, false) => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    };
    let glyph = match &shed.state {
        ShedState::Idle => Span::styled("○", Style::default().fg(Color::DarkGray)),
        ShedState::Running => Span::styled("⏵", Style::default().fg(Color::Yellow)),
        ShedState::Done(0) => Span::styled("●", Style::default().fg(Color::Green)),
        ShedState::Done(_) => Span::styled("⚠", Style::default().fg(Color::Red)),
        ShedState::Snapshotted => Span::styled("❄", Style::default().fg(Color::LightBlue)),
        ShedState::Failed(_) => Span::styled("⚠", Style::default().fg(Color::Red)),
    };
    let title = Line::from(vec![
        Span::styled(id_text, id_style),
        Span::raw(" "),
        glyph,
        Span::raw(" "),
    ]);

    let border_style = if editing {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let widget = TuiBlock::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    let inner = widget.inner(area);
    f.render_widget(widget, area);
    let para = Paragraph::new(lines.to_vec()).wrap(Wrap { trim: false });
    f.render_widget(para, inner);

    // Clickable `×` on the top-right border. Three cells (` × `) so
    // the hit-target is forgiving; only render when the shed is wide
    // enough that it can't collide with the title (left side).
    let close_width: u16 = 3;
    let min_room: u16 = 6; // corner + 1 padding + title room + close + corner
    if area.width >= min_room {
        let close_x = area.right().saturating_sub(close_width + 1);
        let buf = f.buffer_mut();
        let close_style = Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::BOLD);
        // Repaint the three cells over the top border with `[×]` (or
        // ` × ` for a softer look). Picking `[×]` so it visually
        // reads as a button.
        let _ = buf.set_string(close_x, area.y, "[×]", close_style);
        regions.push(ClickRegion {
            rect: Rect {
                x: close_x,
                y: area.y,
                width: close_width,
                height: 1,
            },
            action: ClickAction::DeleteBlock(shed.id),
        });
    }
}

fn render_shed(
    shed: &Shed,
    selected: bool,
    editing: bool,
    pipeline_cursor: Option<usize>,
    command_focused: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if let Some(text) = shed.pre_text.as_deref() {
        lines.extend(render_note_lines(text));
    }

    // Compute pipeline outcome up-front so we can show inline drop counts.
    let pipeline_outcome = shed
        .capture
        .as_ref()
        .map(|c| apply_pipeline(&c.stdout, &shed.pipeline));
    let drops: Vec<usize> = pipeline_outcome
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|(_, d)| d.clone())
        .unwrap_or_else(|| vec![0; shed.pipeline.len()]);

    // Show the command + each pipeline filter only in EditShed focus.
    // ShedCursor stays compact — output only — so the list is scannable.
    if editing {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                shed.argv.join(" "),
                if command_focused {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Magenta)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                },
            ),
        ]));

        // Each filter on its own line. When command_focused is true the
        // pipeline cursor is treated as None so no filter wears the
        // active-magenta highlight and the `+ add` slot is suppressed.
        let effective_cursor = if command_focused { None } else { pipeline_cursor };
        let highlight = Style::default()
            .fg(Color::Black)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD);
        let normal = Style::default().fg(Color::LightCyan);
        let dim = Style::default().fg(Color::DarkGray);
        let warn = Style::default().fg(Color::Yellow);
        let indent = "   ";

        for (i, f) in shed.pipeline.iter().enumerate() {
            let style = if effective_cursor == Some(i) {
                highlight
            } else {
                normal
            };
            let mut spans = vec![
                Span::raw(indent),
                Span::styled("│ ", dim),
                Span::styled(format!(" {} ", describe_filter(f)), style),
            ];
            let n = drops.get(i).copied().unwrap_or(0);
            if n > 0 {
                spans.push(Span::styled(format!("  ⓘ-{n}"), warn));
            }
            lines.push(Line::from(spans));
        }
        if effective_cursor.is_some() {
            let style = if effective_cursor == Some(shed.pipeline.len()) {
                highlight
            } else {
                dim
            };
            lines.push(Line::from(vec![
                Span::raw(indent),
                Span::styled("│ ", dim),
                Span::styled(" + add ", style),
            ]));
        }
    }

    // Running sheds tail their preview: the most recent rows are
    // visible at the bottom, with "N more" pinned at the top. Finished
    // sheds keep the existing head behaviour.
    let tail = matches!(shed.state, ShedState::Running);
    match pipeline_outcome {
        Some(Ok((value, _))) => lines.extend(render_pipeline_value(value, tail)),
        Some(Err(e)) => lines.extend(filter_error_lines(&e)),
        None => {}
    }

    if let Some(capture) = &shed.capture {
        if capture.truncated {
            lines.push(Line::from(Span::styled(
                " ✂ output truncated",
                Style::default().fg(Color::Magenta),
            )));
        }
        if let Some(code) = capture.exit_code {
            if code != 0 {
                lines.push(Line::from(Span::styled(
                    format!(" exit {code}"),
                    Style::default().fg(Color::Red),
                )));
            }
        }
    } else if let ShedState::Done(code) = &shed.state {
        let mut spans = vec![Span::styled(
            " (no captured output)",
            Style::default().fg(Color::DarkGray),
        )];
        if *code != 0 {
            spans.push(Span::styled(
                format!("  exit {code}"),
                Style::default().fg(Color::Red),
            ));
        }
        lines.push(Line::from(spans));
    }
    if let ShedState::Failed(msg) = &shed.state {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(msg.clone(), Style::default().fg(Color::Red)),
        ]));
    }
    if matches!(shed.state, ShedState::Idle) && selected {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "▸ ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Space",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to run",
                Style::default().fg(Color::Cyan),
            ),
        ]));
    }

    if let Some(text) = shed.post_text.as_deref() {
        lines.extend(render_note_lines(text));
    }

    lines
}

/// Render a note string as a series of dim, italicized lines with a
/// `▎` left edge so they're visually distinct from command output.
fn render_note_lines(text: &str) -> Vec<Line<'static>> {
    let edge = Style::default().fg(Color::DarkGray);
    let body = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::ITALIC);
    text.split('\n')
        .map(|line| {
            Line::from(vec![
                Span::styled("  ▎ ", edge),
                Span::styled(line.to_string(), body),
            ])
        })
        .collect()
}

/// Apply a pipeline of filters. Returns the final value plus per-filter
/// drop counts (rows silently filtered by a `where` due to type mismatch),
/// indexed by filter position in the pipeline.
fn apply_pipeline(
    bytes: &bytes::Bytes,
    pipeline: &[FilterSpec],
) -> Result<(PipelineValue, Vec<usize>), String> {
    let mut value = PipelineValue::Bytes(bytes.clone());
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

fn filter_error_lines(message: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::raw("      "),
        Span::styled(
            format!("filter error: {message}"),
            Style::default().fg(Color::Red),
        ),
    ])]
}

fn render_raw_lines(bytes: &bytes::Bytes, max: usize, tail: bool) -> Vec<Line<'static>> {
    let parsed = ansi::parse_to_lines(bytes, "      ", max, tail);
    let extra = parsed.total.saturating_sub(max);
    let more_line = || {
        Line::from(Span::styled(
            format!("      … {extra} more lines"),
            Style::default().fg(Color::DarkGray),
        ))
    };
    let mut out = Vec::with_capacity(parsed.lines.len() + 1);
    if parsed.truncated && tail {
        out.push(more_line());
    }
    out.extend(parsed.lines);
    if parsed.truncated && !tail {
        out.push(more_line());
    }
    out
}

fn render_pipeline_value(value: PipelineValue, tail: bool) -> Vec<Line<'static>> {
    render_pipeline_value_with_max(value, PREVIEW_LINES, tail)
}

fn render_pipeline_value_with_max(
    value: PipelineValue,
    max: usize,
    tail: bool,
) -> Vec<Line<'static>> {
    match value {
        PipelineValue::Bytes(b) => render_raw_lines(&b, max, tail),
        PipelineValue::Structured(Value::List(items)) => {
            let columns = schema_of(&items);
            if columns.is_empty() {
                render_scalar_list(&items, max, tail)
            } else {
                render_table(&items, &columns, max, tail)
            }
        }
        PipelineValue::Structured(other) => vec![Line::from(vec![
            Span::raw("      "),
            Span::raw(format!("{other:?}")),
        ])],
    }
}

/// Render a list of records as an aligned table with header + separator
/// row + data rows. Column widths are computed across ALL records so the
/// table doesn't jiggle as the user scrolls in the pager. Vertical
/// dividers (`│`) make space-containing cell values unambiguous.
///
/// With `tail = true` the *last* `max_rows` rows are shown and the
/// "N more rows" indicator goes between the separator and the data rows
/// (so the freshest row is always at the bottom — useful for streaming).
fn render_table(
    items: &[Value],
    columns: &[String],
    max_rows: usize,
    tail: bool,
) -> Vec<Line<'static>> {
    let widths = compute_column_widths(items, columns);
    let total = items.len();
    let dim = Style::default().fg(Color::DarkGray);
    let header_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);

    let mut lines = Vec::new();

    // Header row
    let mut header_spans = vec![Span::raw("      ")];
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            header_spans.push(Span::styled(" │ ", dim));
        }
        header_spans.push(Span::styled(pad_right(col, widths[i]), header_style));
    }
    lines.push(Line::from(header_spans));

    // Separator
    let mut sep_spans = vec![Span::raw("      ")];
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            sep_spans.push(Span::styled("─┼─", dim));
        }
        sep_spans.push(Span::styled("─".repeat(*w), dim));
    }
    lines.push(Line::from(sep_spans));

    let truncated = total > max_rows;
    let more_line = || {
        Line::from(Span::styled(
            format!("      … {} more rows", total - max_rows),
            dim,
        ))
    };

    if truncated && tail {
        lines.push(more_line());
    }

    // Data rows: head or tail slice.
    let slice: Box<dyn Iterator<Item = &Value>> = if tail && truncated {
        Box::new(items.iter().skip(total - max_rows))
    } else {
        Box::new(items.iter().take(max_rows))
    };
    for item in slice {
        let mut row_spans = vec![Span::raw("      ")];
        for (i, col) in columns.iter().enumerate() {
            if i > 0 {
                row_spans.push(Span::styled(" │ ", dim));
            }
            let cell = match item {
                Value::Record(r) => r.get(col).map(cell_string).unwrap_or_default(),
                _ => String::new(),
            };
            row_spans.push(Span::raw(pad_right(&cell, widths[i])));
        }
        lines.push(Line::from(row_spans));
    }

    if truncated && !tail {
        lines.push(more_line());
    }
    if total == 0 {
        lines.push(Line::from(Span::styled("      (no rows)", dim)));
    }
    lines
}

fn compute_column_widths(items: &[Value], columns: &[String]) -> Vec<usize> {
    let mut widths: Vec<usize> = columns.iter().map(|c| display_width(c)).collect();
    for item in items {
        if let Value::Record(r) = item {
            for (i, col) in columns.iter().enumerate() {
                if let Some(v) = r.get(col) {
                    let w = display_width(&cell_string(v));
                    if w > widths[i] {
                        widths[i] = w;
                    }
                }
            }
        }
    }
    widths
}

fn render_scalar_list(items: &[Value], max: usize, tail: bool) -> Vec<Line<'static>> {
    let total = items.len();
    let dim = Style::default().fg(Color::DarkGray);
    let truncated = total > max;
    let more_line = || {
        Line::from(Span::styled(
            format!("      … {} more rows", total - max),
            dim,
        ))
    };

    let mut out = Vec::new();
    if truncated && tail {
        out.push(more_line());
    }
    let slice: Box<dyn Iterator<Item = &Value>> = if tail && truncated {
        Box::new(items.iter().skip(total - max))
    } else {
        Box::new(items.iter().take(max))
    };
    for item in slice {
        out.push(Line::from(vec![
            Span::raw("      "),
            Span::raw(format_scalar(item)),
        ]));
    }
    if truncated && !tail {
        out.push(more_line());
    }
    if total == 0 {
        out.push(Line::from(Span::styled("      (no rows)", dim)));
    }
    out
}

fn cell_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        other => format_scalar(other),
    }
}

fn pad_right(s: &str, width: usize) -> String {
    let cur = display_width(s);
    if cur >= width {
        s.to_string()
    } else {
        let mut out = s.to_string();
        for _ in 0..(width - cur) {
            out.push(' ');
        }
        out
    }
}

fn display_width(s: &str) -> usize {
    // ASCII-ish approximation. Wide CJK / emoji would need unicode-width
    // for true cell counting; for shed's typical PTY output (ASCII-heavy)
    // this is fine.
    s.chars().count()
}

fn schema_of(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .find_map(|v| match v {
            Value::Record(r) => Some(r.keys().cloned().collect()),
            _ => None,
        })
        .unwrap_or_default()
}


fn format_scalar(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => s.clone(),
        Value::Bytes(b) => format!("<{} bytes>", b.len()),
        Value::List(_) | Value::Record(_) => format!("{v:?}"),
    }
}

fn describe_filter(spec: &FilterSpec) -> String {
    match spec {
        FilterSpec::FromLines => "from-lines".into(),
        FilterSpec::FromFields => "from-fields".into(),
        FilterSpec::FromCsv { delim, has_header } => format!(
            "from-csv {}{}",
            delim_label(*delim),
            if *has_header { "" } else { " (no header)" }
        ),
        FilterSpec::FromJson => "from-json".into(),
        FilterSpec::FromRegex { pattern } => format!("from-regex /{pattern}/"),
        FilterSpec::Where { predicate } => format!("where {}", describe_predicate(predicate)),
        FilterSpec::Select { columns } => format!("select {}", columns.join(", ")),
        FilterSpec::Drop { columns } => format!("drop {}", columns.join(", ")),
        FilterSpec::Take { n } => format!("take {n}"),
        FilterSpec::Skip { n } => format!("skip {n}"),
        FilterSpec::SortBy { keys } => {
            let parts: Vec<String> = keys
                .iter()
                .map(|k| {
                    let dir = match k.direction {
                        SortDirection::Asc => "↑",
                        SortDirection::Desc => "↓",
                    };
                    format!("{} {dir}", k.column)
                })
                .collect();
            format!("sort-by {}", parts.join(", "))
        }
        FilterSpec::Uniq { by } => match by {
            Some(cols) => format!("uniq by {}", cols.join(", ")),
            None => "uniq".into(),
        },
        FilterSpec::Count => "count".into(),
        FilterSpec::Rename { pairs } => {
            let s = pairs
                .iter()
                .map(|(f, t)| format!("{f}→{t}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("rename {s}")
        }
        FilterSpec::Split { column, delimiter } => {
            format!("split {column} by {delimiter:?}")
        }
        FilterSpec::Join { column, delimiter } => {
            format!("join {column} with {delimiter:?}")
        }
    }
}

fn describe_predicate(p: &Predicate) -> String {
    match p {
        Predicate::Matches { column, pattern } => format!("{column} matches {pattern}"),
        Predicate::Contains { column, substring } => format!("{column} contains {substring}"),
        Predicate::Compare { column, op, value } => {
            format!("{column} {} {}", describe_compare_op(*op), describe_compare_value(value))
        }
        Predicate::And(a, b) => format!("({} && {})", describe_predicate(a), describe_predicate(b)),
        Predicate::Or(a, b) => format!("({} || {})", describe_predicate(a), describe_predicate(b)),
        Predicate::Not(p) => format!("!{}", describe_predicate(p)),
    }
}

fn describe_compare_op(op: CompareOp) -> &'static str {
    match op {
        CompareOp::Eq => "=",
        CompareOp::Ne => "≠",
        CompareOp::Lt => "<",
        CompareOp::Le => "≤",
        CompareOp::Gt => ">",
        CompareOp::Ge => "≥",
    }
}

fn describe_compare_value(v: &Value) -> String {
    match v {
        Value::String(s) => format!("\"{s}\""),
        _ => format_scalar(v),
    }
}

fn draw_preview_pane(
    f: &mut Frame,
    area: Rect,
    outcome: &Option<Result<(PipelineValue, Vec<usize>), String>>,
) {
    let pane = TuiBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled("preview", Style::default().fg(Color::Cyan)),
            Span::raw(" "),
        ]));
    let inner = pane.inner(area);
    f.render_widget(pane, area);

    let max_rows = inner.height.saturating_sub(2) as usize;
    let max = max_rows.max(3);
    let lines = match outcome {
        None => vec![Line::from(Span::styled(
            "      (no capture)",
            Style::default().fg(Color::DarkGray),
        ))],
        Some(Ok((value, _))) => render_pipeline_value_with_max(value.clone(), max, false),
        Some(Err(e)) => filter_error_lines(e),
    };
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

fn draw_stack_pane(
    f: &mut Frame,
    area: Rect,
    shed: &Shed,
    state: &FilterEditState,
    drops: &[usize],
) {
    let shed_widget = TuiBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled("pipeline", Style::default().fg(Color::Cyan)),
            Span::raw(" "),
        ]));
    let inner = shed_widget.inner(area);
    f.render_widget(shed_widget, area);

    let active_label = match state.build_filter() {
        Some(spec) => describe_filter(&spec),
        None => format!("{} (incomplete)", state.kind.name()),
    };
    let edit_style = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let normal = Style::default().fg(Color::LightCyan);

    let warn = Style::default().fg(Color::Yellow);
    let mut lines: Vec<Line> = Vec::new();
    for (i, spec) in shed.pipeline.iter().enumerate() {
        // Insert mode: emit the in-progress row before the existing
        // filter at the insert index. The hypothetical pipeline has the
        // new filter at index i, so drops[i] reports the new filter's
        // type-mismatch count; drops indices for existing filters at or
        // after the insert position shift up by one.
        if state.mode == EditMode::Insert(i) {
            let n = drops.get(i).copied().unwrap_or(0);
            let mut spans = vec![
                Span::styled("  ▸ ", edit_style),
                Span::styled(active_label.clone(), edit_style),
                Span::styled(
                    format!("  ← inserting before {}", circled(i + 1)),
                    dim,
                ),
            ];
            if n > 0 {
                spans.push(Span::styled(
                    format!("  ⓘ -{n} (type mismatch)"),
                    warn,
                ));
            }
            lines.push(Line::from(spans));
        }

        let is_editing_here = state.mode == EditMode::Edit(i);
        let (label, style, suffix) = if is_editing_here {
            (active_label.clone(), edit_style, Some("  ← editing"))
        } else {
            (describe_filter(spec), normal, None)
        };
        let mut spans = vec![
            Span::styled(format!("  {} ", circled(i + 1)), dim),
            Span::styled(label, style),
        ];
        if let Some(s) = suffix {
            spans.push(Span::styled(s, dim));
        }
        // For Insert mode, the existing filter at original index i
        // lives in hypothetical at i+1, so use the shifted drops index.
        let drops_idx = match state.mode {
            EditMode::Insert(j) if i >= j => i + 1,
            _ => i,
        };
        let n = drops.get(drops_idx).copied().unwrap_or(0);
        if n > 0 {
            spans.push(Span::styled(
                format!("  ⓘ -{n} (type mismatch)"),
                warn,
            ));
        }
        lines.push(Line::from(spans));
    }
    if state.mode == EditMode::Add {
        let mut spans = vec![
            Span::styled(
                format!("  {} ", circled(shed.pipeline.len() + 1)),
                edit_style,
            ),
            Span::styled(active_label, edit_style),
            Span::styled("  ← editing", dim),
        ];
        let n = drops.get(shed.pipeline.len()).copied().unwrap_or(0);
        if n > 0 {
            spans.push(Span::styled(
                format!("  ⓘ -{n} (type mismatch)"),
                warn,
            ));
        }
        lines.push(Line::from(spans));
    }

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

fn circled(n: usize) -> String {
    const SYMS: &[&str] = &["①", "②", "③", "④", "⑤", "⑥", "⑦", "⑧", "⑨"];
    SYMS.get(n.saturating_sub(1))
        .copied()
        .unwrap_or("●")
        .to_string()
}

fn draw_form_pane(f: &mut Frame, area: Rect, state: &FilterEditState) {
    let title = state.kind.name().to_string();
    let shed_widget = TuiBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title, Style::default().fg(Color::Magenta)),
            Span::raw(" "),
        ]));
    let inner = shed_widget.inner(area);
    f.render_widget(shed_widget, area);

    let mut lines: Vec<Line> = Vec::new();
    for field in state.fields() {
        match *field {
            FormField::SortKeys => lines.extend(render_sort_keys_field(state)),
            FormField::RenameMap => lines.extend(render_rename_map_field(state)),
            _ => lines.push(render_form_row(state, *field)),
        }
    }
    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

fn render_rename_map_field(state: &FilterEditState) -> Vec<Line<'static>> {
    let active = state.field == FormField::RenameMap;
    let label_first = format!("  {:>8}: ", "rename");
    let label_rest: String = " ".repeat(label_first.chars().count());

    let label_style = if active {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    if state.available_columns.is_empty() {
        return vec![Line::from(vec![
            Span::styled(label_first, label_style),
            Span::styled(
                "(no columns — pipeline output is bytes; add a parser)",
                Style::default().fg(Color::Red),
            ),
        ])];
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, col) in state.available_columns.iter().enumerate() {
        let on_cursor = active && i == state.rename_cursor;
        let label = if i == 0 {
            Span::styled(label_first.clone(), label_style)
        } else {
            Span::raw(label_rest.clone())
        };
        let to = state.rename_to_inputs.get(i).cloned().unwrap_or_default();

        let from_style = if on_cursor {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let arrow_style = Style::default().fg(Color::DarkGray);
        let to_style = if on_cursor {
            Style::default().fg(Color::White)
        } else if to.is_empty() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Magenta)
        };

        let mut spans = vec![
            label,
            Span::styled(format!("{col}"), from_style),
            Span::styled(" → ", arrow_style),
        ];
        if to.is_empty() && !on_cursor {
            spans.push(Span::styled(
                "(unchanged)".to_string(),
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            spans.push(Span::styled(to.clone(), to_style));
        }
        if on_cursor {
            spans.push(Span::styled("▏", Style::default().fg(Color::Magenta)));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn render_sort_keys_field(state: &FilterEditState) -> Vec<Line<'static>> {
    let active = state.field == FormField::SortKeys;
    let label_first = format!("  {:>8}: ", "keys");
    let label_rest: String = " ".repeat(label_first.chars().count());

    let label_style = if active {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    if state.available_columns.is_empty() {
        return vec![Line::from(vec![
            Span::styled(label_first, label_style),
            Span::styled(
                "(no columns — pipeline output is bytes; add a parser)",
                Style::default().fg(Color::Red),
            ),
        ])];
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(Color::White);
    let dim = Style::default().fg(Color::DarkGray);

    for (i, (col_idx, dir)) in state.sort_keys.iter().enumerate() {
        let on_cursor = active && i == state.sort_keys_cursor;
        let label = if i == 0 {
            Span::styled(label_first.clone(), label_style)
        } else {
            Span::raw(label_rest.clone())
        };
        let col = state
            .available_columns
            .get(*col_idx)
            .map(String::as_str)
            .unwrap_or("?");
        let dir_glyph = match dir {
            SortDirection::Asc => "↑",
            SortDirection::Desc => "↓",
        };
        let mut spans = vec![label];
        if on_cursor {
            spans.push(Span::styled("◂ ", Style::default().fg(Color::Magenta)));
        }
        spans.push(Span::styled(
            format!("{col} {dir_glyph}"),
            if on_cursor { highlight } else { normal },
        ));
        if on_cursor {
            spans.push(Span::styled(" ▸", Style::default().fg(Color::Magenta)));
        }
        lines.push(Line::from(spans));
    }

    if state.sort_keys.len() < MAX_SORT_KEYS {
        let on_cursor = active && state.sort_keys_cursor == state.sort_keys.len();
        let label = if state.sort_keys.is_empty() {
            Span::styled(label_first.clone(), label_style)
        } else {
            Span::raw(label_rest.clone())
        };
        let mut spans = vec![label];
        if on_cursor {
            spans.push(Span::styled("◂ ", Style::default().fg(Color::Magenta)));
        }
        spans.push(Span::styled(
            "+ add (a)".to_string(),
            if on_cursor { highlight } else { dim },
        ));
        if on_cursor {
            spans.push(Span::styled(" ▸", Style::default().fg(Color::Magenta)));
        }
        lines.push(Line::from(spans));
    }

    lines
}

fn render_form_row(state: &FilterEditState, field: FormField) -> Line<'static> {
    let active = state.field == field;
    let label = match field {
        FormField::Kind => "filter",
        FormField::Column => "column",
        FormField::Op => "op",
        FormField::Pattern => state.active_op().value_label(),
        FormField::N => "n",
        FormField::Columns => "columns",
        FormField::CsvDelim => "delim",
        FormField::CsvHasHeader => "header",
        FormField::RegexPattern => "regex",
        FormField::SortKeys => "keys",
        FormField::RenameMap => "rename",
        FormField::WhereCombine => "combine",
        FormField::WhereClauseSelect => "clause",
        FormField::TargetColumn => "column",
        FormField::DelimText => "delim",
    };
    let label_style = if active {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let label_span = Span::styled(format!("  {label:>8}: "), label_style);

    let value_spans: Vec<Span<'static>> = match field {
        FormField::Kind => render_select_value(
            state.kind.name(),
            state.kind.description(),
            active,
        ),
        FormField::Column => {
            if state.available_columns.is_empty() {
                vec![Span::styled(
                    "(no columns — pipeline output is bytes; add a parser)",
                    Style::default().fg(Color::Red),
                )]
            } else {
                let col = state.selected_column().unwrap_or("");
                render_select_value(col, "", active)
            }
        }
        FormField::Op => {
            let op = state.active_op();
            render_select_value(op.name(), op.description(), active)
        }
        FormField::Pattern => {
            let op = state.active_op();
            let empty_hint = match op {
                WhereOp::Matches | WhereOp::Contains => "(empty: matches everything)",
                _ => "(unset)",
            };
            render_text_field(state.active_pattern(), active, empty_hint)
        }
        FormField::N => render_text_field(&state.n_input, active, "(unset)"),
        FormField::Columns => render_columns_field(state, active),
        FormField::CsvDelim => render_select_value(delim_label(state.csv_delim), "", active),
        FormField::CsvHasHeader => render_select_value(
            if state.csv_has_header { "true" } else { "false" },
            if state.csv_has_header {
                "first row is column names"
            } else {
                "auto-name columns _1, _2, …"
            },
            active,
        ),
        FormField::RegexPattern => render_text_field(
            &state.regex_pattern,
            active,
            "(empty: matches nothing — use named groups like (?<col>…))",
        ),
        FormField::SortKeys | FormField::RenameMap => {
            // Multi-line — handled separately.
            return Line::from("");
        }
        FormField::WhereCombine => {
            render_select_value(state.where_combine.name(), state.where_combine.description(), active)
        }
        FormField::WhereClauseSelect => {
            let total = state.where_clauses.len();
            let label = format!(
                "{} of {}",
                circled(state.where_active_clause + 1),
                total,
            );
            let desc = if active {
                "←→ select  a add  x remove"
            } else {
                ""
            };
            render_select_value(&label, desc, active)
        }
        FormField::TargetColumn => {
            if state.available_columns.is_empty() {
                vec![Span::styled(
                    "(no columns — pipeline output is bytes; add a parser)",
                    Style::default().fg(Color::Red),
                )]
            } else {
                let col = state
                    .available_columns
                    .get(state.target_column)
                    .map(String::as_str)
                    .unwrap_or("");
                render_select_value(col, "", active)
            }
        }
        FormField::DelimText => render_text_field(
            &state.delim_text,
            active,
            "(empty: no split / no separator)",
        ),
    };

    let mut all = vec![label_span];
    all.extend(value_spans);
    Line::from(all)
}

fn render_text_field(value: &str, active: bool, empty_hint: &'static str) -> Vec<Span<'static>> {
    let cursor_marker = if active { "▏" } else { "" };
    let value_style = if active {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::Gray)
    };
    let mut spans = Vec::new();
    spans.push(Span::styled(value.to_string(), value_style));
    spans.push(Span::styled(
        cursor_marker,
        Style::default().fg(Color::Magenta),
    ));
    if value.is_empty() && !active {
        spans.push(Span::styled(
            empty_hint,
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans
}

fn render_columns_field(state: &FilterEditState, active: bool) -> Vec<Span<'static>> {
    if state.available_columns.is_empty() {
        return vec![Span::styled(
            "(no columns — pipeline output is bytes; add a parser)",
            Style::default().fg(Color::Red),
        )];
    }
    let mut spans = Vec::new();
    for (i, col) in state.available_columns.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let check = if state.column_selections[i] { "☑" } else { "☐" };
        let on_cursor = active && i == state.column_cursor;
        let style = if on_cursor {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        } else if state.column_selections[i] {
            Style::default().fg(Color::Magenta)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {check} {col} "), style));
    }
    spans
}

fn render_select_value(value: &str, description: &str, active: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if active {
        spans.push(Span::styled("◂ ", Style::default().fg(Color::Magenta)));
    }
    spans.push(Span::styled(
        value.to_string(),
        if active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        },
    ));
    if active {
        spans.push(Span::styled(" ▸", Style::default().fg(Color::Magenta)));
    }
    if !description.is_empty() {
        spans.push(Span::styled(
            format!("   {description}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    if let Some(prompt) = app.exit_prompt {
        if prompt == ExitPrompt::Confirm {
            let widget = Paragraph::new(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "unsaved changes — save before quitting?",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    "[y]es",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    "[n]o",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    "[c]ancel",
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]))
            .style(Style::default().bg(Color::DarkGray));
            f.render_widget(widget, area);
            return;
        }
        // AwaitingPath falls through; the save_input_mode bar takes over.
    }
    if app.save_input_mode {
        f.render_widget(
            render_input_bar("save to: ", Color::Green, &app.save_input, app.save_cursor),
            area,
        );
        return;
    }
    if app.open_input_mode {
        f.render_widget(
            render_input_bar("open: ", Color::Green, &app.open_input, app.open_cursor),
            area,
        );
        return;
    }
    if app.rerun_input_mode {
        f.render_widget(
            render_input_bar(
                "rerun: ",
                Color::LightCyan,
                &app.rerun_input,
                app.rerun_cursor,
            ),
            area,
        );
        return;
    }
    if app.cmd_edit_input_mode {
        f.render_widget(
            render_input_bar(
                "edit cmd: ",
                Color::LightMagenta,
                &app.cmd_edit_input,
                app.cmd_edit_cursor,
            ),
            area,
        );
        return;
    }
    if let Some(pending) = &app.alias_overwrite {
        let widget = Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!("alias `{}` exists — overwrite?", pending.name),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "[y]es",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "[n]o",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]))
        .style(Style::default().bg(Color::DarkGray));
        f.render_widget(widget, area);
        return;
    }
    if app.alias_name_input_mode {
        f.render_widget(
            render_input_bar(
                "alias name: ",
                Color::LightMagenta,
                &app.alias_name_input,
                app.alias_name_cursor,
            ),
            area,
        );
        return;
    }
    if app.pin_input_mode {
        f.render_widget(
            render_input_bar(
                "pin name: ",
                Color::LightMagenta,
                &app.pin_input,
                app.pin_cursor,
            ),
            area,
        );
        return;
    }
    if app.write_input_mode {
        f.render_widget(
            render_input_bar(
                "write to: ",
                Color::Yellow,
                &app.write_input,
                app.write_cursor,
            ),
            area,
        );
        return;
    }
    if app.search_input_mode {
        let invalid = !app.search_input.is_empty()
            && try_compile(&app.search_input, app.search_case_insensitive).is_none();
        let prefix = if app.search_input_backward { "?" } else { "/" };
        let mut spans = vec![
            Span::raw(" ".to_string()),
            Span::styled(
                prefix.to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        spans.extend(input_spans_with_cursor(
            &app.search_input,
            app.search_cursor,
            Color::Yellow,
        ));
        if app.search_case_insensitive {
            spans.push(Span::styled(
                "  (i)",
                Style::default().fg(Color::DarkGray),
            ));
        }
        if invalid {
            spans.push(Span::styled(
                "  (invalid regex)",
                Style::default().fg(Color::Red),
            ));
        }
        let widget = Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::DarkGray));
        f.render_widget(widget, area);
        return;
    }
    if let Some(msg) = &app.flash {
        let widget = Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(msg.clone(), Style::default().fg(Color::Yellow)),
        ]))
        .style(Style::default().bg(Color::DarkGray));
        f.render_widget(widget, area);
        return;
    }
    let hints: Vec<(&str, &str)> = match app.focus {
        Focus::Prompt => vec![
            ("Enter", "run"),
            ("↑↓", "history"),
            ("!cmd", "fullscreen"),
            ("@name", "snapshot"),
            ("/aliases", "manage"),
            ("Ctrl-A/E/U/K/W", "line edit"),
            ("Ctrl-P", "palette"),
            ("Ctrl-S/O", "save/open"),
            ("Ctrl-Z/Y", "undo/redo"),
            ("Esc", "focus shed"),
            ("Ctrl-D", "quit"),
        ],
        Focus::ShedCursor => vec![
            ("↑↓", "sheds"),
            ("e", "edit"),
            ("v", "view"),
            ("Space", "run"),
            ("x", "delete"),
            ("w", "write"),
            ("p/u", "pin/unpin"),
            ("r", "rerun"),
            ("A", "save alias"),
            ("n/N", "pre/post note"),
            ("/", "prompt"),
            ("Ctrl-S/O", "save/open"),
            ("Ctrl-Z/Y", "undo/redo"),
            ("Ctrl-C", "cancel"),
            ("Esc", "prompt"),
            ("Ctrl-D", "quit"),
        ],
        Focus::EditShed if app.command_focused => vec![
            ("↓", "filters"),
            ("f / Enter", "edit cmd"),
            ("Esc", "back"),
        ],
        Focus::EditShed => vec![
            ("↑↓", "cmd / filters"),
            ("f / Enter", "edit"),
            ("i", "insert"),
            ("d", "drop"),
            ("<>", "reorder"),
            ("Esc", "back"),
        ],
        Focus::FilterEdit => {
            let field = app
                .filter_edit
                .as_ref()
                .map(|s| s.field)
                .unwrap_or(FormField::Kind);
            filter_edit_field_hints(field)
        }
        Focus::ShedExpand => vec![
            ("↑↓ / jk", "scroll"),
            ("PgUp/Dn", "page"),
            ("g/G", "top/bot"),
            ("/?", "search f/b"),
            ("n/N", "next/prev"),
            ("i", "case"),
            ("Esc / q", "back"),
            ("Ctrl-D", "quit"),
        ],
        Focus::EnvEdit => vec![
            ("↑↓", "nav"),
            ("/", "filter"),
            ("e/Enter", "edit"),
            ("a", "add"),
            ("d", "delete"),
            ("Esc / q", "back"),
            ("Ctrl-D", "quit"),
        ],
        Focus::Palette => vec![
            ("↑↓", "navigate"),
            ("Enter", "run"),
            ("Esc", "cancel"),
            ("Ctrl-D", "quit"),
        ],
        Focus::NoteEdit => vec![
            ("type", "edit"),
            ("Enter", "newline"),
            ("←→ ↑↓", "move"),
            ("Home/End", "line ends"),
            ("Backspace", "delete"),
            ("Ctrl-S", "save"),
            ("Esc / Ctrl-C", "cancel"),
        ],
        Focus::AliasManage => vec![
            ("↑↓", "navigate"),
            ("x / d", "delete"),
            ("Esc / q", "back"),
            ("Ctrl-D", "quit"),
        ],
    };
    let mut spans = Vec::new();
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!(" {key} "),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ));
        spans.push(Span::raw(format!(" {label}")));
    }
    let widget = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(Color::DarkGray));
    f.render_widget(widget, area);
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
            }),
            pipeline: Vec::new(),
            state: ShedState::Done(0),
            last_touched: Instant::now(),
            pre_text: None,
            post_text: None,
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
        let texts: Vec<String> = highlighted.spans.iter().map(|s| s.content.to_string()).collect();
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
        assert!(!result.spans[0].style.add_modifier.contains(Modifier::REVERSED));
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
        assert!(matches!(WriteFormat::from_path("foo.csv"), WriteFormat::Csv(b',')));
        assert!(matches!(WriteFormat::from_path("foo.CSV"), WriteFormat::Csv(b',')));
        assert!(matches!(WriteFormat::from_path("foo.tsv"), WriteFormat::Csv(b'\t')));
        assert!(matches!(WriteFormat::from_path("foo.json"), WriteFormat::Json));
        assert!(matches!(WriteFormat::from_path("foo.txt"), WriteFormat::Plain));
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
        let v = PipelineValue::Structured(Value::List(vec![
            Value::Record(rec1),
            Value::Record(rec2),
        ]));
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
        assert_eq!(
            expand_tilde("~"),
            PathBuf::from("/tmp/shed-tilde-test")
        );
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
            Some(FilterSpec::Where { predicate: Predicate::Matches { pattern, .. } }) => {
                assert_eq!(pattern, "abc");
            }
            _ => panic!("expected Matches"),
        }

        state.where_clauses[0].op = WhereOp::Contains;
        match state.build_filter() {
            Some(FilterSpec::Where { predicate: Predicate::Contains { substring, .. } }) => {
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
        assert_eq!(parse_value_input("3.14"), Value::Float(3.14));
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
        app.session.shed_mut(id).unwrap().pipeline.push(FilterSpec::FromLines);
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
    fn is_pinned_ref_only_for_single_at_token() {
        assert!(is_pinned_ref(&["@logs".into()]));
        assert!(!is_pinned_ref(&["@logs".into(), "extra".into()]));
        assert!(!is_pinned_ref(&["@".into()]));
        assert!(!is_pinned_ref(&["logs".into()]));
        assert!(!is_pinned_ref(&[]));
    }

    #[test]
    fn snapshot_pinned_renders_structured_as_json() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["seq".into(), "1".into(), "3".into()]);
        s.set_capture(src, Capture {
            stdout: Bytes::from_static(b"1\n2\n3\n"),
            stderr: Bytes::new(),
            exit_code: Some(0),
            started_at: Instant::now(),
            finished_at: Some(Instant::now()),
            truncated: false,
            snapshotted: false,
        });
        s.shed_mut(src).unwrap().pipeline.push(FilterSpec::FromLines);
        s.pin(src, "nums".into());

        let bytes = snapshot_pinned(&s, "nums").expect("snapshot");
        let text = String::from_utf8(bytes).unwrap();
        // from-lines yields a list of records {line: ...}. Pretty JSON.
        assert!(text.contains("\"line\""));
        assert!(text.contains("\"1\""));
        assert!(text.contains("\"2\""));
        assert!(text.contains("\"3\""));
    }

    #[test]
    fn snapshot_pinned_passes_raw_bytes_through_when_unparsed() {
        let mut s = Session::new();
        let src = s.add_shed(vec!["echo".into(), "hi".into()]);
        s.set_capture(src, Capture {
            stdout: Bytes::from_static(b"hello\n"),
            stderr: Bytes::new(),
            exit_code: Some(0),
            started_at: Instant::now(),
            finished_at: Some(Instant::now()),
            truncated: false,
            snapshotted: false,
        });
        s.pin(src, "h".into());

        let bytes = snapshot_pinned(&s, "h").expect("snapshot");
        assert_eq!(bytes, b"hello\n");
    }

    #[test]
    fn snapshot_pinned_errors_on_unknown_name() {
        let s = Session::new();
        let err = snapshot_pinned(&s, "nope").unwrap_err();
        assert!(err.contains("nope"));
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
        let id = app.session.add_shed(vec!["seq".into(), "1".into(), "3".into()]);
        app.session.set_capture(id, Capture {
            stdout: Bytes::from_static(b"1\n2\n3\n"),
            stderr: Bytes::new(),
            exit_code: Some(0),
            started_at: Instant::now(),
            finished_at: Some(Instant::now()),
            truncated: false,
            snapshotted: false,
        });
        app.session.set_cursor(Some(id));

        // Simulate adding a filter via savepoint + mutation.
        app.savepoint();
        app.session.shed_mut(id).unwrap().pipeline.push(FilterSpec::FromLines);

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
        app.session.shed_mut(id).unwrap().pipeline.push(FilterSpec::FromLines);
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
        let src = app.session.add_shed(vec!["seq".into(), "1".into(), "3".into()]);
        app.session.set_state(src, ShedState::Done(0));
        app.session.pin(src, "src".into());

        let dep = app.session.add_shed(vec!["@src".into()]);
        app.session.set_state(dep, ShedState::Done(0));

        app.session.set_cursor(Some(src));
        app.cmd_edit_input = "seq 1 5".into();
        app.cmd_edit_input_mode = true;
        commit_cmd_edit(&mut app);

        assert!(!app.cmd_edit_input_mode);
        assert_eq!(
            app.session.shed(src).unwrap().argv,
            vec!["seq", "1", "5"]
        );
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
        app.cmd_edit_input = r#"echo "unclosed"#.into();
        app.cmd_edit_input_mode = true;
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
        assert_eq!(validate_alias_name("name_with_underscore").unwrap(), "name_with_underscore");
    }

    #[test]
    fn build_alias_from_cursor_copies_argv_and_pipeline() {
        let mut app = App::new();
        app.history.clear();
        let id = app.session.add_shed(vec!["ls".into(), "-lat".into()]);
        app.session.shed_mut(id).unwrap().pipeline.push(FilterSpec::FromFields);
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
        app.alias_name_input = "list".into();
        app.alias_name_input_mode = true;

        commit_alias_save(&mut app);
        assert!(!app.alias_name_input_mode);
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
        app.alias_name_input = "list".into();
        app.alias_name_input_mode = true;

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
        assert!(app.cmd_edit_input_mode);
        // Pre-fill ends with a trailing space for easy arg appending.
        assert!(app.cmd_edit_input.ends_with(' '));
        assert!(app.cmd_edit_input.starts_with("ls"));
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

    // === tab completion ===

    #[test]
    fn split_last_token_handles_empty_and_whitespace() {
        assert_eq!(split_last_token(""), ("", ""));
        assert_eq!(split_last_token("git"), ("", "git"));
        assert_eq!(split_last_token("git "), ("git ", ""));
        assert_eq!(split_last_token("git ch"), ("git ", "ch"));
        assert_eq!(split_last_token("a b c"), ("a b ", "c"));
        assert_eq!(split_last_token("  ls"), ("  ", "ls"));
    }

    #[test]
    fn classify_completion_picks_each_context() {
        // env var anywhere
        assert_eq!(
            classify_completion(Focus::Prompt, "echo ", "$HO"),
            CompletionContext::EnvVar
        );
        assert_eq!(
            classify_completion(Focus::Prompt, "", "$HO"),
            CompletionContext::EnvVar
        );
        // pinned anywhere a token starts with @
        assert_eq!(
            classify_completion(Focus::Prompt, "", "@lo"),
            CompletionContext::Pinned
        );
        assert_eq!(
            classify_completion(Focus::Prompt, "cat ", "@log"),
            CompletionContext::Pinned
        );
        // slash only at argv0 in Prompt
        assert_eq!(
            classify_completion(Focus::Prompt, "", "/al"),
            CompletionContext::Slash
        );
        // / outside Prompt at argv0 → path (cmd-edit can be /usr/bin/foo)
        assert_eq!(
            classify_completion(Focus::EditShed, "", "/usr"),
            CompletionContext::Path
        );
        // ./ at argv0 → path
        assert_eq!(
            classify_completion(Focus::Prompt, "", "./bu"),
            CompletionContext::Path
        );
        // bare argv0
        assert_eq!(
            classify_completion(Focus::Prompt, "", "gi"),
            CompletionContext::Argv0
        );
        // argv1+
        assert_eq!(
            classify_completion(Focus::Prompt, "git ", "ch"),
            CompletionContext::Path
        );
    }

    #[test]
    fn pinned_completions_filters_by_prefix() {
        let mut s = Session::new();
        let a = s.add_shed(vec!["a".into()]);
        let b = s.add_shed(vec!["b".into()]);
        let c = s.add_shed(vec!["c".into()]);
        s.pin(a, "logs".into());
        s.pin(b, "long".into());
        s.pin(c, "other".into());
        let got = pinned_completions(&s, "@lo");
        assert_eq!(got, vec!["@logs".to_string(), "@long".to_string()]);
        let none = pinned_completions(&s, "@zzz");
        assert!(none.is_empty());
    }

    #[test]
    fn slash_completions_returns_known_commands() {
        let got = slash_completions("/al");
        assert_eq!(got, vec!["/aliases".to_string()]);
        let got = slash_completions("/x");
        assert!(got.is_empty());
    }

    #[test]
    fn argv0_completions_includes_builtins_and_aliases() {
        let mut aliases = AliasFile::default();
        aliases.upsert(Alias {
            name: "exalt".into(),
            argv: vec!["echo".into()],
            pipeline: vec![],
        });
        // Use a prefix that overlaps a builtin and the alias.
        let got = argv0_completions(&aliases, "ex");
        assert!(got.iter().any(|s| s == "exit"), "got={got:?}");
        assert!(got.iter().any(|s| s == "exalt"), "got={got:?}");
        assert!(got.iter().any(|s| s == "export"), "got={got:?}");
    }

    #[test]
    fn env_completions_pulls_from_environment() {
        // SAFETY: test-only mutation; tests run on a single thread by
        // default in cargo, but env mutation is racy if other tests
        // read $TAB_COMPLETION_TEST_VAR concurrently. The name is
        // unique enough that we accept the risk.
        unsafe {
            std::env::set_var("TAB_COMPLETION_TEST_VAR", "1");
        }
        let got = env_completions("$TAB_COMPLETION_TEST");
        assert!(
            got.iter().any(|s| s == "$TAB_COMPLETION_TEST_VAR"),
            "got={got:?}"
        );
        unsafe {
            std::env::remove_var("TAB_COMPLETION_TEST_VAR");
        }
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
        assert!(first == "cat @alpha" || first == "cat @alphabet", "first={first}");
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

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn tf_insert_inserts_at_cursor_and_advances() {
        let mut text = "ad".to_string();
        let mut cursor = 1;
        tf_insert_char(&mut text, &mut cursor, 'b');
        assert_eq!(text, "abd");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn tf_backspace_removes_char_before_cursor() {
        let mut text = "abc".to_string();
        let mut cursor = 2;
        tf_backspace(&mut text, &mut cursor);
        assert_eq!(text, "ac");
        assert_eq!(cursor, 1);
        // No-op at start.
        cursor = 0;
        tf_backspace(&mut text, &mut cursor);
        assert_eq!(text, "ac");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn tf_delete_removes_char_after_cursor() {
        let mut text = "abc".to_string();
        let mut cursor = 1;
        tf_delete(&mut text, &mut cursor);
        assert_eq!(text, "ac");
        assert_eq!(cursor, 1);
        // No-op at end.
        cursor = text.len();
        tf_delete(&mut text, &mut cursor);
        assert_eq!(text, "ac");
    }

    #[test]
    fn tf_kill_to_beginning_and_end() {
        let mut text = "hello world".to_string();
        let mut cursor = 6;
        tf_kill_to_beginning(&mut text, &mut cursor);
        assert_eq!(text, "world");
        assert_eq!(cursor, 0);

        let mut text = "hello world".to_string();
        let mut cursor = 5;
        tf_kill_to_end(&mut text, &mut cursor);
        assert_eq!(text, "hello");
        assert_eq!(cursor, 5);
    }

    #[test]
    fn tf_kill_word_back_eats_one_word_and_trailing_ws() {
        let mut text = "git checkout main".to_string();
        let mut cursor = text.len();
        tf_kill_word_back(&mut text, &mut cursor);
        assert_eq!(text, "git checkout ");
        assert_eq!(cursor, "git checkout ".len());
        tf_kill_word_back(&mut text, &mut cursor);
        assert_eq!(text, "git ");
    }

    #[test]
    fn tf_word_left_right_jump_by_word() {
        let text = "one two  three";
        let mut cursor = text.len();
        tf_word_left(text, &mut cursor);
        assert_eq!(&text[cursor..], "three");
        tf_word_left(text, &mut cursor);
        assert_eq!(&text[cursor..], "two  three");

        let mut cursor = 0;
        tf_word_right(text, &mut cursor);
        assert_eq!(&text[..cursor], "one");
        tf_word_right(text, &mut cursor);
        assert_eq!(&text[..cursor], "one two");
    }

    #[test]
    fn apply_readline_handles_basic_keys() {
        let mut t = String::from("ab");
        let mut c = 2;
        assert!(apply_readline_edit(&mut t, &mut c, &key(KeyCode::Char('c'))));
        assert_eq!(t, "abc");
        assert_eq!(c, 3);

        assert!(apply_readline_edit(&mut t, &mut c, &ctrl('a')));
        assert_eq!(c, 0);
        assert!(apply_readline_edit(&mut t, &mut c, &ctrl('e')));
        assert_eq!(c, 3);
        assert!(apply_readline_edit(&mut t, &mut c, &ctrl('u')));
        assert_eq!(t, "");
        assert_eq!(c, 0);

        // Returns false for keys it doesn't own.
        assert!(!apply_readline_edit(&mut t, &mut c, &key(KeyCode::Enter)));
        assert!(!apply_readline_edit(&mut t, &mut c, &key(KeyCode::Esc)));
        assert!(!apply_readline_edit(&mut t, &mut c, &key(KeyCode::Tab)));
    }

    #[test]
    fn apply_readline_alt_word_movement() {
        let mut t = String::from("one two three");
        let mut c = t.len();
        assert!(apply_readline_edit(&mut t, &mut c, &alt('b')));
        assert_eq!(&t[c..], "three");
        assert!(apply_readline_edit(
            &mut t,
            &mut c,
            &KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
        ));
        assert_eq!(&t[c..], "two three");
    }

    #[test]
    fn input_outcome_routes_enter_esc_and_edits() {
        let mut t = String::from("ab");
        let mut c = 2;
        assert_eq!(
            handle_text_input(&mut t, &mut c, &key(KeyCode::Enter)),
            InputOutcome::Commit
        );
        assert_eq!(
            handle_text_input(&mut t, &mut c, &key(KeyCode::Esc)),
            InputOutcome::Cancel
        );
        assert_eq!(
            handle_text_input(&mut t, &mut c, &key(KeyCode::Char('c'))),
            InputOutcome::Continue
        );
        assert_eq!(t, "abc");
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
        handle_cursor_key(&mut app, KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
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
        handle_cursor_key(&mut app, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(app.focus, Focus::Prompt);
    }

    // === carapace integration ===

    #[test]
    fn carapace_export_parser_extracts_values_matching_prefix() {
        let json = br#"{
            "version": "unknown",
            "messages": [],
            "values": [
                {"value": "main", "display": "main", "description": "branch"},
                {"value": "master", "display": "master"},
                {"value": "develop", "display": "develop"}
            ]
        }"#;
        let got = carapace_completions_from_export(json, "ma");
        assert_eq!(got, vec!["main".to_string(), "master".to_string()]);
    }

    #[test]
    fn carapace_export_parser_empty_token_returns_all_values_sorted() {
        let json = br#"{
            "values": [
                {"value": "zeta"},
                {"value": "alpha"},
                {"value": "alpha"}
            ]
        }"#;
        let got = carapace_completions_from_export(json, "");
        assert_eq!(got, vec!["alpha".to_string(), "zeta".to_string()]);
    }

    #[test]
    fn carapace_export_parser_missing_values_returns_empty() {
        let got = carapace_completions_from_export(b"{\"version\":\"x\"}", "");
        assert!(got.is_empty());
    }

    #[test]
    fn carapace_export_parser_handles_invalid_json() {
        let got = carapace_completions_from_export(b"not json at all", "");
        assert!(got.is_empty());
    }

    #[test]
    fn carapace_export_parser_ignores_records_without_value_field() {
        let json = br#"{"values": [{"display": "x"}, {"value": "real"}]}"#;
        let got = carapace_completions_from_export(json, "");
        assert_eq!(got, vec!["real".to_string()]);
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
            rect: Rect { x: 10, y: 5, width: 3, height: 1 },
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
            rect: Rect { x: 10, y: 5, width: 3, height: 1 },
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
            rect: Rect { x: 10, y: 5, width: 3, height: 1 },
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

    #[test]
    fn render_table_tail_puts_more_indicator_at_top_and_shows_last_rows() {
        use shed_core::Value;
        let cols = vec!["n".to_string()];
        let items: Vec<Value> = (1..=10)
            .map(|i| {
                let mut r = indexmap::IndexMap::new();
                r.insert("n".to_string(), Value::Int(i));
                Value::Record(r)
            })
            .collect();
        let lines = render_table(&items, &cols, 3, true);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // Header, separator, "… N more rows", then last 3 records (8, 9, 10).
        assert!(texts[0].contains("n"));
        assert!(texts[1].contains("─"));
        assert!(texts[2].contains("… 7 more rows"), "got: {:?}", texts[2]);
        assert!(texts[3].contains("8"));
        assert!(texts[4].contains("9"));
        assert!(texts[5].contains("10"));
    }

    #[test]
    fn render_scalar_list_tail_puts_more_at_top_and_shows_last_items() {
        use shed_core::Value;
        let items: Vec<Value> = (1..=10).map(Value::Int).collect();
        let lines = render_scalar_list(&items, 3, true);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts[0].contains("… 7 more rows"));
        assert!(texts[1].trim().ends_with("8"));
        assert!(texts[2].trim().ends_with("9"));
        assert!(texts[3].trim().ends_with("10"));
    }

    #[test]
    fn render_raw_lines_tail_puts_more_at_top_and_shows_last_lines() {
        let bytes = bytes::Bytes::from_static(b"a\nb\nc\nd\ne\nf\n");
        let lines = render_raw_lines(&bytes, 3, true);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts[0].contains("… 3 more lines"));
        assert!(texts[1].trim().ends_with("d"));
        assert!(texts[2].trim().ends_with("e"));
        assert!(texts[3].trim().ends_with("f"));
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
}
