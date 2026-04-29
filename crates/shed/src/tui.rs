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
    Block, BlockId, BlockState, Filter, FilterSpec, PipelineValue, Predicate, Session, Value,
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
    Where,
}

impl FilterKind {
    const ALL: &'static [FilterKind] = &[FilterKind::FromLines, FilterKind::Where];

    fn name(self) -> &'static str {
        match self {
            FilterKind::FromLines => "from-lines",
            FilterKind::Where => "where",
        }
    }

    fn description(self) -> &'static str {
        match self {
            FilterKind::FromLines => "parse raw bytes into one record per line",
            FilterKind::Where => "keep rows whose column matches a regex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    Kind,
    Column,
    Pattern,
}

struct FilterEditState {
    block_id: BlockId,
    kind: FilterKind,
    where_column: usize,
    where_pattern: String,
    available_columns: Vec<String>,
    field: FormField,
}

impl FilterEditState {
    fn from_block(block: &Block) -> Self {
        let available_columns = compute_schema(block);
        let kind = if available_columns.is_empty() {
            FilterKind::FromLines
        } else {
            FilterKind::Where
        };
        Self {
            block_id: block.id,
            kind,
            where_column: 0,
            where_pattern: String::new(),
            available_columns,
            field: FormField::Kind,
        }
    }

    fn fields(&self) -> &'static [FormField] {
        match self.kind {
            FilterKind::FromLines => &[FormField::Kind],
            FilterKind::Where => &[FormField::Kind, FormField::Column, FormField::Pattern],
        }
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

    fn build_filter(&self) -> Option<FilterSpec> {
        match self.kind {
            FilterKind::FromLines => Some(FilterSpec::FromLines),
            FilterKind::Where => {
                let column = self.selected_column()?;
                Some(FilterSpec::Where {
                    predicate: Predicate::Matches {
                        column: column.to_string(),
                        pattern: self.where_pattern.clone(),
                    },
                })
            }
        }
    }
}

fn compute_schema(block: &Block) -> Vec<String> {
    let Some(capture) = &block.capture else {
        return Vec::new();
    };
    let mut value = PipelineValue::Bytes(capture.stdout.clone());
    for filter in &block.pipeline {
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
        }
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
        KeyCode::Char('f') | KeyCode::Enter => open_filter_edit(app),
        KeyCode::Char('d') => drop_last_filter(app),
        _ => {}
    }
}

fn open_filter_edit(app: &mut App) {
    let Some(id) = app.session.cursor() else {
        return;
    };
    let Some(block) = app.session.block(id) else {
        return;
    };
    app.filter_edit = Some(FilterEditState::from_block(block));
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
            FormField::Pattern => handle_pattern_key(state, key),
        },
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

fn apply_filter_edit(app: &mut App) {
    let Some(state) = app.filter_edit.as_ref() else {
        return;
    };
    let Some(spec) = state.build_filter() else {
        app.flash = Some("pick a column first".into());
        return;
    };
    let id = state.block_id;
    if let Some(block) = app.session.block_mut(id) {
        block.pipeline.push(spec);
    }
    app.filter_edit = None;
    app.focus = Focus::BlockCursor;
}

fn drop_last_filter(app: &mut App) {
    let Some(id) = app.session.cursor() else { return };
    if let Some(block) = app.session.block_mut(id) {
        if block.pipeline.pop().is_none() {
            app.flash = Some("no filters to drop".into());
        }
    }
}

async fn run_prompt(app: &mut App) {
    let argv: Vec<String> = app
        .prompt
        .split_whitespace()
        .map(str::to_string)
        .collect();
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

    let stack_height = (block.pipeline.len() + 1) as u16 + 2;
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
        lines.extend(render_block(block, selected));
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn render_block(block: &Block, selected: bool) -> Vec<Line<'static>> {
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

    if !block.pipeline.is_empty() {
        lines.push(pipeline_line(&block.pipeline));
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

fn pipeline_line(pipeline: &[FilterSpec]) -> Line<'static> {
    let mut spans = vec![Span::raw("      ")];
    for (i, f) in pipeline.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        }
        spans.push(Span::styled(
            describe_filter(f),
            Style::default().fg(Color::LightCyan),
        ));
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
        FilterSpec::Where { predicate } => format!("where {}", describe_predicate(predicate)),
    }
}

fn describe_predicate(p: &Predicate) -> String {
    match p {
        Predicate::Matches { column, pattern } => format!("{column} matches {pattern}"),
        Predicate::And(a, b) => format!("({} && {})", describe_predicate(a), describe_predicate(b)),
        Predicate::Or(a, b) => format!("({} || {})", describe_predicate(a), describe_predicate(b)),
        Predicate::Not(p) => format!("!{}", describe_predicate(p)),
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
    if let Some(spec) = state.build_filter() {
        hypothetical.push(spec);
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

    let mut lines: Vec<Line> = Vec::new();
    for (i, spec) in block.pipeline.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", circled(i + 1)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                describe_filter(spec),
                Style::default().fg(Color::LightCyan),
            ),
        ]));
    }
    let active_label = match state.build_filter() {
        Some(spec) => describe_filter(&spec),
        None => format!("{} (incomplete)", state.kind.name()),
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {} ", circled(block.pipeline.len() + 1)),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            active_label,
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ← editing", Style::default().fg(Color::DarkGray)),
    ]));

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
        FormField::Pattern => "pattern",
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
        FormField::Pattern => {
            let cursor_marker = if active { "▏" } else { "" };
            let value_style = if active {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };
            let mut spans = Vec::new();
            spans.push(Span::styled(state.where_pattern.clone(), value_style));
            spans.push(Span::styled(
                cursor_marker,
                Style::default().fg(Color::Magenta),
            ));
            if state.where_pattern.is_empty() && !active {
                spans.push(Span::styled(
                    "(empty: matches everything)",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            spans
        }
    };

    let mut all = vec![label_span];
    all.extend(value_spans);
    Line::from(all)
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
            ("↑↓", "navigate"),
            ("f", "add filter"),
            ("d", "drop last"),
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
        assert!(compute_schema(&block).is_empty());
    }

    #[test]
    fn schema_has_line_after_from_lines() {
        let mut block = block_with_stdout(b"a\nb\nc\n");
        block.pipeline.push(FilterSpec::FromLines);
        assert_eq!(compute_schema(&block), vec!["line".to_string()]);
    }

    #[test]
    fn filter_edit_state_picks_parser_when_input_is_bytes() {
        let block = block_with_stdout(b"a\nb\nc\n");
        let state = FilterEditState::from_block(&block);
        assert_eq!(state.kind, FilterKind::FromLines);
    }

    #[test]
    fn filter_edit_state_picks_where_when_schema_available() {
        let mut block = block_with_stdout(b"a\nb\nc\n");
        block.pipeline.push(FilterSpec::FromLines);
        let state = FilterEditState::from_block(&block);
        assert_eq!(state.kind, FilterKind::Where);
        assert_eq!(state.available_columns, vec!["line".to_string()]);
    }

    #[test]
    fn build_filter_for_where_requires_column() {
        let block = block_with_stdout(b"a\n");
        let mut state = FilterEditState::from_block(&block);
        state.kind = FilterKind::Where;
        // No columns available
        assert!(state.build_filter().is_none());
    }

    #[test]
    fn cycle_kind_changes_filter_type() {
        let mut block = block_with_stdout(b"a\n");
        block.pipeline.push(FilterSpec::FromLines);
        let mut state = FilterEditState::from_block(&block);
        assert_eq!(state.kind, FilterKind::Where);
        state.cycle_kind(1);
        assert_eq!(state.kind, FilterKind::FromLines);
        state.cycle_kind(1);
        assert_eq!(state.kind, FilterKind::Where);
    }
}
