//! Rendering primitives for shed's TUI.
//!
//! This module currently holds the pure (App-free) rendering helpers:
//! - per-line / per-cell formatters for structured rows (`render_table`,
//!   `render_scalar_list`)
//! - the raw-bytes path that drives ANSI-colored output
//!   (`render_raw_lines`)
//! - the entry point that branches on `PipelineValue` shape
//!   (`render_pipeline_value`)
//! - a handful of one-liner helpers (`cell_string`, `pad_right`,
//!   `display_width`, `schema_of`, `format_scalar`, `filter_error_lines`,
//!   `render_note_lines`)
//!
//! The App-bound `draw_*` functions (modal screens, REPL chrome,
//! status bar, etc.) still live in `tui.rs`; they'll migrate here in
//! subsequent commits.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as TuiBlock, Borders, Paragraph, Wrap};
use shed_core::{
    CompareOp, FilterSpec, PipelineValue, Predicate, Shed, ShedState, SortDirection, Value,
};

use super::PREVIEW_LINES;
use super::{
    App, CellLayout, EnvInputMode, NotePosition, ansi, apply_pipeline, delim_label, draw_status,
    find_matches_regex, highlight_matches_in_line, matches_for_input, try_compile,
};

pub(super) fn filter_error_lines(message: &str) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::raw("      "),
        Span::styled(
            format!("filter error: {message}"),
            Style::default().fg(Color::Red),
        ),
    ])]
}

pub(super) fn render_raw_lines(bytes: &bytes::Bytes, max: usize, tail: bool) -> Vec<Line<'static>> {
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

pub(super) fn render_pipeline_value(
    value: PipelineValue,
    tail: bool,
    cells: &mut Vec<CellLayout>,
) -> Vec<Line<'static>> {
    render_pipeline_value_with_max(value, PREVIEW_LINES, tail, cells)
}

pub(super) fn render_pipeline_value_with_max(
    value: PipelineValue,
    max: usize,
    tail: bool,
    cells: &mut Vec<CellLayout>,
) -> Vec<Line<'static>> {
    match value {
        PipelineValue::Bytes(b) => render_raw_lines(&b, max, tail),
        PipelineValue::Structured(Value::List(items)) => {
            let columns = schema_of(&items);
            if columns.is_empty() {
                render_scalar_list(&items, max, tail)
            } else {
                render_table(&items, &columns, max, tail, cells)
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
pub(super) fn render_table(
    items: &[Value],
    columns: &[String],
    max_rows: usize,
    tail: bool,
    cells: &mut Vec<CellLayout>,
) -> Vec<Line<'static>> {
    // Leading indent before the first column (matches `Span::raw("      ")`
    // below). Used to compute x_offset for cell hit-testing.
    const INDENT: u16 = 6;
    // Width of the column separator (" │ "). Three display cells.
    const SEP: u16 = 3;

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

    // Precompute per-column x_offset (start column within the line) so
    // each data row can register cell rects in one pass.
    let mut col_x: Vec<u16> = Vec::with_capacity(columns.len());
    let mut x = INDENT;
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            x = x.saturating_add(SEP);
        }
        col_x.push(x);
        x = x.saturating_add(*w as u16);
    }

    // Data rows: head or tail slice.
    let slice: Box<dyn Iterator<Item = &Value>> = if tail && truncated {
        Box::new(items.iter().skip(total - max_rows))
    } else {
        Box::new(items.iter().take(max_rows))
    };
    for item in slice {
        let row_line_idx = lines.len();
        let mut row_spans = vec![Span::raw("      ")];
        for (i, col) in columns.iter().enumerate() {
            if i > 0 {
                row_spans.push(Span::styled(" │ ", dim));
            }
            let value = match item {
                Value::Record(r) => r.get(col).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            };
            let cell = cell_string(&value);
            row_spans.push(Span::raw(pad_right(&cell, widths[i])));
            cells.push(CellLayout {
                line_idx: row_line_idx,
                x_offset: col_x[i],
                width: widths[i] as u16,
                value,
            });
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

pub(super) fn compute_column_widths(items: &[Value], columns: &[String]) -> Vec<usize> {
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

pub(super) fn render_scalar_list(items: &[Value], max: usize, tail: bool) -> Vec<Line<'static>> {
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

pub(super) fn cell_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        other => format_scalar(other),
    }
}

pub(super) fn pad_right(s: &str, width: usize) -> String {
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

pub(super) fn display_width(s: &str) -> usize {
    // ASCII-ish approximation. Wide CJK / emoji would need unicode-width
    // for true cell counting; for shed's typical PTY output (ASCII-heavy)
    // this is fine.
    s.chars().count()
}

pub(super) fn schema_of(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .find_map(|v| match v {
            Value::Record(r) => Some(r.keys().cloned().collect()),
            _ => None,
        })
        .unwrap_or_default()
}

pub(super) fn format_scalar(v: &Value) -> String {
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

/// Render a note (the free-form text attached as pre/post on a shed).
/// Dimmed and italicised with a `▎` left edge so it's visually distinct
/// from command output.
pub(super) fn render_note_lines(text: &str) -> Vec<Line<'static>> {
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

/// Render the body of a single shed.
///
/// The body comprises: an optional pre-text note, the command +
/// pipeline summary (only in EditShed focus), the pipeline-applied
/// output (table / scalar list / raw bytes), truncation + exit-code
/// annotations, the "Space to run" hint for idle sheds, and the
/// optional post-text note. `cells` accumulates per-cell layout entries
/// for right-click hit-testing — caller passes a fresh
/// `Vec<CellLayout>` per shed.
pub(super) fn render_shed(
    shed: &Shed,
    selected: bool,
    editing: bool,
    pipeline_cursor: Option<usize>,
    command_focused: bool,
    cells: &mut Vec<CellLayout>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if let Some(text) = shed.pre_text.as_deref() {
        lines.extend(render_note_lines(text));
    }

    // Compute pipeline outcome up-front so we can show inline drop counts.
    let pipeline_outcome = shed
        .capture
        .as_ref()
        .map(|c| apply_pipeline(c, &shed.pipeline));
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
        let effective_cursor = if command_focused {
            None
        } else {
            pipeline_cursor
        };
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
        Some(Ok((value, _))) => {
            // render_pipeline_value's cells track line_idx relative to its
            // own output (which is `render_table`'s line vector). Offset by
            // the number of lines already in this shed body so cells end
            // up in body-relative coordinates.
            let body_offset = lines.len();
            let before = cells.len();
            lines.extend(render_pipeline_value(value, tail, cells));
            for cell in &mut cells[before..] {
                cell.line_idx += body_offset;
            }
        }
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
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Space",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to run", Style::default().fg(Color::Cyan)),
        ]));
    }

    if let Some(text) = shed.post_text.as_deref() {
        lines.extend(render_note_lines(text));
    }

    lines
}

/// Human-readable label for a filter — used in EditShed's per-filter
/// row, in the alias-manage view, and in pipeline previews. Mirrors
/// what `Notebook::from_session` would persist, minus the
/// data/parameter encoding.
pub(super) fn describe_filter(spec: &FilterSpec) -> String {
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

pub(super) fn describe_predicate(p: &Predicate) -> String {
    match p {
        Predicate::Matches { column, pattern } => format!("{column} matches {pattern}"),
        Predicate::Contains { column, substring } => format!("{column} contains {substring}"),
        Predicate::Compare { column, op, value } => {
            format!(
                "{column} {} {}",
                describe_compare_op(*op),
                describe_compare_value(value)
            )
        }
        Predicate::And(a, b) => format!("({} && {})", describe_predicate(a), describe_predicate(b)),
        Predicate::Or(a, b) => format!("({} || {})", describe_predicate(a), describe_predicate(b)),
        Predicate::Not(p) => format!("!{}", describe_predicate(p)),
    }
}

pub(super) fn describe_compare_op(op: CompareOp) -> &'static str {
    match op {
        CompareOp::Eq => "=",
        CompareOp::Ne => "≠",
        CompareOp::Lt => "<",
        CompareOp::Le => "≤",
        CompareOp::Gt => ">",
        CompareOp::Ge => "≥",
    }
}

pub(super) fn describe_compare_value(v: &Value) -> String {
    match v {
        Value::String(s) => format!("\"{s}\""),
        _ => format_scalar(v),
    }
}

/// One-line header shown at the top of every modal screen
/// (FilterEdit, ShedExpand, EnvEdit, Palette, NoteEdit, AliasManage).
/// Combines a `shed` banner with the screen's title.
pub(super) fn draw_header(f: &mut Frame, area: Rect, title: &str) {
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

/// Render the floating right-click context menu over the existing
/// frame. Sized to the longest item label + padding; shifted inward if
/// it would overflow the right or bottom edge.
pub(super) fn draw_context_menu(f: &mut Frame, app: &App) {
    let Some(menu) = app.context_menu.as_ref() else {
        return;
    };
    if menu.items.is_empty() {
        return;
    }
    let frame = f.area();
    let inner_width: u16 = menu
        .items
        .iter()
        .map(|i| i.label.chars().count() as u16)
        .max()
        .unwrap_or(1);
    // 2 borders + 2 padding cells on each side.
    let width = (inner_width + 4).min(frame.width.max(1));
    let height = (menu.items.len() as u16 + 2).min(frame.height.max(1));
    let mut x = menu.pos.0;
    let mut y = menu.pos.1;
    if x + width > frame.x + frame.width {
        x = frame.x + frame.width - width;
    }
    if y + height > frame.y + frame.height {
        y = frame.y + frame.height - height;
    }
    let area = Rect {
        x,
        y,
        width,
        height,
    };

    let block = TuiBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(area);
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(block, area);

    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(Color::White);
    let lines: Vec<Line<'static>> = menu
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == menu.selected {
                highlight
            } else {
                normal
            };
            Line::from(Span::styled(format!(" {} ", item.label), style))
        })
        .collect();
    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

pub(super) fn draw_alias_manage(f: &mut Frame, app: &App) {
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

pub(super) fn draw_note_edit(f: &mut Frame, app: &App) {
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
        let mut prefixed = vec![Span::styled("▎ ", Style::default().fg(Color::DarkGray))];
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

pub(super) fn draw_env_edit(f: &mut Frame, app: &App) {
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

pub(super) fn draw_palette(f: &mut Frame, app: &App) {
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
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
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

pub(super) fn draw_shed_expand(f: &mut Frame, app: &App) {
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

    let all_lines: Vec<Line<'static>> = match shed.capture.as_ref() {
        Some(capture) => match apply_pipeline(capture, &shed.pipeline) {
            Ok((value, _drops)) => {
                render_pipeline_value_with_max(value, usize::MAX, false, &mut Vec::new())
            }
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
        let flags = if app.search_case_insensitive {
            " (i)"
        } else {
            ""
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn render_table_tail_puts_more_indicator_at_top_and_shows_last_rows() {
        let cols = vec!["n".to_string()];
        let items: Vec<Value> = (1..=10)
            .map(|i| {
                let mut r = indexmap::IndexMap::new();
                r.insert("n".to_string(), Value::Int(i));
                Value::Record(r)
            })
            .collect();
        let lines = render_table(&items, &cols, 3, true, &mut Vec::new());
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
}
