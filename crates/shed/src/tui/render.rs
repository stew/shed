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

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use shed_core::{PipelineValue, Value};

use super::PREVIEW_LINES;
use super::{CellLayout, ansi};

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
