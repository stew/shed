use std::io;
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
    Block, BlockId, BlockState, CompareOp, Filter, FilterSpec, PipelineValue, Predicate, Session,
    SortDirection, SortKey, Value,
};

use crate::exec::run_command;

const CAPTURE_CAP: usize = 16 * 1024 * 1024;
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const PREVIEW_LINES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Prompt,
    BlockCursor,
    FilterEdit,
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
    SortColumn,
    SortDir,
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

struct FilterEditState {
    block_id: BlockId,
    kind: FilterKind,
    where_column: usize,
    where_op: WhereOp,
    where_pattern: String,
    n_input: String,
    column_selections: Vec<bool>,
    column_cursor: usize,
    csv_delim: char,
    csv_has_header: bool,
    regex_pattern: String,
    sort_column: usize,
    sort_direction: SortDirection,
    available_columns: Vec<String>,
    field: FormField,
    editing_index: Option<usize>,
}

impl FilterEditState {
    fn empty(block_id: BlockId, available_columns: Vec<String>, editing_index: Option<usize>) -> Self {
        let column_selections = vec![false; available_columns.len()];
        Self {
            block_id,
            kind: FilterKind::FromLines,
            where_column: 0,
            where_op: WhereOp::Matches,
            where_pattern: String::new(),
            n_input: String::new(),
            column_selections,
            column_cursor: 0,
            csv_delim: ',',
            csv_has_header: true,
            regex_pattern: String::new(),
            sort_column: 0,
            sort_direction: SortDirection::Asc,
            available_columns,
            field: FormField::Kind,
            editing_index,
        }
    }

    fn for_add(block: &Block) -> Self {
        let available_columns = compute_schema_at(block, block.pipeline.len());
        let mut state = Self::empty(block.id, available_columns, None);
        state.kind = if state.available_columns.is_empty() {
            FilterKind::FromLines
        } else {
            FilterKind::Where
        };
        state
    }

    fn for_edit(block: &Block, index: usize) -> Self {
        let available_columns = compute_schema_at(block, index);
        let mut state = Self::empty(block.id, available_columns, Some(index));
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
                if let Some(first) = keys.first() {
                    state.sort_column = state
                        .available_columns
                        .iter()
                        .position(|c| c == &first.column)
                        .unwrap_or(0);
                    state.sort_direction = first.direction;
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
            Some(FilterSpec::Where {
                predicate: Predicate::Matches { column, pattern },
            }) => {
                state.kind = FilterKind::Where;
                state.where_op = WhereOp::Matches;
                state.where_column = state
                    .available_columns
                    .iter()
                    .position(|c| c == column)
                    .unwrap_or(0);
                state.where_pattern = pattern.clone();
            }
            Some(FilterSpec::Where {
                predicate: Predicate::Contains { column, substring },
            }) => {
                state.kind = FilterKind::Where;
                state.where_op = WhereOp::Contains;
                state.where_column = state
                    .available_columns
                    .iter()
                    .position(|c| c == column)
                    .unwrap_or(0);
                state.where_pattern = substring.clone();
            }
            Some(FilterSpec::Where {
                predicate: Predicate::Compare { column, op, value },
            }) => {
                state.kind = FilterKind::Where;
                state.where_op = compare_op_to_where_op(*op);
                state.where_column = state
                    .available_columns
                    .iter()
                    .position(|c| c == column)
                    .unwrap_or(0);
                state.where_pattern = value_to_input_string(value);
            }
            Some(FilterSpec::Where { .. }) => state.kind = FilterKind::Where,
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
                FormField::Column,
                FormField::Op,
                FormField::Pattern,
            ],
            FilterKind::Select | FilterKind::Drop | FilterKind::Uniq => {
                &[FormField::Kind, FormField::Columns]
            }
            FilterKind::Take | FilterKind::Skip => &[FormField::Kind, FormField::N],
            FilterKind::SortBy => &[FormField::Kind, FormField::SortColumn, FormField::SortDir],
        }
    }

    fn cycle_sort_column(&mut self, delta: i32) {
        if self.available_columns.is_empty() {
            return;
        }
        let len = self.available_columns.len() as i32;
        self.sort_column = (self.sort_column as i32 + delta).rem_euclid(len) as usize;
    }

    fn flip_sort_direction(&mut self) {
        self.sort_direction = match self.sort_direction {
            SortDirection::Asc => SortDirection::Desc,
            SortDirection::Desc => SortDirection::Asc,
        };
    }

    fn cycle_delim(&mut self, delta: i32) {
        let i = DELIM_CHOICES
            .iter()
            .position(|(c, _)| *c == self.csv_delim)
            .unwrap_or(0) as i32;
        let new_i = (i + delta).rem_euclid(DELIM_CHOICES.len() as i32) as usize;
        self.csv_delim = DELIM_CHOICES[new_i].0;
    }

    fn cycle_op(&mut self, delta: i32) {
        let i = WhereOp::ALL.iter().position(|o| *o == self.where_op).unwrap_or(0) as i32;
        let new_i = (i + delta).rem_euclid(WhereOp::ALL.len() as i32) as usize;
        self.where_op = WhereOp::ALL[new_i];
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
        let i = self.where_column as i32;
        let new_i = (i + delta).rem_euclid(self.available_columns.len() as i32) as usize;
        self.where_column = new_i;
    }

    fn selected_column(&self) -> Option<&str> {
        self.available_columns.get(self.where_column).map(|s| s.as_str())
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
                let column = self.selected_column()?.to_string();
                let predicate = match self.where_op {
                    WhereOp::Matches => Predicate::Matches {
                        column,
                        pattern: self.where_pattern.clone(),
                    },
                    WhereOp::Contains => Predicate::Contains {
                        column,
                        substring: self.where_pattern.clone(),
                    },
                    other => Predicate::Compare {
                        column,
                        op: other.to_compare_op()?,
                        value: parse_value_input(&self.where_pattern),
                    },
                };
                Some(FilterSpec::Where { predicate })
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
                let col = self.available_columns.get(self.sort_column)?.clone();
                Some(FilterSpec::SortBy {
                    keys: vec![SortKey {
                        column: col,
                        direction: self.sort_direction,
                    }],
                })
            }
            FilterKind::Uniq => {
                let cols = self.selected_columns();
                Some(FilterSpec::Uniq {
                    by: if cols.is_empty() { None } else { Some(cols) },
                })
            }
            FilterKind::Count => Some(FilterSpec::Count),
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
    focus: Focus,
    filter_edit: Option<FilterEditState>,
    pipeline_cursor: usize,
    flash: Option<String>,
    quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            session: Session::new(),
            prompt: String::new(),
            focus: Focus::Prompt,
            filter_edit: None,
            pipeline_cursor: 0,
            flash: None,
            quit: false,
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

async fn handle_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
    {
        app.quit = true;
        return;
    }
    match app.focus {
        Focus::Prompt => handle_prompt_key(app, key).await,
        Focus::BlockCursor => handle_cursor_key(app, key),
        Focus::FilterEdit => handle_filter_edit_key(app, key),
    }
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
        KeyCode::Char(c) => app.prompt.push(c),
        KeyCode::Backspace => {
            app.prompt.pop();
        }
        KeyCode::Enter => run_prompt(app).await,
        _ => {}
    }
}

fn handle_cursor_key(app: &mut App, key: KeyEvent) {
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
        KeyCode::Char('d') => drop_filter_at_cursor(app),
        _ => {}
    }
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
    let Some(id) = app.session.cursor() else {
        return;
    };
    let Some(block) = app.session.block(id) else {
        return;
    };
    let state = if app.pipeline_cursor < block.pipeline.len() {
        FilterEditState::for_edit(block, app.pipeline_cursor)
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
            FormField::SortColumn => handle_sort_column_key(state, key),
            FormField::SortDir => handle_sort_dir_key(state, key),
        },
    }
}

fn handle_sort_column_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Left | KeyCode::Up => state.cycle_sort_column(-1),
        KeyCode::Right | KeyCode::Down => state.cycle_sort_column(1),
        _ => {}
    }
}

fn handle_sort_dir_key(state: &mut FilterEditState, key: KeyEvent) {
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Char(' ') => {
            state.flip_sort_direction();
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
        KeyCode::Left | KeyCode::Up => state.cycle_op(-1),
        KeyCode::Right | KeyCode::Down => state.cycle_op(1),
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
    match key.code {
        KeyCode::Char(c) => state.where_pattern.push(c),
        KeyCode::Backspace => {
            state.where_pattern.pop();
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
    let editing_index = state.editing_index;
    if let Some(block) = app.session.block_mut(id) {
        match editing_index {
            None => block.pipeline.push(spec),
            Some(i) if i < block.pipeline.len() => block.pipeline[i] = spec,
            Some(_) => block.pipeline.push(spec),
        }
    }
    app.filter_edit = None;
    app.focus = Focus::BlockCursor;
    app.reset_pipeline_cursor();
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

async fn run_prompt(app: &mut App) {
    let trimmed = app.prompt.trim();
    if trimmed.is_empty() {
        return;
    }
    let Some(argv) = shlex::split(trimmed) else {
        app.flash = Some("unmatched quote".into());
        return;
    };
    if argv.is_empty() {
        return;
    }
    app.prompt.clear();
    let id = app.session.add_block(argv.clone());

    match run_command(&argv, CAPTURE_CAP).await {
        Ok(capture) => {
            let exit = capture.exit_code.unwrap_or(-1);
            app.session.set_capture(id, capture);
            app.session.set_state(id, BlockState::Done(exit));
        }
        Err(e) => {
            app.session.set_state(id, BlockState::Failed(e.to_string()));
        }
    }
    app.session.evict_to_fit();
}

fn draw(f: &mut Frame, app: &App) {
    match app.focus {
        Focus::FilterEdit => draw_filter_edit(f, app),
        _ => draw_repl(f, app),
    }
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

    draw_header(f, chunks[0], "shed");
    draw_blocks(f, chunks[1], app);
    draw_input(f, chunks[2], app);
    draw_status(f, chunks[3], app);
}

fn draw_filter_edit(f: &mut Frame, app: &App) {
    let Some(state) = app.filter_edit.as_ref() else {
        return;
    };
    let Some(block) = app.session.block(state.block_id) else {
        return;
    };

    let stack_rows = if state.editing_index.is_some() {
        block.pipeline.len()
    } else {
        block.pipeline.len() + 1
    };
    let stack_height = stack_rows.max(1) as u16 + 2;
    let form_height = (state.fields().len() as u16) + 2;

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

    let title = format!("editing  %{}  {}", block.id.0, block.argv.join(" "));
    draw_header(f, chunks[0], &title);
    draw_preview_pane(f, chunks[1], block, state);
    draw_stack_pane(f, chunks[2], block, state);
    draw_form_pane(f, chunks[3], state);
    draw_status(f, chunks[4], app);
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

    if !block.pipeline.is_empty() || pipeline_cursor.is_some() {
        lines.push(pipeline_line(&block.pipeline, pipeline_cursor));
    }

    lines.extend(render_block_preview(block));

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

fn pipeline_line(pipeline: &[FilterSpec], selected: Option<usize>) -> Line<'static> {
    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(Color::LightCyan);
    let dim = Style::default().fg(Color::DarkGray);

    let mut spans = vec![Span::raw("      ")];
    for (i, f) in pipeline.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", dim));
        }
        let style = if selected == Some(i) { highlight } else { normal };
        spans.push(Span::styled(format!(" {} ", describe_filter(f)), style));
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

fn render_block_preview(block: &Block) -> Vec<Line<'static>> {
    let Some(capture) = &block.capture else {
        return Vec::new();
    };
    let value = apply_pipeline(&capture.stdout, &block.pipeline);
    render_value_or_error(value)
}

fn apply_pipeline(
    bytes: &bytes::Bytes,
    pipeline: &[FilterSpec],
) -> Result<PipelineValue, String> {
    let mut value = PipelineValue::Bytes(bytes.clone());
    for filter in pipeline {
        match filter.apply(value) {
            Ok(v) => value = v,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(value)
}

fn render_value_or_error(value: Result<PipelineValue, String>) -> Vec<Line<'static>> {
    match value {
        Ok(v) => render_pipeline_value(v),
        Err(e) => vec![Line::from(vec![
            Span::raw("      "),
            Span::styled(
                format!("filter error: {e}"),
                Style::default().fg(Color::Red),
            ),
        ])],
    }
}

fn render_raw_lines(bytes: &bytes::Bytes, max: usize) -> Vec<Line<'static>> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    let preview: Vec<&str> = text.lines().take(max).collect();
    for line in &preview {
        out.push(Line::from(vec![
            Span::raw("      "),
            Span::raw(line.to_string()),
        ]));
    }
    let total = text.lines().count();
    if total > max {
        out.push(Line::from(Span::styled(
            format!("      … {} more lines", total - max),
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
        Focus::FilterEdit => Line::from(""),
    };
    let widget = Paragraph::new(line).block(border);
    f.render_widget(widget, area);
}

fn draw_preview_pane(f: &mut Frame, area: Rect, block: &Block, state: &FilterEditState) {
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
    let lines = compute_preview_lines(block, state, max_rows.max(3));
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

fn compute_preview_lines(
    block: &Block,
    state: &FilterEditState,
    max: usize,
) -> Vec<Line<'static>> {
    let Some(capture) = &block.capture else {
        return vec![Line::from(Span::styled(
            "      (no capture)",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let mut hypothetical: Vec<FilterSpec> = block.pipeline.clone();
    match (state.editing_index, state.build_filter()) {
        (None, Some(spec)) => hypothetical.push(spec),
        (Some(i), Some(spec)) if i < hypothetical.len() => hypothetical[i] = spec,
        _ => {}
    }
    let value = apply_pipeline(&capture.stdout, &hypothetical);
    match value {
        Ok(v) => render_pipeline_value_with_max(v, max),
        Err(e) => vec![Line::from(vec![
            Span::raw("      "),
            Span::styled(
                format!("filter error: {e}"),
                Style::default().fg(Color::Red),
            ),
        ])],
    }
}

fn draw_stack_pane(f: &mut Frame, area: Rect, block: &Block, state: &FilterEditState) {
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

    let mut lines: Vec<Line> = Vec::new();
    for (i, spec) in block.pipeline.iter().enumerate() {
        let is_editing_here = state.editing_index == Some(i);
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
        lines.push(Line::from(spans));
    }
    if state.editing_index.is_none() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", circled(block.pipeline.len() + 1)),
                edit_style,
            ),
            Span::styled(active_label, edit_style),
            Span::styled("  ← editing", dim),
        ]));
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
        lines.push(render_form_row(state, *field));
    }
    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

fn render_form_row(state: &FilterEditState, field: FormField) -> Line<'static> {
    let active = state.field == field;
    let label = match field {
        FormField::Kind => "filter",
        FormField::Column => "column",
        FormField::Op => "op",
        FormField::Pattern => state.where_op.value_label(),
        FormField::N => "n",
        FormField::Columns => "columns",
        FormField::CsvDelim => "delim",
        FormField::CsvHasHeader => "header",
        FormField::RegexPattern => "regex",
        FormField::SortColumn => "column",
        FormField::SortDir => "direction",
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
        FormField::Op => render_select_value(state.where_op.name(), state.where_op.description(), active),
        FormField::Pattern => {
            let empty_hint = match state.where_op {
                WhereOp::Matches => "(empty: matches everything)",
                WhereOp::Contains => "(empty: matches everything)",
                _ => "(unset)",
            };
            render_text_field(&state.where_pattern, active, empty_hint)
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
        FormField::SortColumn => {
            if state.available_columns.is_empty() {
                vec![Span::styled(
                    "(no columns — pipeline output is bytes; add a parser)",
                    Style::default().fg(Color::Red),
                )]
            } else {
                let col = state
                    .available_columns
                    .get(state.sort_column)
                    .map(String::as_str)
                    .unwrap_or("");
                render_select_value(col, "", active)
            }
        }
        FormField::SortDir => {
            let (label, desc) = match state.sort_direction {
                SortDirection::Asc => ("asc ↑", "smallest first"),
                SortDirection::Desc => ("desc ↓", "largest first"),
            };
            render_select_value(label, desc, active)
        }
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
            ("Esc", "focus block"),
            ("Ctrl-D", "quit"),
        ],
        Focus::BlockCursor => vec![
            ("↑↓", "blocks"),
            ("←→", "filters"),
            ("f", "add/edit"),
            ("d", "drop"),
            ("Esc", "back"),
            ("Ctrl-D", "quit"),
        ],
        Focus::FilterEdit => vec![
            ("Tab", "next field"),
            ("←→", "cycle"),
            ("Enter", "apply"),
            ("Esc", "cancel"),
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
        assert_eq!(state.editing_index, None);
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
    fn build_filter_sort_by_includes_direction() {
        let mut block = block_with_stdout(b"a\n");
        block.pipeline.push(FilterSpec::FromLines);
        let mut state = FilterEditState::for_add(&block);
        state.kind = FilterKind::SortBy;
        state.sort_direction = SortDirection::Desc;
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
    fn for_edit_prepopulates_sort_by() {
        let mut block = block_with_stdout(b"a\n");
        block.pipeline.push(FilterSpec::FromLines);
        block.pipeline.push(FilterSpec::SortBy {
            keys: vec![SortKey {
                column: "line".into(),
                direction: SortDirection::Desc,
            }],
        });
        let state = FilterEditState::for_edit(&block, 1);
        assert_eq!(state.kind, FilterKind::SortBy);
        assert_eq!(state.sort_direction, SortDirection::Desc);
        assert_eq!(state.available_columns, vec!["line"]);
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
    fn build_filter_where_uses_op_for_predicate_kind() {
        let mut block = block_with_stdout(b"a\n");
        block.pipeline.push(FilterSpec::FromLines);
        let mut state = FilterEditState::for_add(&block);
        state.kind = FilterKind::Where;

        state.where_op = WhereOp::Matches;
        state.where_pattern = "abc".into();
        match state.build_filter() {
            Some(FilterSpec::Where { predicate: Predicate::Matches { pattern, .. } }) => {
                assert_eq!(pattern, "abc");
            }
            _ => panic!("expected Matches"),
        }

        state.where_op = WhereOp::Contains;
        match state.build_filter() {
            Some(FilterSpec::Where { predicate: Predicate::Contains { substring, .. } }) => {
                assert_eq!(substring, "abc");
            }
            _ => panic!("expected Contains"),
        }

        state.where_op = WhereOp::Gt;
        state.where_pattern = "42".into();
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
        assert_eq!(state.where_op, WhereOp::Gt);
        assert_eq!(state.where_pattern, "5");
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
        assert_eq!(state.where_op, WhereOp::Contains);
        assert_eq!(state.where_pattern, "ell");
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
        assert_eq!(state.editing_index, Some(1));
        assert_eq!(state.where_pattern, "bb");
        assert_eq!(state.available_columns, vec!["line".to_string()]);
        assert_eq!(state.selected_column(), Some("line"));
    }

    #[test]
    fn for_edit_prepopulates_from_existing_from_lines() {
        let mut block = block_with_stdout(b"x\n");
        block.pipeline.push(FilterSpec::FromLines);
        let state = FilterEditState::for_edit(&block, 0);
        assert_eq!(state.kind, FilterKind::FromLines);
        assert_eq!(state.editing_index, Some(0));
    }
}
