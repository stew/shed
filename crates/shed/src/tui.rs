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
    FilterAdd,
}

struct App {
    session: Session,
    prompt: String,
    filter_input: String,
    focus: Focus,
    flash: Option<String>,
    quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            session: Session::new(),
            prompt: String::new(),
            filter_input: String::new(),
            focus: Focus::Prompt,
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
        let idx = ids.iter().position(|id| *id == cur);
        if let Some(i) = idx {
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
        Focus::FilterAdd => handle_filter_add_key(app, key),
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
        KeyCode::Char('f') | KeyCode::Enter => {
            app.filter_input.clear();
            app.focus = Focus::FilterAdd;
        }
        KeyCode::Char('d') => drop_last_filter(app),
        _ => {}
    }
}

fn handle_filter_add_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.filter_input.clear();
            app.focus = Focus::BlockCursor;
        }
        KeyCode::Char(c) => app.filter_input.push(c),
        KeyCode::Backspace => {
            app.filter_input.pop();
        }
        KeyCode::Enter => apply_filter_input(app),
        _ => {}
    }
}

fn drop_last_filter(app: &mut App) {
    let Some(id) = app.session.cursor() else { return };
    if let Some(block) = app.session.block_mut(id) {
        if block.pipeline.pop().is_none() {
            app.flash = Some("no filters to drop".into());
        }
    }
}

fn apply_filter_input(app: &mut App) {
    let input = app.filter_input.trim().to_string();
    if input.is_empty() {
        app.focus = Focus::BlockCursor;
        return;
    }
    let spec = match parse_filter(&input) {
        Ok(s) => s,
        Err(e) => {
            app.flash = Some(format!("filter parse: {e}"));
            return;
        }
    };
    let Some(id) = app.session.cursor() else { return };
    if let Some(block) = app.session.block_mut(id) {
        block.pipeline.push(spec);
    }
    app.filter_input.clear();
    app.focus = Focus::BlockCursor;
}

fn parse_filter(s: &str) -> Result<FilterSpec, String> {
    let s = s.trim();
    if s == "from-lines" {
        return Ok(FilterSpec::FromLines);
    }
    if let Some(rest) = s.strip_prefix("where ") {
        let parts: Vec<&str> = rest.splitn(3, ' ').collect();
        if parts.len() == 3 && parts[1] == "matches" {
            return Ok(FilterSpec::Where {
                predicate: Predicate::Matches {
                    column: parts[0].to_string(),
                    pattern: parts[2].to_string(),
                },
            });
        }
        return Err("usage: where <column> matches <pattern>".into());
    }
    Err(format!("unknown filter: `{s}` (try `from-lines` or `where col matches pat`)"))
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, chunks[0]);
    draw_blocks(f, chunks[1], app);
    draw_input(f, chunks[2], app);
    draw_status(f, chunks[3], app);
}

fn draw_header(f: &mut Frame, area: Rect) {
    let header = Paragraph::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "shed",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" v0.0.0", Style::default().fg(Color::DarkGray)),
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
        let mut spans = vec![Span::raw("      ")];
        for (i, f) in block.pipeline.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
            }
            spans.push(Span::styled(
                describe_filter(f),
                Style::default().fg(Color::LightCyan),
            ));
        }
        lines.push(Line::from(spans));
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

fn render_block_preview(block: &Block) -> Vec<Line<'static>> {
    let Some(capture) = &block.capture else {
        return Vec::new();
    };
    if block.pipeline.is_empty() {
        return render_raw_lines(&capture.stdout);
    }
    let mut value = PipelineValue::Bytes(capture.stdout.clone());
    for filter in &block.pipeline {
        match filter.apply(value) {
            Ok(v) => value = v,
            Err(e) => {
                return vec![Line::from(vec![
                    Span::raw("      "),
                    Span::styled(
                        format!("filter error: {e}"),
                        Style::default().fg(Color::Red),
                    ),
                ])];
            }
        }
    }
    render_pipeline_value(value)
}

fn render_raw_lines(bytes: &bytes::Bytes) -> Vec<Line<'static>> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    let preview: Vec<&str> = text.lines().take(PREVIEW_LINES).collect();
    for line in &preview {
        out.push(Line::from(vec![
            Span::raw("      "),
            Span::raw(line.to_string()),
        ]));
    }
    let total = text.lines().count();
    if total > PREVIEW_LINES {
        out.push(Line::from(Span::styled(
            format!("      … {} more lines", total - PREVIEW_LINES),
            Style::default().fg(Color::DarkGray),
        )));
    }
    out
}

fn render_pipeline_value(value: PipelineValue) -> Vec<Line<'static>> {
    match value {
        PipelineValue::Bytes(b) => render_raw_lines(&b),
        PipelineValue::Structured(Value::List(items)) => {
            let mut out = Vec::new();
            let total = items.len();
            for item in items.iter().take(PREVIEW_LINES) {
                out.push(Line::from(vec![
                    Span::raw("      "),
                    Span::raw(format_record_or_value(item)),
                ]));
            }
            if total > PREVIEW_LINES {
                out.push(Line::from(Span::styled(
                    format!("      … {} more rows", total - PREVIEW_LINES),
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

fn format_record_or_value(v: &Value) -> String {
    match v {
        Value::Record(r) => {
            if r.len() == 1 {
                if let Some((_, Value::String(s))) = r.iter().next() {
                    return s.clone();
                }
            }
            let parts: Vec<String> = r
                .iter()
                .map(|(k, v)| format!("{k}={}", format_scalar(v)))
                .collect();
            parts.join("  ")
        }
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
        Focus::FilterAdd => Line::from(vec![
            Span::styled(
                "+ ",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(app.filter_input.clone()),
            Span::styled("▏", Style::default().fg(Color::Magenta)),
        ]),
    };
    let widget = Paragraph::new(line).block(border);
    f.render_widget(widget, area);
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
        Focus::FilterAdd => vec![
            ("Enter", "apply"),
            ("Esc", "cancel"),
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

    #[test]
    fn parse_from_lines() {
        assert!(matches!(parse_filter("from-lines"), Ok(FilterSpec::FromLines)));
        assert!(matches!(parse_filter("  from-lines  "), Ok(FilterSpec::FromLines)));
    }

    #[test]
    fn parse_where_matches() {
        let spec = parse_filter("where line matches ^err").unwrap();
        match spec {
            FilterSpec::Where {
                predicate: Predicate::Matches { column, pattern },
            } => {
                assert_eq!(column, "line");
                assert_eq!(pattern, "^err");
            }
            _ => panic!("wrong spec"),
        }
    }

    #[test]
    fn parse_where_with_spaces_in_pattern() {
        let spec = parse_filter("where line matches hello world").unwrap();
        match spec {
            FilterSpec::Where {
                predicate: Predicate::Matches { pattern, .. },
            } => assert_eq!(pattern, "hello world"),
            _ => panic!(),
        }
    }

    #[test]
    fn parse_unknown_filter_errors() {
        assert!(parse_filter("nope").is_err());
        assert!(parse_filter("where").is_err());
        assert!(parse_filter("where line").is_err());
    }
}
