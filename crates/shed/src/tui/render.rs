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
    App, BodyRegion, CellLayout, CellRegion, ClickAction, ClickRegion, EditMode, EnvInputMode,
    ExitPrompt, FilterEditState, Focus, FormField, InputKind, MAX_SORT_KEYS, NotePosition, WhereOp,
    ansi, apply_pipeline, delim_label, filter_edit_field_hints, find_matches_regex,
    highlight_matches_in_line, input_spans_with_cursor, line_plain_text, matches_for_input,
    render_input_bar, tabs::draw_tab_bar, try_compile,
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
            let cell = cell_display(&value);
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
                    let w = display_width(&cell_display(v));
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
        let text = match item {
            Value::DateTime(ts) => humanize_timestamp(*ts),
            other => format_scalar(other),
        };
        out.push(Line::from(vec![Span::raw("      "), Span::raw(text)]));
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
        // Canonical RFC 3339 form — stable for copying, export, and
        // column widths. The relative "3 minutes ago" form is applied
        // separately by `cell_display` at table-render time.
        Value::DateTime(ts) => ts.to_string(),
        Value::Bytes(b) => format!("<{} bytes>", b.len()),
        Value::List(_) | Value::Record(_) => format!("{v:?}"),
    }
}

/// Display form of a cell for the inline table: same as [`cell_string`]
/// except [`Value::DateTime`] renders as relative time ("3 minutes
/// ago"). Recomputed every frame, so the relative text ticks live.
pub(super) fn cell_display(v: &Value) -> String {
    match v {
        Value::DateTime(ts) => humanize_timestamp(*ts),
        other => cell_string(other),
    }
}

/// Render an absolute instant as a coarse relative phrase against now —
/// "just now", "3 minutes ago", "in 2 hours", "5 days ago".
pub(super) fn humanize_timestamp(ts: jiff::Timestamp) -> String {
    let delta = jiff::Timestamp::now().as_second() - ts.as_second();
    let (ago, secs) = (delta >= 0, delta.unsigned_abs());
    let (n, unit) = if secs < 45 {
        return "just now".to_string();
    } else if secs < 90 {
        (1, "minute")
    } else if secs < 3600 {
        (secs / 60, "minute")
    } else if secs < 86_400 {
        (secs / 3600, "hour")
    } else if secs < 2_592_000 {
        (secs / 86_400, "day")
    } else if secs < 31_536_000 {
        (secs / 2_592_000, "month")
    } else {
        (secs / 31_536_000, "year")
    };
    let plural = if n == 1 { "" } else { "s" };
    if ago {
        format!("{n} {unit}{plural} ago")
    } else {
        format!("in {n} {unit}{plural}")
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
#[allow(clippy::too_many_arguments)]
pub(super) fn render_shed(
    shed: &Shed,
    selected: bool,
    editing: bool,
    pipeline_cursor: Option<usize>,
    command_focused: bool,
    output_cursor: Option<usize>,
    output_cap: usize,
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

        // Outputs section — visible whenever editing, even if empty
        // (the `+ add output` slot is the discoverability hook). The
        // cursor moves into this section when output_cursor is Some.
        let outputs_heading = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD);
        lines.push(Line::from(vec![
            Span::raw(indent),
            Span::styled("outputs:", outputs_heading),
        ]));
        let value_dim = Style::default().fg(Color::Green);
        for (i, (name, spec)) in shed.outputs.iter().enumerate() {
            let style = if output_cursor == Some(i) {
                highlight
            } else {
                Style::default().fg(Color::LightCyan)
            };
            let spec_str = match spec {
                shed_core::OutputSpec::TempPath => "TempPath".to_string(),
                shed_core::OutputSpec::Literal(v) => format!("\"{v}\""),
            };
            let mut spans = vec![
                Span::raw(indent),
                Span::styled("│ ", dim),
                Span::styled(format!(" {name} = {spec_str} "), style),
            ];
            if let Some(value) = shed.output_values.get(name)
                && !matches!(spec, shed_core::OutputSpec::Literal(_))
            {
                spans.push(Span::styled(format!("  → {value}"), value_dim));
            }
            lines.push(Line::from(spans));
        }
        let add_style = if output_cursor == Some(shed.outputs.len()) {
            highlight
        } else {
            dim
        };
        lines.push(Line::from(vec![
            Span::raw(indent),
            Span::styled("│ ", dim),
            Span::styled(" + add output ", add_style),
        ]));
    }

    // Output is always tailed: when it overflows `output_cap`, the
    // oldest lines drop off the top and a "N more" marker is pinned
    // there. The selected shed passes a huge `output_cap` so its whole
    // output is rendered (then windowed + scrolled by the caller);
    // unselected sheds pass PREVIEW_LINES for a compact tail preview.
    match pipeline_outcome {
        Some(Ok((value, _))) => {
            // render_pipeline_value_with_max's cells track line_idx
            // relative to its own output (`render_table`'s line vector).
            // Offset by the number of lines already in this shed body so
            // cells end up in body-relative coordinates.
            let body_offset = lines.len();
            let before = cells.len();
            lines.extend(render_pipeline_value_with_max(
                value, output_cap, true, cells,
            ));
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
        if let Some(code) = capture.exit_code
            && code != 0
        {
            lines.push(Line::from(Span::styled(
                format!(" exit {code}"),
                Style::default().fg(Color::Red),
            )));
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
        FilterSpec::ParseTime { columns } => {
            format!("parse-time {}", columns.join(", "))
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

/// Top-level dispatcher: routes the current frame to the right
/// per-focus renderer (modals get full-screen replacements; the
/// non-modal focuses share the main REPL view), then paints the
/// context menu overlay on top if one is open.
pub(super) fn draw(
    f: &mut Frame,
    app: &App,
    regions: &mut Vec<ClickRegion>,
    bodies: &mut Vec<BodyRegion>,
) {
    match app.focus {
        Focus::FilterEdit => draw_filter_edit(f, app),
        Focus::ShedExpand => draw_shed_expand(f, app),
        Focus::EnvEdit => draw_env_edit(f, app),
        Focus::Palette => draw_palette(f, app),
        Focus::NoteEdit => draw_note_edit(f, app),
        Focus::AliasManage => draw_alias_manage(f, app),
        _ => draw_repl(f, app, regions, bodies),
    }
    if app.context_menu.is_some() {
        draw_context_menu(f, app);
    }
}

pub(super) fn draw_repl(
    f: &mut Frame,
    app: &App,
    regions: &mut Vec<ClickRegion>,
    bodies: &mut Vec<BodyRegion>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tab bar (now also holds cwd on the right)
            Constraint::Min(1),    // sheds
            Constraint::Length(1), // status
        ])
        .split(f.area());

    draw_tab_bar(f, chunks[0], app, regions);
    draw_sheds(f, chunks[1], app, regions, bodies);
    draw_status(f, chunks[2], app);
}

/// Decide which contiguous range of sheds is visible and the box height
/// to give each one. With a selection the window anchors on the selected
/// shed — it claims up to the whole pane (`avail`) and neighbours fill
/// the rest, preferring more-recent (downward) sheds; without one it is
/// the most-recent suffix that fits.
///
/// Returns `(start, end, layout_heights)`: the visible range is
/// `[start, end)` and `layout_heights[i]` is the box height for shed
/// `i` (it only differs from `heights[i]` for the expanded selected
/// shed, which is clamped to `avail`).
fn visible_shed_layout(
    heights: &[u16],
    avail: u16,
    sel_idx: Option<usize>,
) -> (usize, usize, Vec<u16>) {
    let n = heights.len();
    let mut layout_h = heights.to_vec();
    match sel_idx {
        Some(sel) => {
            // The selected shed claims min(full height, whole pane).
            let sel_box = heights[sel].min(avail);
            layout_h[sel] = sel_box;
            let mut used = sel_box;
            let mut start = sel;
            let mut end = sel + 1;
            // Grow the window outward, preferring more-recent sheds
            // (downward) over older ones, until neither side fits.
            loop {
                let mut progressed = false;
                if end < n && used.saturating_add(heights[end]) <= avail {
                    used = used.saturating_add(heights[end]);
                    end += 1;
                    progressed = true;
                }
                if start > 0 && used.saturating_add(heights[start - 1]) <= avail {
                    start -= 1;
                    used = used.saturating_add(heights[start]);
                    progressed = true;
                }
                if !progressed {
                    break;
                }
            }
            (start, end, layout_h)
        }
        None => {
            let mut total: u16 = 0;
            let mut start = n;
            for i in (0..n).rev() {
                if total.saturating_add(heights[i]) > avail {
                    break;
                }
                total = total.saturating_add(heights[i]);
                start = i;
            }
            (start, n, layout_h)
        }
    }
}

fn draw_sheds(
    f: &mut Frame,
    area: Rect,
    app: &App,
    regions: &mut Vec<ClickRegion>,
    bodies: &mut Vec<BodyRegion>,
) {
    let cursor_id = app.session.cursor();
    let cursor_visible = matches!(app.focus, Focus::ShedCursor | Focus::EditShed);
    let sheds: Vec<&shed_core::Shed> = app.session.sheds().collect();

    let scratch_height: u16 = 3;
    let avail = area.height.saturating_sub(scratch_height);

    // Render each shed's interior content + record selection state. The
    // selected shed renders its *whole* output (output_cap = MAX) so its
    // box can expand to fill the pane and the body becomes scrollable;
    // unselected sheds get a compact PREVIEW_LINES tail.
    let mut renders: Vec<(Vec<Line<'static>>, bool, bool, Vec<CellLayout>)> =
        Vec::with_capacity(sheds.len());
    let mut sel_idx: Option<usize> = None;
    for (idx, shed) in sheds.iter().enumerate() {
        let selected = cursor_visible && cursor_id == Some(shed.id);
        if selected {
            sel_idx = Some(idx);
        }
        let editing = selected && app.focus == Focus::EditShed;
        // When the cursor is on argv (command_focused) or in the outputs
        // section (output_cursor is Some), the pipeline cursor should
        // render as None so no filter wears the active-magenta highlight.
        let on_outputs = app.output_cursor.is_some();
        let pipeline_cursor = if editing && !on_outputs {
            Some(app.pipeline_cursor)
        } else {
            None
        };
        let command_focused = editing && app.command_focused;
        let output_cursor = if editing { app.output_cursor } else { None };
        let output_cap = if selected { usize::MAX } else { PREVIEW_LINES };
        let mut shed_cells: Vec<CellLayout> = Vec::new();
        let lines = render_shed(
            shed,
            selected,
            editing,
            pipeline_cursor,
            command_focused,
            output_cursor,
            output_cap,
            &mut shed_cells,
        );
        renders.push((lines, selected, editing, shed_cells));
    }

    // Natural box height each shed wants: its content + 2 border rows.
    let heights: Vec<u16> = renders
        .iter()
        .map(|(l, _, _, _)| (l.len() as u16).saturating_add(2))
        .collect();

    let (start, end, layout_h) = visible_shed_layout(&heights, avail, sel_idx);

    let visible = &renders[start..end];
    let mut constraints: Vec<Constraint> = Vec::with_capacity(visible.len() + 2);
    for h in &layout_h[start..end] {
        constraints.push(Constraint::Length(*h));
    }
    constraints.push(Constraint::Min(0));
    constraints.push(Constraint::Length(scratch_height));

    let rects = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    for (i, (lines, selected, editing, shed_cells)) in visible.iter().enumerate() {
        let shed = sheds[start + i];
        // The selected shed's body is windowed: by default it shows the
        // tail (bottom), and `cursor_body_scroll` lifts the window up.
        let scroll = if *selected {
            app.body_scroll_for(shed.id)
        } else {
            0
        };
        draw_one_shed(
            f, rects[i], shed, lines, *selected, *editing, scroll, regions, bodies, shed_cells,
        );
    }
    let scratch_rect = rects[rects.len() - 1];
    draw_scratch_box(f, scratch_rect, app);
}

/// Render the always-present "scratch" / prompt box at the end of the
/// shed list. When focus is `Prompt`, the buffer is rendered inside the
/// box with a cursor; otherwise the box shows a hint inviting the user
/// to press `/` (or scroll down) to start typing.
fn draw_scratch_box(f: &mut Frame, area: Rect, app: &App) {
    let active = app.focus == Focus::Prompt;
    let selected = !active && app.focus == Focus::ShedCursor && app.session.cursor().is_none();

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
#[allow(clippy::too_many_arguments)]
fn draw_one_shed(
    f: &mut Frame,
    area: Rect,
    shed: &shed_core::Shed,
    lines: &[Line<'static>],
    selected: bool,
    editing: bool,
    scroll: usize,
    regions: &mut Vec<ClickRegion>,
    bodies: &mut Vec<BodyRegion>,
    shed_cells: &[CellLayout],
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
    let argv_text = shed.argv.join(" ");
    let title = Line::from(vec![
        Span::styled(id_text, id_style),
        Span::raw(" "),
        glyph,
        Span::raw(" "),
        Span::styled(argv_text, Style::default().fg(Color::DarkGray)),
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

    // Window the body to the box's interior height. By default the
    // *tail* is shown (oldest lines scroll off the top); `scroll` lifts
    // that window upward. Only the selected shed is ever taller than its
    // box, so non-selected sheds window to the identity slice.
    //
    // EditShed is the exception: the command + filter rows it edits sit
    // at the *top* of the body, so the window anchors there regardless
    // of scroll — otherwise a long output would push the editable rows
    // off-screen the moment you press `e`.
    let visible_h = area.height.saturating_sub(2) as usize;
    let content_h = lines.len();
    let max_scroll = content_h.saturating_sub(visible_h);
    let lift = scroll.min(max_scroll);
    let top = if editing { 0 } else { max_scroll - lift };
    let bottom = top.saturating_add(visible_h).min(content_h);
    let windowed: Vec<Line<'static>> = lines[top..bottom].to_vec();

    // When the body overflows, annotate the bottom border with the
    // visible line range so the scroll position is legible.
    let mut widget = TuiBlock::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    if content_h > visible_h && visible_h > 0 {
        let marker = format!(" {}–{}/{} ", top + 1, bottom, content_h);
        widget = widget.title_bottom(
            Line::from(Span::styled(marker, Style::default().fg(Color::DarkGray))).right_aligned(),
        );
    }
    let inner = widget.inner(area);
    f.render_widget(widget, area);
    let para = Paragraph::new(windowed.clone()).wrap(Wrap { trim: false });
    f.render_widget(para, inner);

    if inner.width > 0 && inner.height > 0 {
        let plain: Vec<String> = windowed.iter().map(line_plain_text).collect();
        let cell_regions: Vec<CellRegion> = shed_cells
            .iter()
            .filter_map(|cell| {
                // Cell line indices are body-relative; shift into the
                // visible window and drop anything scrolled out of view.
                let screen_row = cell.line_idx.checked_sub(top)?;
                if screen_row >= visible_h {
                    return None;
                }
                let abs_x = inner.x.checked_add(cell.x_offset)?;
                let abs_y = inner.y.checked_add(screen_row as u16)?;
                if abs_x >= inner.x.saturating_add(inner.width) {
                    return None;
                }
                if abs_y >= inner.y.saturating_add(inner.height) {
                    return None;
                }
                let max_w = inner.x.saturating_add(inner.width).saturating_sub(abs_x);
                let width = cell.width.min(max_w);
                if width == 0 {
                    return None;
                }
                Some(CellRegion {
                    rect: Rect {
                        x: abs_x,
                        y: abs_y,
                        width,
                        height: 1,
                    },
                    value: cell.value.clone(),
                })
            })
            .collect();
        bodies.push(BodyRegion {
            rect: inner,
            shed_id: shed.id,
            lines: plain,
            cells: cell_regions,
        });
    }

    let close_width: u16 = 3;
    let min_room: u16 = 6; // corner + 1 padding + title room + close + corner
    if area.width >= min_room {
        let close_x = area.right().saturating_sub(close_width + 1);
        let buf = f.buffer_mut();
        let close_style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
        buf.set_string(close_x, area.y, "[×]", close_style);
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

pub(super) fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    if let Some(prompt) = app.exit_prompt
        && prompt == ExitPrompt::Confirm
    {
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
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "[n]o",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "[c]ancel",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .style(Style::default().bg(Color::DarkGray));
        f.render_widget(widget, area);
        return;
    }
    if let Some(id) = app.delete_confirm {
        let widget = Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!("%{} is still running — delete anyway?", id.0),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "[y]es",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
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
    // AwaitingPath falls through; the save_input_mode bar takes over.
    if app.is_input(InputKind::Save) {
        f.render_widget(
            render_input_bar(
                "save to: ",
                Color::Green,
                app.input_text(),
                app.input_cursor(),
            ),
            area,
        );
        return;
    }
    if app.is_input(InputKind::Open) {
        f.render_widget(
            render_input_bar("open: ", Color::Green, app.input_text(), app.input_cursor()),
            area,
        );
        return;
    }
    if app.is_input(InputKind::Rerun) {
        f.render_widget(
            render_input_bar(
                "rerun: ",
                Color::LightCyan,
                app.input_text(),
                app.input_cursor(),
            ),
            area,
        );
        return;
    }
    if app.is_input(InputKind::CmdEdit) {
        f.render_widget(
            render_input_bar(
                "edit cmd: ",
                Color::LightMagenta,
                app.input_text(),
                app.input_cursor(),
            ),
            area,
        );
        return;
    }
    if app.is_input(InputKind::AliasInvoke) {
        f.render_widget(
            render_input_bar(
                "alias: ",
                Color::LightMagenta,
                app.input_text(),
                app.input_cursor(),
            ),
            area,
        );
        return;
    }
    if app.is_input(InputKind::RenameTab) {
        f.render_widget(
            render_input_bar(
                "tab name: ",
                Color::Cyan,
                app.input_text(),
                app.input_cursor(),
            ),
            area,
        );
        return;
    }
    if app.is_input(InputKind::OutputSpec) {
        f.render_widget(
            render_input_bar(
                "output (name=TempPath or name=value): ",
                Color::Cyan,
                app.input_text(),
                app.input_cursor(),
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
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
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
    if app.is_input(InputKind::AliasName) {
        f.render_widget(
            render_input_bar(
                "alias name: ",
                Color::LightMagenta,
                app.input_text(),
                app.input_cursor(),
            ),
            area,
        );
        return;
    }
    if app.is_input(InputKind::Pin) {
        f.render_widget(
            render_input_bar(
                "pin name: ",
                Color::LightMagenta,
                app.input_text(),
                app.input_cursor(),
            ),
            area,
        );
        return;
    }
    if app.is_input(InputKind::Write) {
        f.render_widget(
            render_input_bar(
                "write to: ",
                Color::Yellow,
                app.input_text(),
                app.input_cursor(),
            ),
            area,
        );
        return;
    }
    if app.is_input(InputKind::Search) {
        let invalid = !app.input_text().is_empty()
            && try_compile(app.input_text(), app.search_case_insensitive).is_none();
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
            app.input_text(),
            app.input_cursor(),
            Color::Yellow,
        ));
        if app.search_case_insensitive {
            spans.push(Span::styled("  (i)", Style::default().fg(Color::DarkGray)));
        }
        if invalid {
            spans.push(Span::styled(
                "  (invalid regex)",
                Style::default().fg(Color::Red),
            ));
        }
        let widget = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
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
            ("@name | %N", "snapshot"),
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
        Focus::EditShed if app.command_focused => {
            vec![("↓", "filters"), ("f / Enter", "edit cmd"), ("Esc", "back")]
        }
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
    let widget = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    f.render_widget(widget, area);
}

/// Build the pipeline as it would be with the in-progress filter
/// applied (added / inserted / replaced), and run it. Used by the
/// FilterEdit preview pane to show the live effect of the form's
/// current values.
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
    Some(apply_pipeline(capture, &hypothetical))
}

pub(super) fn draw_filter_edit(f: &mut Frame, app: &App) {
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
        Some(Ok((value, _))) => {
            render_pipeline_value_with_max(value.clone(), max, false, &mut Vec::new())
        }
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
        if state.mode == EditMode::Insert(i) {
            let n = drops.get(i).copied().unwrap_or(0);
            let mut spans = vec![
                Span::styled("  ▸ ", edit_style),
                Span::styled(active_label.clone(), edit_style),
                Span::styled(format!("  ← inserting before {}", circled(i + 1)), dim),
            ];
            if n > 0 {
                spans.push(Span::styled(format!("  ⓘ -{n} (type mismatch)"), warn));
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
        let drops_idx = match state.mode {
            EditMode::Insert(j) if i >= j => i + 1,
            _ => i,
        };
        let n = drops.get(drops_idx).copied().unwrap_or(0);
        if n > 0 {
            spans.push(Span::styled(format!("  ⓘ -{n} (type mismatch)"), warn));
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
            spans.push(Span::styled(format!("  ⓘ -{n} (type mismatch)"), warn));
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
            Span::styled(col.to_string(), from_style),
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
        FormField::Kind => render_select_value(state.kind.name(), state.kind.description(), active),
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
            if state.csv_has_header {
                "true"
            } else {
                "false"
            },
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
            return Line::from("");
        }
        FormField::WhereCombine => render_select_value(
            state.where_combine.name(),
            state.where_combine.description(),
            active,
        ),
        FormField::WhereClauseSelect => {
            let total = state.where_clauses.len();
            let label = format!("{} of {}", circled(state.where_active_clause + 1), total,);
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
        let check = if state.column_selections[i] {
            "☑"
        } else {
            "☐"
        };
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

    #[test]
    fn visible_shed_layout_selection_at_index_0_does_not_overflow() {
        // Regression: the upward-expansion arm used to index
        // `heights[start - 1]` *after* decrementing `start`, so reaching
        // shed 0 computed `heights[usize::MAX]` and panicked.
        let heights = vec![5u16, 5, 5, 5];
        let (start, end, layout) = visible_shed_layout(&heights, 40, Some(0));
        assert_eq!(start, 0);
        assert_eq!(end, 4);
        assert_eq!(layout.len(), 4);
    }

    #[test]
    fn visible_shed_layout_expands_selected_into_the_pane() {
        // Selected shed wants 100 rows but the pane is 30 — it's clamped
        // to 30 and claims the whole window, no room for neighbours.
        let heights = vec![5u16, 100, 5];
        let (start, end, layout) = visible_shed_layout(&heights, 30, Some(1));
        assert_eq!((start, end), (1, 2));
        assert_eq!(layout[1], 30);
    }

    #[test]
    fn visible_shed_layout_no_selection_is_recent_suffix() {
        // 4 sheds of height 10, pane fits 3 → newest three visible.
        let heights = vec![10u16, 10, 10, 10];
        let (start, end, _) = visible_shed_layout(&heights, 30, None);
        assert_eq!((start, end), (1, 4));
    }

    #[test]
    fn visible_shed_layout_single_shed() {
        let heights = vec![6u16];
        let (start, end, layout) = visible_shed_layout(&heights, 40, Some(0));
        assert_eq!((start, end), (0, 1));
        assert_eq!(layout[0], 6);
    }

    #[test]
    fn humanize_timestamp_renders_relative_phrases() {
        let now = jiff::Timestamp::now().as_second();
        let at = |secs: i64| jiff::Timestamp::from_second(now + secs).unwrap();
        assert_eq!(humanize_timestamp(at(-5)), "just now");
        assert_eq!(humanize_timestamp(at(-180)), "3 minutes ago");
        assert_eq!(humanize_timestamp(at(-3600)), "1 hour ago");
        assert_eq!(humanize_timestamp(at(-2 * 86_400)), "2 days ago");
        // A future instant reads as "in …".
        assert_eq!(humanize_timestamp(at(600)), "in 10 minutes");
    }
}
