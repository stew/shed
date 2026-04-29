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
use shed_core::{BlockState, Session};

use crate::exec::run_command;

const CAPTURE_CAP: usize = 16 * 1024 * 1024;
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const PREVIEW_LINES: usize = 8;

pub async fn run() -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = app_loop(&mut terminal).await;
    ratatui::restore();
    result
}

struct App {
    session: Session,
    prompt: String,
    quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            session: Session::new(),
            prompt: String::new(),
            quit: false,
        }
    }
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
                    handle_key(&mut app, key).await;
                }
            }
        }
    }
}

async fn handle_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') | KeyCode::Char('d') => app.quit = true,
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Char(c) => app.prompt.push(c),
        KeyCode::Backspace => {
            app.prompt.pop();
        }
        KeyCode::Enter => run_prompt(app).await,
        _ => {}
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
    draw_blocks(f, chunks[1], &app.session);
    draw_prompt(f, chunks[2], &app.prompt);
    draw_status(f, chunks[3]);
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

fn draw_blocks(f: &mut Frame, area: Rect, session: &Session) {
    let mut lines: Vec<Line> = Vec::new();
    for block in session.blocks() {
        let glyph = match &block.state {
            BlockState::Running => Span::styled("⏵", Style::default().fg(Color::Yellow)),
            BlockState::Done(0) => Span::styled("●", Style::default().fg(Color::Green)),
            BlockState::Done(_) => Span::styled("⚠", Style::default().fg(Color::Red)),
            BlockState::Snapshotted => Span::styled("❄", Style::default().fg(Color::LightBlue)),
            BlockState::Failed(_) => Span::styled("⚠", Style::default().fg(Color::Red)),
        };
        let id_span = Span::styled(
            format!("%{}", block.id.0),
            Style::default().fg(Color::Cyan),
        );
        let cmd_span = Span::raw(block.argv.join(" "));
        let mut header = vec![
            Span::raw("  "),
            id_span,
            Span::raw(" "),
            glyph,
            Span::raw(" "),
            cmd_span,
        ];
        if let Some(name) = &block.name {
            header.push(Span::styled(
                format!("  ◉ {name}"),
                Style::default().fg(Color::Magenta),
            ));
        }
        lines.push(Line::from(header));

        if let Some(capture) = &block.capture {
            let text = String::from_utf8_lossy(&capture.stdout);
            let preview: Vec<&str> = text.lines().take(PREVIEW_LINES).collect();
            for line in &preview {
                lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::raw(line.to_string()),
                ]));
            }
            let total = text.lines().count();
            if total > PREVIEW_LINES {
                lines.push(Line::from(Span::styled(
                    format!("      … {} more lines", total - PREVIEW_LINES),
                    Style::default().fg(Color::DarkGray),
                )));
            }
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
    }

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn draw_prompt(f: &mut Frame, area: Rect, prompt: &str) {
    let border = TuiBlock::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray));
    let widget = Paragraph::new(Line::from(vec![
        Span::styled(
            "▶ ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(prompt),
        Span::styled("▏", Style::default().fg(Color::Green)),
    ]))
    .block(border);
    f.render_widget(widget, area);
}

fn draw_status(f: &mut Frame, area: Rect) {
    let status = Paragraph::new(Line::from(vec![
        Span::styled(
            " Enter ",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ),
        Span::raw(" run  "),
        Span::styled(
            " Ctrl-D ",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ),
        Span::raw(" quit"),
    ]))
    .style(Style::default().bg(Color::DarkGray));
    f.render_widget(status, area);
}
