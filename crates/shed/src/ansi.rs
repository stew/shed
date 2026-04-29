//! ANSI escape parser for rendering captured PTY output.
//!
//! Programs running under shed's PTY emit ANSI escape sequences for color
//! (SGR), cursor positioning, line clears, alt-screen, etc. This module
//! walks the byte stream via `vte` and produces `ratatui::text::Line`s with
//! `Span`s carrying the corresponding `Style`.
//!
//! Internally each line is a `Vec<(char, Style)>` with a cursor column,
//! so that single-line cursor effects work:
//!
//! - **SGR** (CSI `m`): foreground/background colors (8-color, bright,
//!   8-bit indexed, 24-bit RGB), bold, dim, italic, underlined, reversed,
//!   crossed-out.
//! - **`\r`** (carriage return): resets the column to 0, so subsequent
//!   prints overwrite earlier characters on the same line. This is what
//!   makes cargo's `Building (10%) … (100%)` progress bar collapse to
//!   only the final state in the captured block.
//! - **`\x1b[K`** (Erase in Line): truncates from cursor to end (param 0
//!   or default), erases from start to cursor (param 1), or clears the
//!   whole line (param 2). Combined with `\r` this is the standard
//!   "rewrite this line" idiom.
//! - **`\n`**: emits the current line into scrollback and resets.
//! - **`\t`**: pads to the next tab stop (every 8 columns).
//!
//! What we drop:
//! - Cursor up/down/left/right and absolute positioning. Multi-line cursor
//!   manipulation needs full screen state, which is the next step beyond
//!   v0. Programs that need it (`top`, `vim`, …) trigger fullscreen
//!   handover instead.
//! - Scroll regions, alt-screen requests (those auto-trigger handover),
//!   OSC sequences (window titles, hyperlinks, …).

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use vte::{Params, Parser, Perform};

/// Parse `bytes` as a stream of text + ANSI escape sequences and produce
/// styled `Line`s suitable for ratatui rendering.
pub fn parse_to_lines(bytes: &[u8], indent: &str, max_lines: usize) -> ParsedPreview {
    let mut performer = AnsiToLines::new(indent.to_string());
    let mut parser = Parser::new();
    for &b in bytes {
        parser.advance(&mut performer, &[b]);
    }
    performer.finalize();

    let total = performer.lines.len();
    let truncated = total > max_lines;
    let lines: Vec<Line<'static>> = performer
        .lines
        .into_iter()
        .take(max_lines)
        .collect();
    ParsedPreview {
        lines,
        total,
        truncated,
    }
}

pub struct ParsedPreview {
    pub lines: Vec<Line<'static>>,
    pub total: usize,
    pub truncated: bool,
}

struct AnsiToLines {
    indent: String,
    lines: Vec<Line<'static>>,
    current_line: Vec<(char, Style)>,
    cursor_col: usize,
    current_style: Style,
}

impl AnsiToLines {
    fn new(indent: String) -> Self {
        Self {
            indent,
            lines: Vec::new(),
            current_line: Vec::new(),
            cursor_col: 0,
            current_style: Style::default(),
        }
    }

    fn finish_line(&mut self) {
        let line = self.build_line();
        self.lines.push(line);
        self.current_line.clear();
        self.cursor_col = 0;
    }

    /// Collapse the current cell array into a `Line` by merging
    /// consecutive cells with the same `Style` into a single styled span.
    fn build_line(&self) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if !self.indent.is_empty() {
            spans.push(Span::raw(self.indent.clone()));
        }
        let mut run = String::new();
        let mut run_style: Option<Style> = None;
        for (ch, style) in &self.current_line {
            if Some(*style) == run_style {
                run.push(*ch);
            } else {
                if let Some(prev) = run_style {
                    spans.push(Span::styled(std::mem::take(&mut run), prev));
                }
                run.push(*ch);
                run_style = Some(*style);
            }
        }
        if let Some(style) = run_style {
            spans.push(Span::styled(run, style));
        }
        Line::from(spans)
    }

    fn finalize(&mut self) {
        if !self.current_line.is_empty() {
            self.finish_line();
        }
    }

    fn write_at_cursor(&mut self, c: char) {
        if self.cursor_col < self.current_line.len() {
            self.current_line[self.cursor_col] = (c, self.current_style);
        } else {
            while self.current_line.len() < self.cursor_col {
                self.current_line.push((' ', Style::default()));
            }
            self.current_line.push((c, self.current_style));
        }
        self.cursor_col += 1;
    }

    fn erase_in_line(&mut self, params: &Params) {
        let n = params.iter().flatten().copied().next().unwrap_or(0);
        match n {
            0 => self.current_line.truncate(self.cursor_col),
            1 => {
                let upper = self.cursor_col.min(self.current_line.len());
                for cell in &mut self.current_line[..upper] {
                    *cell = (' ', Style::default());
                }
            }
            2 => self.current_line.clear(),
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        let mut iter = params.iter().flatten().copied();
        while let Some(p) = iter.next() {
            match p {
                0 => self.current_style = Style::default(),
                1 => self.current_style = self.current_style.add_modifier(Modifier::BOLD),
                2 => self.current_style = self.current_style.add_modifier(Modifier::DIM),
                3 => self.current_style = self.current_style.add_modifier(Modifier::ITALIC),
                4 => self.current_style = self.current_style.add_modifier(Modifier::UNDERLINED),
                7 => self.current_style = self.current_style.add_modifier(Modifier::REVERSED),
                9 => {
                    self.current_style = self.current_style.add_modifier(Modifier::CROSSED_OUT);
                }
                22 => {
                    self.current_style = self
                        .current_style
                        .remove_modifier(Modifier::BOLD | Modifier::DIM);
                }
                23 => {
                    self.current_style = self.current_style.remove_modifier(Modifier::ITALIC);
                }
                24 => {
                    self.current_style = self.current_style.remove_modifier(Modifier::UNDERLINED);
                }
                27 => {
                    self.current_style = self.current_style.remove_modifier(Modifier::REVERSED);
                }
                29 => {
                    self.current_style = self.current_style.remove_modifier(Modifier::CROSSED_OUT);
                }
                30..=37 => self.current_style = self.current_style.fg(basic_color(p - 30)),
                38 => match iter.next() {
                    Some(5) => {
                        if let Some(idx) = iter.next() {
                            self.current_style =
                                self.current_style.fg(extended_color_8bit(idx as u8));
                        }
                    }
                    Some(2) => {
                        if let (Some(r), Some(g), Some(b)) =
                            (iter.next(), iter.next(), iter.next())
                        {
                            self.current_style =
                                self.current_style.fg(Color::Rgb(r as u8, g as u8, b as u8));
                        }
                    }
                    _ => {}
                },
                39 => self.current_style = self.current_style.fg(Color::Reset),
                40..=47 => self.current_style = self.current_style.bg(basic_color(p - 40)),
                48 => match iter.next() {
                    Some(5) => {
                        if let Some(idx) = iter.next() {
                            self.current_style =
                                self.current_style.bg(extended_color_8bit(idx as u8));
                        }
                    }
                    Some(2) => {
                        if let (Some(r), Some(g), Some(b)) =
                            (iter.next(), iter.next(), iter.next())
                        {
                            self.current_style =
                                self.current_style.bg(Color::Rgb(r as u8, g as u8, b as u8));
                        }
                    }
                    _ => {}
                },
                49 => self.current_style = self.current_style.bg(Color::Reset),
                90..=97 => self.current_style = self.current_style.fg(bright_basic(p - 90)),
                100..=107 => self.current_style = self.current_style.bg(bright_basic(p - 100)),
                _ => {}
            }
        }
    }
}

impl Perform for AnsiToLines {
    fn print(&mut self, c: char) {
        self.write_at_cursor(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.finish_line(),
            b'\r' => self.cursor_col = 0,
            b'\t' => {
                let target = (self.cursor_col / 8 + 1) * 8;
                while self.cursor_col < target {
                    self.write_at_cursor(' ');
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        match action {
            'm' => self.apply_sgr(params),
            'K' => self.erase_in_line(params),
            _ => {}
        }
    }
}

fn basic_color(idx: u16) -> Color {
    match idx {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        _ => Color::Reset,
    }
}

fn bright_basic(idx: u16) -> Color {
    match idx {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        7 => Color::White,
        _ => Color::Reset,
    }
}

fn extended_color_8bit(n: u8) -> Color {
    match n {
        0..=7 => basic_color(n as u16),
        8..=15 => bright_basic(n as u16 - 8),
        n => Color::Indexed(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn plain_text_makes_one_line_per_newline() {
        let p = parse_to_lines(b"alpha\nbeta\n", "  ", 10);
        assert_eq!(p.total, 2);
        assert!(!p.truncated);
    }

    #[test]
    fn cr_lf_collapses_to_a_single_line_break() {
        let p = parse_to_lines(b"alpha\r\nbeta\r\n", "  ", 10);
        assert_eq!(p.total, 2);
        assert_eq!(line_text(&p.lines[0]), "  alpha");
        assert_eq!(line_text(&p.lines[1]), "  beta");
    }

    #[test]
    fn truncation_is_reported() {
        let p = parse_to_lines(b"a\nb\nc\nd\ne\n", "  ", 3);
        assert_eq!(p.total, 5);
        assert!(p.truncated);
        assert_eq!(p.lines.len(), 3);
    }

    #[test]
    fn sgr_red_emits_styled_span() {
        let p = parse_to_lines(b"\x1b[31mred\x1b[0m\n", "", 10);
        assert_eq!(p.total, 1);
        let any_red = p.lines.iter().any(|l| {
            l.spans.iter().any(|s| s.style.fg == Some(Color::Red))
        });
        assert!(any_red, "expected a red span");
    }

    #[test]
    fn cursor_moves_are_dropped() {
        let p = parse_to_lines(b"\x1b[2J\x1b[Hhello\x1b[K\nworld\n", "", 10);
        assert_eq!(p.total, 2);
    }

    #[test]
    fn cr_overwrites_existing_chars_on_same_line() {
        // "hello" → \r → "XY" overwrites first two chars → final "XYllo".
        let p = parse_to_lines(b"hello\rXY\n", "", 10);
        assert_eq!(p.total, 1);
        assert_eq!(line_text(&p.lines[0]), "XYllo");
    }

    #[test]
    fn cr_plus_erase_line_then_write_replaces_line() {
        let p = parse_to_lines(b"hello\r\x1b[Kworld\n", "", 10);
        assert_eq!(line_text(&p.lines[0]), "world");
    }

    #[test]
    fn cargo_style_progress_bar_collapses_to_final_state() {
        let input =
            b"\rBuilding (10%)\r\x1b[KBuilding (50%)\r\x1b[KBuilding (100%)\nDone\n";
        let p = parse_to_lines(input, "", 10);
        assert_eq!(p.total, 2);
        assert_eq!(line_text(&p.lines[0]), "Building (100%)");
        assert_eq!(line_text(&p.lines[1]), "Done");
    }

    #[test]
    fn tab_expands_to_next_multiple_of_eight() {
        let p = parse_to_lines(b"a\tb\n", "", 10);
        assert_eq!(line_text(&p.lines[0]), "a       b");
    }

    #[test]
    fn sgr_color_persists_after_overwrite() {
        // First write "hello" in red, \r, write "X" in default style. Cell 0
        // should be 'X' with default style; cells 1-4 stay red.
        let p = parse_to_lines(b"\x1b[31mhello\x1b[0m\rX\n", "", 10);
        assert_eq!(line_text(&p.lines[0]), "Xello");
        // The 'X' span should NOT be red.
        let first_char_red = p.lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "X")
            .map(|s| s.style.fg == Some(Color::Red))
            .unwrap_or(false);
        assert!(!first_char_red, "X should be in default style, not red");
    }
}
