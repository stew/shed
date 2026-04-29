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
//! | `BlockCursor` | navigate blocks (↑↓), filters within (←→), edit |
//! | `FilterEdit`  | schema-aware form for the active filter          |
//! | `BlockExpand` | fullscreen pager for the selected block          |
//!
//! Focus transitions: `Esc` from `Prompt` enters `BlockCursor` on the
//! newest block; `Esc` from `BlockCursor` returns to `Prompt`; `f`/Enter
//! on a block enters `FilterEdit`; `Esc` from `FilterEdit` cancels.
//!
//! # Concurrency
//!
//! Commands run as tokio `spawn_blocking` tasks (`portable-pty`'s API is
//! sync). The event loop polls completed tasks via
//! [`reap_completed`] each iteration and updates the corresponding block.
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

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as TuiBlock, Borders, Paragraph, Wrap},
};
use shed_core::{
    Block, BlockId, BlockState, CompareOp, Filter, FilterSpec, PipelineValue, Predicate,
    Session, SortDirection, SortKey, Value, apply_with_notes,
};
use tokio::task::JoinHandle;

use crate::ansi;
use crate::exec::{self, CaptureOutcome, ExecError, Killer};

type CommandTask = JoinHandle<Result<CaptureOutcome, ExecError>>;

struct RunningCommand {
    handle: CommandTask,
    killer: Killer,
}

#[derive(Debug, Clone)]
struct HandoverRequest {
    argv: Vec<String>,
    /// If `Some`, reuse this block's id (for auto-handover after
    /// alt-screen detection); if `None`, allocate a new one (for
    /// user-initiated handover via blacklist or `!` prefix).
    reuse_block: Option<BlockId>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Prompt,
    BlockCursor,
    FilterEdit,
    BlockExpand,
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
    block_id: BlockId,
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
    fn empty(block_id: BlockId, available_columns: Vec<String>, mode: EditMode) -> Self {
        let column_selections = vec![false; available_columns.len()];
        Self {
            block_id,
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

    fn for_add(block: &Block) -> Self {
        let available_columns = compute_schema_at(block, block.pipeline.len());
        let mut state = Self::empty(block.id, available_columns, EditMode::Add);
        state.kind = if state.available_columns.is_empty() {
            FilterKind::FromLines
        } else {
            FilterKind::Where
        };
        state
    }

    fn for_insert(block: &Block, index: usize) -> Self {
        let available_columns = compute_schema_at(block, index);
        let mut state = Self::empty(block.id, available_columns, EditMode::Insert(index));
        state.kind = if state.available_columns.is_empty() {
            FilterKind::FromLines
        } else {
            FilterKind::Where
        };
        state
    }

    fn for_edit(block: &Block, index: usize) -> Self {
        let available_columns = compute_schema_at(block, index);
        let mut state = Self::empty(block.id, available_columns, EditMode::Edit(index));
        match block.pipeline.get(index) {
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

fn compute_schema_at(block: &Block, before_index: usize) -> Vec<String> {
    let Some(capture) = &block.capture else {
        return Vec::new();
    };
    let mut value = PipelineValue::Bytes(capture.stdout.clone());
    for filter in block.pipeline.iter().take(before_index) {
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
    history: Vec<String>,
    history_cursor: Option<usize>,
    write_input_mode: bool,
    write_input: String,
    last_cwd: Option<PathBuf>,
    focus: Focus,
    filter_edit: Option<FilterEditState>,
    pipeline_cursor: usize,
    expand_scroll: usize,
    search_query: String,
    search_input: String,
    search_input_mode: bool,
    search_anchor_scroll: usize,
    search_input_backward: bool,
    search_case_insensitive: bool,
    flash: Option<String>,
    quit: bool,
    running: HashMap<BlockId, RunningCommand>,
    pending_handover: Option<HandoverRequest>,
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
            history: load_history_from_default_path().unwrap_or_default(),
            history_cursor: None,
            write_input_mode: false,
            write_input: String::new(),
            last_cwd: None,
            focus: Focus::Prompt,
            filter_edit: None,
            pipeline_cursor: 0,
            expand_scroll: 0,
            search_query: String::new(),
            search_input: String::new(),
            search_input_mode: false,
            search_anchor_scroll: 0,
            search_input_backward: false,
            search_case_insensitive: false,
            flash: None,
            quit: false,
            running: HashMap::new(),
            pending_handover: None,
        }
    }

    fn newest_block_id(&self) -> Option<BlockId> {
        self.session.blocks().last().map(|b| b.id)
    }

    fn block_ids_in_order(&self) -> Vec<BlockId> {
        self.session.blocks().map(|b| b.id).collect()
    }

    fn move_cursor(&mut self, delta: i32) {
        let ids = self.block_ids_in_order();
        let Some(cur) = self.session.cursor() else {
            return;
        };
        if let Some(i) = ids.iter().position(|id| *id == cur) {
            let new_i = (i as i32 + delta).clamp(0, ids.len() as i32 - 1) as usize;
            self.session.set_cursor(Some(ids[new_i]));
            self.reset_pipeline_cursor();
        }
    }

    fn reset_pipeline_cursor(&mut self) {
        self.pipeline_cursor = self
            .session
            .cursor()
            .and_then(|id| self.session.block(id))
            .map(|b| b.pipeline.len())
            .unwrap_or(0);
    }

    fn cursor_block_pipeline_len(&self) -> Option<usize> {
        self.session
            .cursor()
            .and_then(|id| self.session.block(id))
            .map(|b| b.pipeline.len())
    }
}

pub async fn run() -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = app_loop(&mut terminal).await;
    ratatui::restore();
    result
}

async fn app_loop(terminal: &mut DefaultTerminal) -> io::Result<()> {
    let mut app = App::new();
    loop {
        if let Some(req) = app.pending_handover.take() {
            perform_handover(terminal, &mut app, req).await?;
        }
        reap_completed(&mut app).await;
        terminal.draw(|f| draw(f, &app))?;
        if app.quit {
            return Ok(());
        }
        if event::poll(POLL_TIMEOUT)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.flash = None;
                    handle_key(&mut app, key).await;
                }
            }
        }
    }
}

async fn perform_handover(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    req: HandoverRequest,
) -> io::Result<()> {
    ratatui::restore();

    let id = match req.reuse_block {
        Some(existing) => existing,
        None => app.session.add_block(req.argv.clone()),
    };
    let status_result = tokio::process::Command::new(&req.argv[0])
        .args(&req.argv[1..])
        .status()
        .await;

    *terminal = ratatui::init();

    match status_result {
        Ok(status) => {
            app.session
                .set_state(id, BlockState::Done(status.code().unwrap_or(-1)));
        }
        Err(e) => {
            app.session
                .set_state(id, BlockState::Failed(format!("spawn: {e}")));
        }
    }
    if req.reuse_block.is_some() {
        app.flash = Some(format!("%{} switched to fullscreen mode", id.0));
    }
    Ok(())
}

async fn reap_completed(app: &mut App) {
    let finished_ids: Vec<BlockId> = app
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
                app.session.set_state(id, BlockState::Done(exit));
            }
            Ok(Ok(CaptureOutcome::NeededFullscreen)) => {
                let argv = app
                    .session
                    .block(id)
                    .map(|b| b.argv.clone())
                    .unwrap_or_default();
                if argv.is_empty() {
                    app.session.set_state(
                        id,
                        BlockState::Failed("alt-screen detected, argv missing".into()),
                    );
                } else {
                    app.pending_handover = Some(HandoverRequest {
                        argv,
                        reuse_block: Some(id),
                    });
                }
            }
            Ok(Err(e)) => {
                app.session.set_state(id, BlockState::Failed(e.to_string()));
            }
            Err(e) => {
                app.session
                    .set_state(id, BlockState::Failed(format!("task error: {e}")));
            }
        }
        app.session.evict_to_fit();
    }
}

async fn handle_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('d') => {
                app.quit = true;
                return;
            }
            KeyCode::Char('c') => {
                if app.focus == Focus::BlockCursor && cancel_at_cursor(app) {
                    return;
                }
                app.quit = true;
                return;
            }
            _ => {}
        }
    }
    match app.focus {
        Focus::Prompt => handle_prompt_key(app, key).await,
        Focus::BlockCursor => handle_cursor_key(app, key),
        Focus::FilterEdit => handle_filter_edit_key(app, key),
        Focus::BlockExpand => handle_block_expand_key(app, key),
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
        .set_state(id, BlockState::Failed("cancelled".into()));
    app.flash = Some(format!("cancelled %{}", id.0));
    true
}

async fn handle_prompt_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            if let Some(id) = app.newest_block_id() {
                app.session.set_cursor(Some(id));
                app.reset_pipeline_cursor();
                app.focus = Focus::BlockCursor;
            } else {
                app.flash = Some("no blocks yet".into());
            }
        }
        KeyCode::Up => history_step(app, -1),
        KeyCode::Down => history_step(app, 1),
        KeyCode::Char(c) => {
            app.prompt.push(c);
            app.history_cursor = None;
        }
        KeyCode::Backspace => {
            app.prompt.pop();
            app.history_cursor = None;
        }
        KeyCode::Enter => spawn_prompt(app).await,
        _ => {}
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
}

fn handle_cursor_key(app: &mut App, key: KeyEvent) {
    if app.write_input_mode {
        match key.code {
            KeyCode::Esc => {
                app.write_input_mode = false;
                app.write_input.clear();
            }
            KeyCode::Enter => commit_write(app),
            KeyCode::Char(c) => app.write_input.push(c),
            KeyCode::Backspace => {
                app.write_input.pop();
            }
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Esc => {
            app.focus = Focus::Prompt;
            app.session.set_cursor(None);
        }
        KeyCode::Up => app.move_cursor(-1),
        KeyCode::Down => app.move_cursor(1),
        KeyCode::Left => move_filter_cursor(app, -1),
        KeyCode::Right => move_filter_cursor(app, 1),
        KeyCode::Char('f') | KeyCode::Enter => open_filter_edit(app),
        KeyCode::Char('i') => open_filter_insert(app),
        KeyCode::Char('d') => drop_filter_at_cursor(app),
        KeyCode::Char('<') => move_filter_in_pipeline(app, -1),
        KeyCode::Char('>') => move_filter_in_pipeline(app, 1),
        KeyCode::Char('e') => {
            if app.session.cursor().is_some() {
                app.expand_scroll = 0;
                app.focus = Focus::BlockExpand;
            }
        }
        KeyCode::Char('w') => {
            if app.session.cursor().is_some() {
                app.write_input_mode = true;
                app.write_input.clear();
            }
        }
        _ => {}
    }
}

fn commit_write(app: &mut App) {
    let path = std::mem::take(&mut app.write_input);
    app.write_input_mode = false;
    let path = path.trim();
    if path.is_empty() {
        app.flash = Some("path required".into());
        return;
    }
    let Some(id) = app.session.cursor() else { return };
    let Some(block) = app.session.block(id) else { return };
    let lines = compute_block_lines(block);
    let mut text = String::new();
    for line in &lines {
        text.push_str(&line_text(line));
        text.push('\n');
    }
    match std::fs::write(path, text) {
        Ok(()) => {
            app.flash = Some(format!(
                "wrote %{} ({} lines) to {}",
                id.0,
                lines.len(),
                path
            ));
        }
        Err(e) => {
            app.flash = Some(format!("write failed: {e}"));
        }
    }
}

fn handle_block_expand_key(app: &mut App, key: KeyEvent) {
    if app.search_input_mode {
        match key.code {
            KeyCode::Esc => {
                app.search_input_mode = false;
                app.search_input.clear();
                app.search_query.clear();
                app.expand_scroll = app.search_anchor_scroll;
            }
            KeyCode::Enter => {
                app.search_input_mode = false;
                // search_query is already in sync via update_search; nothing to do.
            }
            KeyCode::Char(c) => {
                app.search_input.push(c);
                update_search(app);
            }
            KeyCode::Backspace => {
                app.search_input.pop();
                update_search(app);
            }
            _ => {}
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
                app.focus = Focus::BlockCursor;
            }
        }
        KeyCode::Char('q') => {
            app.search_query.clear();
            app.expand_scroll = 0;
            app.focus = Focus::BlockCursor;
        }
        KeyCode::Char('/') => {
            app.search_input_mode = true;
            app.search_input_backward = false;
            app.search_input.clear();
            app.search_query.clear();
            app.search_anchor_scroll = app.expand_scroll;
        }
        KeyCode::Char('?') => {
            app.search_input_mode = true;
            app.search_input_backward = true;
            app.search_input.clear();
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
    let Some(block) = app.session.block(id) else { return };
    let lines = compute_block_lines(block);
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
    let Some(block) = app.session.block(id) else { return };
    let lines = compute_block_lines(block);
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

fn compute_block_lines(block: &Block) -> Vec<Line<'static>> {
    match block.capture.as_ref() {
        Some(capture) => match apply_pipeline(&capture.stdout, &block.pipeline) {
            Ok((value, _drops)) => render_pipeline_value_with_max(value, usize::MAX),
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
    let Some(len) = app.cursor_block_pipeline_len() else {
        return;
    };
    let max = len as i32;
    let new = (app.pipeline_cursor as i32 + delta).clamp(0, max) as usize;
    app.pipeline_cursor = new;
}

fn open_filter_edit(app: &mut App) {
    let Some(id) = app.session.cursor() else { return };
    let Some(block) = app.session.block(id) else { return };
    if block.capture.is_none() {
        let msg = match block.state {
            BlockState::Running => "still running — no capture yet",
            _ => "no captured output to filter",
        };
        app.flash = Some(msg.into());
        return;
    }
    let state = if app.pipeline_cursor < block.pipeline.len() {
        FilterEditState::for_edit(block, app.pipeline_cursor)
    } else {
        FilterEditState::for_add(block)
    };
    app.filter_edit = Some(state);
    app.focus = Focus::FilterEdit;
}

fn open_filter_insert(app: &mut App) {
    let Some(id) = app.session.cursor() else { return };
    let Some(block) = app.session.block(id) else { return };
    if block.capture.is_none() {
        let msg = match block.state {
            BlockState::Running => "still running — no capture yet",
            _ => "no captured output to filter",
        };
        app.flash = Some(msg.into());
        return;
    }
    // On the `+ add` slot, `i` is functionally the same as `f`.
    let state = if app.pipeline_cursor < block.pipeline.len() {
        FilterEditState::for_insert(block, app.pipeline_cursor)
    } else {
        FilterEditState::for_add(block)
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
            app.focus = Focus::BlockCursor;
        }
        KeyCode::Tab => state.cycle_field(1),
        KeyCode::BackTab => state.cycle_field(-1),
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
    let id = state.block_id;
    let mode = state.mode;
    if let Some(block) = app.session.block_mut(id) {
        match mode {
            EditMode::Add => block.pipeline.push(spec),
            EditMode::Edit(i) if i < block.pipeline.len() => block.pipeline[i] = spec,
            EditMode::Edit(_) => block.pipeline.push(spec),
            EditMode::Insert(i) => {
                let pos = i.min(block.pipeline.len());
                block.pipeline.insert(pos, spec);
            }
        }
    }
    app.filter_edit = None;
    app.focus = Focus::BlockCursor;
    app.reset_pipeline_cursor();
}

fn move_filter_in_pipeline(app: &mut App, delta: i32) {
    let Some(id) = app.session.cursor() else { return };
    let pos = app.pipeline_cursor;
    let Some(block) = app.session.block_mut(id) else { return };
    if pos >= block.pipeline.len() {
        return; // Cursor is on the `+ add` slot, nothing to move.
    }
    let new_pos_signed = pos as i32 + delta;
    if new_pos_signed < 0 || new_pos_signed as usize >= block.pipeline.len() {
        return;
    }
    let new_pos = new_pos_signed as usize;
    block.pipeline.swap(pos, new_pos);
    app.pipeline_cursor = new_pos;
}

fn drop_filter_at_cursor(app: &mut App) {
    let Some(id) = app.session.cursor() else { return };
    let cursor = app.pipeline_cursor;
    let dropped = if let Some(block) = app.session.block_mut(id) {
        if block.pipeline.is_empty() {
            false
        } else if cursor < block.pipeline.len() {
            block.pipeline.remove(cursor);
            true
        } else {
            block.pipeline.pop();
            true
        }
    } else {
        false
    };
    if !dropped {
        app.flash = Some("no filters to drop".into());
    }
    let new_len = app
        .session
        .block(id)
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

    if force_fullscreen || needs_fullscreen(&argv) {
        app.pending_handover = Some(HandoverRequest {
            argv,
            reuse_block: None,
        });
        return;
    }

    if argv[0] == "cd" {
        run_cd_builtin(app, &argv);
        return;
    }

    let id = app.session.add_block(argv.clone());
    match exec::spawn_command(argv, CAPTURE_CAP).await {
        Ok((handle, killer)) => {
            app.running.insert(id, RunningCommand { handle, killer });
        }
        Err(e) => {
            app.session
                .set_state(id, BlockState::Failed(e.to_string()));
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
    let id = app.session.add_block(argv.to_vec());
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
            app.session.set_state(id, BlockState::Done(0));
            app.flash = Some(format!("cd → {}", new_path.display()));
        }
        Err(e) => {
            app.session
                .set_state(id, BlockState::Failed(format!("cd: {e}")));
        }
    }
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

fn draw(f: &mut Frame, app: &App) {
    match app.focus {
        Focus::FilterEdit => draw_filter_edit(f, app),
        Focus::BlockExpand => draw_block_expand(f, app),
        _ => draw_repl(f, app),
    }
}

fn draw_block_expand(f: &mut Frame, app: &App) {
    let Some(id) = app.session.cursor() else {
        return;
    };
    let Some(block) = app.session.block(id) else {
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
    let all_lines: Vec<Line<'static>> = match block.capture.as_ref() {
        Some(capture) => match apply_pipeline(&capture.stdout, &block.pipeline) {
            Ok((value, _drops)) => render_pipeline_value_with_max(value, usize::MAX),
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
        format!("inspect  %{}  {}", block.id.0, block.argv.join(" "))
    } else {
        format!(
            "inspect  %{}  {}    lines {}-{} of {}",
            block.id.0,
            block.argv.join(" "),
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

fn draw_repl(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(f.area());

    let cwd = std::env::current_dir()
        .ok()
        .map(|p| collapse_home_in_path(&p))
        .unwrap_or_else(|| "?".into());
    draw_header(f, chunks[0], &cwd);
    draw_blocks(f, chunks[1], app);
    draw_input(f, chunks[2], app);
    draw_status(f, chunks[3], app);
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
    let Some(block) = app.session.block(state.block_id) else {
        return;
    };

    let stack_rows = match state.mode {
        EditMode::Add | EditMode::Insert(_) => block.pipeline.len() + 1,
        EditMode::Edit(_) => block.pipeline.len(),
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

    let outcome = hypothetical_outcome(block, state);
    let drops: Vec<usize> = outcome
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|(_, d)| d.clone())
        .unwrap_or_default();

    let title = format!("editing  %{}  {}", block.id.0, block.argv.join(" "));
    draw_header(f, chunks[0], &title);
    draw_preview_pane(f, chunks[1], &outcome);
    draw_stack_pane(f, chunks[2], block, state, &drops);
    draw_form_pane(f, chunks[3], state);
    draw_status(f, chunks[4], app);
}

fn hypothetical_outcome(
    block: &Block,
    state: &FilterEditState,
) -> Option<Result<(PipelineValue, Vec<usize>), String>> {
    let capture = block.capture.as_ref()?;
    let mut hypothetical: Vec<FilterSpec> = block.pipeline.clone();
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

fn draw_blocks(f: &mut Frame, area: Rect, app: &App) {
    let cursor_id = app.session.cursor();
    let cursor_visible = app.focus != Focus::Prompt;

    let mut lines: Vec<Line> = Vec::new();
    for block in app.session.blocks() {
        let selected = cursor_visible && cursor_id == Some(block.id);
        let pipeline_cursor = if selected {
            Some(app.pipeline_cursor)
        } else {
            None
        };
        lines.extend(render_block(block, selected, pipeline_cursor));
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn render_block(
    block: &Block,
    selected: bool,
    pipeline_cursor: Option<usize>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let glyph = match &block.state {
        BlockState::Running => Span::styled("⏵", Style::default().fg(Color::Yellow)),
        BlockState::Done(0) => Span::styled("●", Style::default().fg(Color::Green)),
        BlockState::Done(_) => Span::styled("⚠", Style::default().fg(Color::Red)),
        BlockState::Snapshotted => Span::styled("❄", Style::default().fg(Color::LightBlue)),
        BlockState::Failed(_) => Span::styled("⚠", Style::default().fg(Color::Red)),
    };
    let id_style = if selected {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Cyan)
    };
    let prefix = if selected { "▸ " } else { "  " };
    let mut header = vec![
        Span::raw(prefix),
        Span::styled(format!(" %{} ", block.id.0), id_style),
        Span::raw(" "),
        glyph,
        Span::raw(" "),
        Span::styled(
            block.argv.join(" "),
            if selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ),
    ];
    if let Some(name) = &block.name {
        header.push(Span::styled(
            format!("  ◉ {name}"),
            Style::default().fg(Color::Magenta),
        ));
    }
    lines.push(Line::from(header));

    let pipeline_outcome = block
        .capture
        .as_ref()
        .map(|c| apply_pipeline(&c.stdout, &block.pipeline));
    let drops: Vec<usize> = pipeline_outcome
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|(_, d)| d.clone())
        .unwrap_or_else(|| vec![0; block.pipeline.len()]);

    if !block.pipeline.is_empty() || pipeline_cursor.is_some() {
        lines.push(pipeline_line(&block.pipeline, pipeline_cursor, &drops));
    }

    match pipeline_outcome {
        Some(Ok((value, _))) => lines.extend(render_pipeline_value(value)),
        Some(Err(e)) => lines.extend(filter_error_lines(&e)),
        None => {}
    }

    if let Some(capture) = &block.capture {
        if capture.truncated {
            lines.push(Line::from(Span::styled(
                "      ✂ output truncated",
                Style::default().fg(Color::Magenta),
            )));
        }
        if let Some(code) = capture.exit_code {
            if code != 0 {
                lines.push(Line::from(Span::styled(
                    format!("      exit {code}"),
                    Style::default().fg(Color::Red),
                )));
            }
        }
    } else if let BlockState::Done(code) = &block.state {
        let mut spans = vec![Span::styled(
            "      (no captured output)",
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
    if let BlockState::Failed(msg) = &block.state {
        lines.push(Line::from(vec![
            Span::raw("      "),
            Span::styled(msg.clone(), Style::default().fg(Color::Red)),
        ]));
    }

    lines.push(Line::from(""));
    lines
}

fn pipeline_line(
    pipeline: &[FilterSpec],
    selected: Option<usize>,
    drops: &[usize],
) -> Line<'static> {
    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(Color::LightCyan);
    let dim = Style::default().fg(Color::DarkGray);
    let warn = Style::default().fg(Color::Yellow);

    let mut spans = vec![Span::raw("      ")];
    for (i, f) in pipeline.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", dim));
        }
        let style = if selected == Some(i) { highlight } else { normal };
        spans.push(Span::styled(format!(" {} ", describe_filter(f)), style));
        let n = drops.get(i).copied().unwrap_or(0);
        if n > 0 {
            spans.push(Span::styled(format!(" ⓘ-{n}"), warn));
        }
    }
    if selected.is_some() {
        if !pipeline.is_empty() {
            spans.push(Span::styled(" │ ", dim));
        }
        let style = if selected == Some(pipeline.len()) {
            highlight
        } else {
            dim
        };
        spans.push(Span::styled(" + add ", style));
    }
    Line::from(spans)
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

fn render_raw_lines(bytes: &bytes::Bytes, max: usize) -> Vec<Line<'static>> {
    let parsed = ansi::parse_to_lines(bytes, "      ", max);
    let mut out = parsed.lines;
    if parsed.truncated {
        out.push(Line::from(Span::styled(
            format!("      … {} more lines", parsed.total - max),
            Style::default().fg(Color::DarkGray),
        )));
    }
    out
}

fn render_pipeline_value(value: PipelineValue) -> Vec<Line<'static>> {
    render_pipeline_value_with_max(value, PREVIEW_LINES)
}

fn render_pipeline_value_with_max(value: PipelineValue, max: usize) -> Vec<Line<'static>> {
    match value {
        PipelineValue::Bytes(b) => render_raw_lines(&b, max),
        PipelineValue::Structured(Value::List(items)) => {
            let mut out = Vec::new();
            let total = items.len();
            let columns = schema_of(&items);
            if !columns.is_empty() {
                let header_text = columns.join("   ");
                out.push(Line::from(vec![
                    Span::raw("      "),
                    Span::styled(
                        header_text,
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    ),
                ]));
            }
            for item in items.iter().take(max) {
                out.push(Line::from(vec![
                    Span::raw("      "),
                    Span::raw(format_row(item, &columns)),
                ]));
            }
            if total > max {
                out.push(Line::from(Span::styled(
                    format!("      … {} more rows", total - max),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if total == 0 {
                out.push(Line::from(Span::styled(
                    "      (no rows)",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            out
        }
        PipelineValue::Structured(other) => vec![Line::from(vec![
            Span::raw("      "),
            Span::raw(format!("{other:?}")),
        ])],
    }
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

fn format_row(v: &Value, columns: &[String]) -> String {
    match v {
        Value::Record(r) => columns
            .iter()
            .map(|c| match r.get(c) {
                Some(val) => format_scalar(val),
                None => "—".into(),
            })
            .collect::<Vec<_>>()
            .join("   "),
        other => format_scalar(other),
    }
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

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let border = TuiBlock::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray));
    let line = match app.focus {
        Focus::Prompt => Line::from(vec![
            Span::styled(
                "▶ ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(app.prompt.clone()),
            Span::styled("▏", Style::default().fg(Color::Green)),
        ]),
        Focus::BlockCursor => Line::from(vec![
            Span::styled(
                "  ",
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                match app.session.cursor() {
                    Some(id) => format!("(block %{} selected)", id.0),
                    None => "(no block)".into(),
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Focus::FilterEdit | Focus::BlockExpand => Line::from(""),
    };
    let widget = Paragraph::new(line).block(border);
    f.render_widget(widget, area);
}

fn draw_preview_pane(
    f: &mut Frame,
    area: Rect,
    outcome: &Option<Result<(PipelineValue, Vec<usize>), String>>,
) {
    let block_widget = TuiBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled("preview", Style::default().fg(Color::Cyan)),
            Span::raw(" "),
        ]));
    let inner = block_widget.inner(area);
    f.render_widget(block_widget, area);

    let max_rows = inner.height.saturating_sub(2) as usize;
    let max = max_rows.max(3);
    let lines = match outcome {
        None => vec![Line::from(Span::styled(
            "      (no capture)",
            Style::default().fg(Color::DarkGray),
        ))],
        Some(Ok((value, _))) => render_pipeline_value_with_max(value.clone(), max),
        Some(Err(e)) => filter_error_lines(e),
    };
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

fn draw_stack_pane(
    f: &mut Frame,
    area: Rect,
    block: &Block,
    state: &FilterEditState,
    drops: &[usize],
) {
    let block_widget = TuiBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled("pipeline", Style::default().fg(Color::Cyan)),
            Span::raw(" "),
        ]));
    let inner = block_widget.inner(area);
    f.render_widget(block_widget, area);

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
    for (i, spec) in block.pipeline.iter().enumerate() {
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
                format!("  {} ", circled(block.pipeline.len() + 1)),
                edit_style,
            ),
            Span::styled(active_label, edit_style),
            Span::styled("  ← editing", dim),
        ];
        let n = drops.get(block.pipeline.len()).copied().unwrap_or(0);
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
    let block_widget = TuiBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title, Style::default().fg(Color::Magenta)),
            Span::raw(" "),
        ]));
    let inner = block_widget.inner(area);
    f.render_widget(block_widget, area);

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
    if app.write_input_mode {
        let widget = Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "write to: ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(app.write_input.clone()),
            Span::styled("▏", Style::default().fg(Color::Yellow)),
        ]))
        .style(Style::default().bg(Color::DarkGray));
        f.render_widget(widget, area);
        return;
    }
    if app.search_input_mode {
        let invalid = !app.search_input.is_empty()
            && try_compile(&app.search_input, app.search_case_insensitive).is_none();
        let prefix = if app.search_input_backward { "?" } else { "/" };
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(
                prefix.to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(app.search_input.clone()),
            Span::styled("▏", Style::default().fg(Color::Yellow)),
        ];
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
            ("Esc", "focus block"),
            ("Ctrl-D", "quit"),
        ],
        Focus::BlockCursor => vec![
            ("↑↓", "blocks"),
            ("←→", "filters"),
            ("<>", "reorder"),
            ("f", "add/edit"),
            ("i", "insert"),
            ("d", "drop"),
            ("e", "expand"),
            ("w", "write"),
            ("Ctrl-C", "cancel"),
            ("Esc", "back"),
            ("Ctrl-D", "quit"),
        ],
        Focus::FilterEdit => vec![
            ("Tab", "next field"),
            ("←→", "cycle"),
            ("Enter", "apply"),
            ("Esc", "cancel"),
        ],
        Focus::BlockExpand => vec![
            ("↑↓ / jk", "scroll"),
            ("PgUp/Dn", "page"),
            ("g/G", "top/bot"),
            ("/?", "search f/b"),
            ("n/N", "next/prev"),
            ("i", "case"),
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

    fn block_with_stdout(bytes: &[u8]) -> Block {
        Block {
            id: BlockId(1),
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
            state: BlockState::Done(0),
            last_touched: Instant::now(),
        }
    }

    #[test]
    fn schema_empty_for_bytes_input() {
        let block = block_with_stdout(b"a\nb\nc\n");
        assert!(compute_schema_at(&block, 0).is_empty());
    }

    #[test]
    fn schema_has_line_after_from_lines() {
        let mut block = block_with_stdout(b"a\nb\nc\n");
        block.pipeline.push(FilterSpec::FromLines);
        assert_eq!(compute_schema_at(&block, 1), vec!["line".to_string()]);
    }

    #[test]
    fn schema_at_index_uses_filters_before_only() {
        let mut block = block_with_stdout(b"a\nb\n");
        block.pipeline.push(FilterSpec::FromLines);
        block.pipeline.push(FilterSpec::Where {
            predicate: Predicate::Matches {
                column: "line".into(),
                pattern: "a".into(),
            },
        });
        // Schema BEFORE the where filter — only from-lines applied.
        assert_eq!(compute_schema_at(&block, 1), vec!["line".to_string()]);
        // Schema BEFORE from-lines — bytes, no schema.
        assert!(compute_schema_at(&block, 0).is_empty());
    }

    #[test]
    fn filter_edit_state_picks_parser_when_input_is_bytes() {
        let block = block_with_stdout(b"a\nb\nc\n");
        let state = FilterEditState::for_add(&block);
        assert_eq!(state.kind, FilterKind::FromLines);
        assert_eq!(state.mode, EditMode::Add);
    }

    #[test]
    fn filter_edit_state_picks_where_when_schema_available() {
        let mut block = block_with_stdout(b"a\nb\nc\n");
        block.pipeline.push(FilterSpec::FromLines);
        let state = FilterEditState::for_add(&block);
        assert_eq!(state.kind, FilterKind::Where);
        assert_eq!(state.available_columns, vec!["line".to_string()]);
    }

    #[test]
    fn build_filter_for_where_requires_column() {
        let block = block_with_stdout(b"a\n");
        let mut state = FilterEditState::for_add(&block);
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
        let mut block = block_with_stdout(b"a\n");
        block.pipeline.push(FilterSpec::FromLines);
        let mut state = FilterEditState::for_add(&block);
        assert_eq!(state.kind, FilterKind::Where);
        // Walks once around the cycle and lands back on Where.
        for _ in 0..FilterKind::ALL.len() {
            state.cycle_kind(1);
        }
        assert_eq!(state.kind, FilterKind::Where);
    }

    #[test]
    fn build_filter_sort_by_single_key() {
        let mut block = block_with_stdout(b"a\n");
        block.pipeline.push(FilterSpec::FromLines);
        let mut state = FilterEditState::for_add(&block);
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
        let mut block = block_with_stdout(b"a b c\n");
        block.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&block);
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
        let block = block_with_stdout(b"x\n");
        let mut state = FilterEditState::for_add(&block);
        state.kind = FilterKind::SortBy;
        state.sort_keys.clear();
        assert!(state.build_filter().is_none());
    }

    #[test]
    fn build_filter_uniq_no_columns_means_full_dedupe() {
        let mut block = block_with_stdout(b"a b\n");
        block.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&block);
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
        let mut block = block_with_stdout(b"a b c\n");
        block.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&block);
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
        let mut block = block_with_stdout(b"a b\n");
        block.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&block);
        state.kind = FilterKind::Rename;
        // All inputs empty.
        assert!(state.build_filter().is_none());
    }

    #[test]
    fn for_edit_prepopulates_rename() {
        let mut block = block_with_stdout(b"a b c\n");
        block.pipeline.push(FilterSpec::FromFields);
        block.pipeline.push(FilterSpec::Rename {
            pairs: vec![("_2".into(), "size".into())],
        });
        let state = FilterEditState::for_edit(&block, 1);
        assert_eq!(state.kind, FilterKind::Rename);
        assert_eq!(state.available_columns, vec!["_1", "_2", "_3"]);
        assert_eq!(state.rename_to_inputs, vec!["", "size", ""]);
    }

    #[test]
    fn for_edit_prepopulates_multi_key_sort_by() {
        let mut block = block_with_stdout(b"a b c\n");
        block.pipeline.push(FilterSpec::FromFields);
        block.pipeline.push(FilterSpec::SortBy {
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
        let state = FilterEditState::for_edit(&block, 1);
        assert_eq!(state.kind, FilterKind::SortBy);
        assert_eq!(state.available_columns, vec!["_1", "_2", "_3"]);
        assert_eq!(state.sort_keys.len(), 2);
        assert_eq!(state.sort_keys[0], (2, SortDirection::Desc));
        assert_eq!(state.sort_keys[1], (0, SortDirection::Asc));
    }

    #[test]
    fn for_edit_prepopulates_from_csv() {
        let mut block = block_with_stdout(b"a,b\n1,2\n");
        block.pipeline.push(FilterSpec::FromCsv {
            delim: ';',
            has_header: false,
        });
        let state = FilterEditState::for_edit(&block, 0);
        assert_eq!(state.kind, FilterKind::FromCsv);
        assert_eq!(state.csv_delim, ';');
        assert!(!state.csv_has_header);
    }

    #[test]
    fn for_edit_prepopulates_from_regex() {
        let mut block = block_with_stdout(b"x\n");
        block.pipeline.push(FilterSpec::FromRegex {
            pattern: r"(?<k>\w+)".into(),
        });
        let state = FilterEditState::for_edit(&block, 0);
        assert_eq!(state.kind, FilterKind::FromRegex);
        assert_eq!(state.regex_pattern, r"(?<k>\w+)");
    }

    #[test]
    fn build_filter_from_regex_requires_pattern() {
        let block = block_with_stdout(b"x\n");
        let mut state = FilterEditState::for_add(&block);
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
        let block = block_with_stdout(b"x\n");
        let mut state = FilterEditState::for_add(&block);
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
        let mut block = block_with_stdout(b"a\n");
        block.pipeline.push(FilterSpec::FromLines);
        let mut state = FilterEditState::for_add(&block);
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
        let mut block = block_with_stdout(b"a b\n");
        block.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&block);
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
        let mut block = block_with_stdout(b"a\nb\nc\n");
        block.pipeline.push(FilterSpec::FromLines);
        block.pipeline.push(FilterSpec::Take { n: 2 });
        let state = FilterEditState::for_edit(&block, 1);
        assert_eq!(state.kind, FilterKind::Take);
        assert_eq!(state.n_input, "2");
    }

    #[test]
    fn build_filter_where_single_clause_uses_op_for_predicate_kind() {
        let mut block = block_with_stdout(b"a\n");
        block.pipeline.push(FilterSpec::FromLines);
        let mut state = FilterEditState::for_add(&block);
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
        let mut block = block_with_stdout(b"a b\n");
        block.pipeline.push(FilterSpec::FromFields);
        let mut state = FilterEditState::for_add(&block);
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
        let mut block = block_with_stdout(b"a b\n");
        block.pipeline.push(FilterSpec::FromFields);
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
        block.pipeline.push(FilterSpec::Where { predicate: p });
        let state = FilterEditState::for_edit(&block, 1);
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
        let mut block = block_with_stdout(b"1\n2\n10\n");
        block.pipeline.push(FilterSpec::FromLines);
        block.pipeline.push(FilterSpec::Where {
            predicate: Predicate::Compare {
                column: "line".into(),
                op: CompareOp::Gt,
                value: Value::Int(5),
            },
        });
        let state = FilterEditState::for_edit(&block, 1);
        assert_eq!(state.kind, FilterKind::Where);
        assert_eq!(state.where_clauses.len(), 1);
        assert_eq!(state.where_clauses[0].op, WhereOp::Gt);
        assert_eq!(state.where_clauses[0].pattern, "5");
    }

    #[test]
    fn for_edit_prepopulates_contains_predicate() {
        let mut block = block_with_stdout(b"hello\n");
        block.pipeline.push(FilterSpec::FromLines);
        block.pipeline.push(FilterSpec::Where {
            predicate: Predicate::Contains {
                column: "line".into(),
                substring: "ell".into(),
            },
        });
        let state = FilterEditState::for_edit(&block, 1);
        assert_eq!(state.where_clauses[0].op, WhereOp::Contains);
        assert_eq!(state.where_clauses[0].pattern, "ell");
    }

    #[test]
    fn for_edit_prepopulates_select_columns() {
        let mut block = block_with_stdout(b"a b c\n");
        block.pipeline.push(FilterSpec::FromFields);
        block.pipeline.push(FilterSpec::Select {
            columns: vec!["_1".into(), "_3".into()],
        });
        let state = FilterEditState::for_edit(&block, 1);
        assert_eq!(state.kind, FilterKind::Select);
        // available_columns at index 1 = [_1, _2, _3]; selections should mark _1 and _3
        assert_eq!(state.available_columns, vec!["_1", "_2", "_3"]);
        assert_eq!(state.column_selections, vec![true, false, true]);
    }

    #[test]
    fn for_edit_prepopulates_from_existing_where() {
        let mut block = block_with_stdout(b"a\nb\nbb\n");
        block.pipeline.push(FilterSpec::FromLines);
        block.pipeline.push(FilterSpec::Where {
            predicate: Predicate::Matches {
                column: "line".into(),
                pattern: "bb".into(),
            },
        });
        let state = FilterEditState::for_edit(&block, 1);
        assert_eq!(state.kind, FilterKind::Where);
        assert_eq!(state.mode, EditMode::Edit(1));
        assert_eq!(state.where_clauses[0].pattern, "bb");
        assert_eq!(state.available_columns, vec!["line".to_string()]);
        assert_eq!(state.selected_column(), Some("line"));
    }

    #[test]
    fn for_edit_prepopulates_from_existing_from_lines() {
        let mut block = block_with_stdout(b"x\n");
        block.pipeline.push(FilterSpec::FromLines);
        let state = FilterEditState::for_edit(&block, 0);
        assert_eq!(state.kind, FilterKind::FromLines);
        assert_eq!(state.mode, EditMode::Edit(0));
    }

    #[test]
    fn for_insert_inserts_before_existing_filter() {
        let mut block = block_with_stdout(b"a\nb\nc\n");
        block.pipeline.push(FilterSpec::FromLines);
        block.pipeline.push(FilterSpec::Take { n: 5 });
        let state = FilterEditState::for_insert(&block, 1);
        assert_eq!(state.mode, EditMode::Insert(1));
        // Schema is computed BEFORE index 1, i.e. after FromLines.
        assert_eq!(state.available_columns, vec!["line".to_string()]);
    }

    #[test]
    fn apply_filter_edit_insert_pushes_existing_right() {
        let mut block = block_with_stdout(b"a\n");
        block.pipeline.push(FilterSpec::FromLines);
        block.pipeline.push(FilterSpec::Take { n: 5 });
        // Insert a `where` between FromLines and Take.
        let mut state = FilterEditState::for_insert(&block, 1);
        state.kind = FilterKind::Where;
        state.where_clauses[0].pattern = "x".into();
        // Direct pipeline mutation (mirrors apply_filter_edit's logic).
        let spec = state.build_filter().expect("buildable");
        match state.mode {
            EditMode::Insert(i) => block.pipeline.insert(i, spec),
            _ => panic!("expected Insert"),
        }
        assert_eq!(block.pipeline.len(), 3);
        assert!(matches!(block.pipeline[0], FilterSpec::FromLines));
        assert!(matches!(block.pipeline[1], FilterSpec::Where { .. }));
        assert!(matches!(block.pipeline[2], FilterSpec::Take { n: 5 }));
    }
}
