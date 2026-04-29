use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use vte::{Params, Parser, Perform};

/// Parse `bytes` as a stream of text + ANSI escape sequences and produce
/// styled `Line`s suitable for ratatui rendering. SGR (color/style) is honored;
/// cursor positioning, line clears, and other terminal control sequences are
/// dropped. CR is silently consumed so PTY-output `\r\n` collapses to `\n`.
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
    current_spans: Vec<Span<'static>>,
    pending_text: String,
    current_style: Style,
}

impl AnsiToLines {
    fn new(indent: String) -> Self {
        Self {
            indent,
            lines: Vec::new(),
            current_spans: Vec::new(),
            pending_text: String::new(),
            current_style: Style::default(),
        }
    }

    fn flush_text(&mut self) {
        if !self.pending_text.is_empty() {
            let text = std::mem::take(&mut self.pending_text);
            self.current_spans.push(Span::styled(text, self.current_style));
        }
    }

    fn finish_line(&mut self) {
        self.flush_text();
        let mut spans = vec![Span::raw(self.indent.clone())];
        spans.append(&mut self.current_spans);
        self.lines.push(Line::from(spans));
    }

    fn finalize(&mut self) {
        self.flush_text();
        if !self.current_spans.is_empty() {
            let mut spans = vec![Span::raw(self.indent.clone())];
            spans.append(&mut self.current_spans);
            self.lines.push(Line::from(spans));
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
        self.pending_text.push(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.finish_line(),
            b'\t' => self.pending_text.push('\t'),
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
        if action == 'm' {
            self.flush_text();
            self.apply_sgr(params);
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

    #[test]
    fn plain_text_makes_one_line_per_newline() {
        let p = parse_to_lines(b"alpha\nbeta\n", "  ", 10);
        assert_eq!(p.total, 2);
        assert!(!p.truncated);
    }

    #[test]
    fn cr_is_silently_consumed() {
        let p = parse_to_lines(b"alpha\r\nbeta\r\n", "  ", 10);
        assert_eq!(p.total, 2);
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
        // First line should contain a span styled red somewhere.
        let any_red = p.lines.iter().any(|l| {
            l.spans.iter().any(|s| s.style.fg == Some(Color::Red))
        });
        assert!(any_red, "expected a red span");
    }

    #[test]
    fn cursor_moves_are_dropped() {
        // \x1b[2J = clear screen; \x1b[H = home; \x1b[K = clear line.
        // None of these should produce extra lines or visible text.
        let p = parse_to_lines(b"\x1b[2J\x1b[Hhello\x1b[K\nworld\n", "", 10);
        assert_eq!(p.total, 2);
    }
}
