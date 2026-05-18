//! Readline-style line editing for shed's single-line input bars.
//!
//! Every input bar (main prompt, in-place command editor, pin/rerun/
//! write/save/open/alias-name/search/rename-tab) reads through the same
//! pure helpers. The model is `(text: &mut String, cursor: &mut usize)`
//! — `cursor` is a byte offset into `text`, always at a char boundary
//! in `[0, text.len()]`.
//!
//! - [`apply_readline_edit`] dispatches a single key event to the right
//!   `tf_*` helper, returning `true` when the key was consumed.
//! - [`handle_text_input`] wraps `apply_readline_edit` and additionally
//!   translates `Enter` → `Commit` and `Esc` → `Cancel` for callers that
//!   want the commit/cancel/continue contract.
//! - [`render_input_bar`] / [`input_spans_with_cursor`] paint the bar
//!   with an inline inverted-block cursor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub(super) fn tf_insert_char(text: &mut String, cursor: &mut usize, c: char) {
    text.insert(*cursor, c);
    *cursor += c.len_utf8();
}

pub(super) fn tf_backspace(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let prev = text[..*cursor]
        .chars()
        .next_back()
        .expect("non-empty prefix")
        .len_utf8();
    *cursor -= prev;
    text.replace_range(*cursor..*cursor + prev, "");
}

pub(super) fn tf_delete(text: &mut String, cursor: &mut usize) {
    if *cursor >= text.len() {
        return;
    }
    let next = text[*cursor..]
        .chars()
        .next()
        .expect("non-empty suffix")
        .len_utf8();
    text.replace_range(*cursor..*cursor + next, "");
}

pub(super) fn tf_left(text: &str, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let prev = text[..*cursor]
        .chars()
        .next_back()
        .expect("non-empty prefix")
        .len_utf8();
    *cursor -= prev;
}

pub(super) fn tf_right(text: &str, cursor: &mut usize) {
    if *cursor >= text.len() {
        return;
    }
    let next = text[*cursor..]
        .chars()
        .next()
        .expect("non-empty suffix")
        .len_utf8();
    *cursor += next;
}

pub(super) fn tf_home(cursor: &mut usize) {
    *cursor = 0;
}

pub(super) fn tf_end(text: &str, cursor: &mut usize) {
    *cursor = text.len();
}

pub(super) fn tf_kill_to_beginning(text: &mut String, cursor: &mut usize) {
    text.replace_range(..*cursor, "");
    *cursor = 0;
}

pub(super) fn tf_kill_to_end(text: &mut String, cursor: &mut usize) {
    text.replace_range(*cursor.., "");
}

/// Word-back boundary: skip whitespace immediately before `cursor`,
/// then skip the run of non-whitespace before that. Returns the byte
/// index of the boundary.
fn tf_word_back_index(text: &str, cursor: usize) -> usize {
    let mut end = cursor;
    while end > 0 {
        let c = text[..end].chars().next_back().expect("non-empty");
        if !c.is_whitespace() {
            break;
        }
        end -= c.len_utf8();
    }
    while end > 0 {
        let c = text[..end].chars().next_back().expect("non-empty");
        if c.is_whitespace() {
            break;
        }
        end -= c.len_utf8();
    }
    end
}

fn tf_word_forward_index(text: &str, cursor: usize) -> usize {
    let mut pos = cursor;
    while pos < text.len() {
        let c = text[pos..].chars().next().expect("non-empty");
        if !c.is_whitespace() {
            break;
        }
        pos += c.len_utf8();
    }
    while pos < text.len() {
        let c = text[pos..].chars().next().expect("non-empty");
        if c.is_whitespace() {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

pub(super) fn tf_kill_word_back(text: &mut String, cursor: &mut usize) {
    let new_pos = tf_word_back_index(text, *cursor);
    text.replace_range(new_pos..*cursor, "");
    *cursor = new_pos;
}

pub(super) fn tf_word_left(text: &str, cursor: &mut usize) {
    *cursor = tf_word_back_index(text, *cursor);
}

pub(super) fn tf_word_right(text: &str, cursor: &mut usize) {
    *cursor = tf_word_forward_index(text, *cursor);
}

/// Outcome of [`handle_text_input`] applied to a single-line input bar:
/// `Commit` if Enter was pressed, `Cancel` for Esc, `Continue` for an
/// editing keystroke (any non-terminal key, even one that didn't
/// actually mutate the buffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InputOutcome {
    Commit,
    Cancel,
    Continue,
}

/// Single-line input bar dispatch: handle Enter / Esc as commit /
/// cancel, route everything else through [`apply_readline_edit`].
pub(super) fn handle_text_input(
    text: &mut String,
    cursor: &mut usize,
    key: &KeyEvent,
) -> InputOutcome {
    match key.code {
        KeyCode::Enter => InputOutcome::Commit,
        KeyCode::Esc => InputOutcome::Cancel,
        _ => {
            apply_readline_edit(text, cursor, key);
            InputOutcome::Continue
        }
    }
}

/// Apply a readline-style key edit. Returns `true` if the key was
/// consumed (the caller should not run its own key logic). Returns
/// `false` for keys this layer doesn't handle (Enter, Esc, Tab, Up/Down,
/// etc.) — the caller handles those.
pub(super) fn apply_readline_edit(text: &mut String, cursor: &mut usize, key: &KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) if !ctrl && !alt => {
            tf_insert_char(text, cursor, c);
            true
        }
        KeyCode::Char('a') if ctrl => {
            tf_home(cursor);
            true
        }
        KeyCode::Char('e') if ctrl => {
            tf_end(text, cursor);
            true
        }
        KeyCode::Char('u') if ctrl => {
            tf_kill_to_beginning(text, cursor);
            true
        }
        KeyCode::Char('k') if ctrl => {
            tf_kill_to_end(text, cursor);
            true
        }
        KeyCode::Char('w') if ctrl => {
            tf_kill_word_back(text, cursor);
            true
        }
        KeyCode::Char('b') if ctrl => {
            tf_left(text, cursor);
            true
        }
        KeyCode::Char('f') if ctrl => {
            tf_right(text, cursor);
            true
        }
        KeyCode::Char('b') if alt => {
            tf_word_left(text, cursor);
            true
        }
        KeyCode::Char('f') if alt => {
            tf_word_right(text, cursor);
            true
        }
        KeyCode::Backspace => {
            tf_backspace(text, cursor);
            true
        }
        KeyCode::Delete => {
            tf_delete(text, cursor);
            true
        }
        KeyCode::Home => {
            tf_home(cursor);
            true
        }
        KeyCode::End => {
            tf_end(text, cursor);
            true
        }
        KeyCode::Left if alt => {
            tf_word_left(text, cursor);
            true
        }
        KeyCode::Right if alt => {
            tf_word_right(text, cursor);
            true
        }
        KeyCode::Left => {
            tf_left(text, cursor);
            true
        }
        KeyCode::Right => {
            tf_right(text, cursor);
            true
        }
        _ => false,
    }
}

/// Render a status-bar style input prompt as a [`Paragraph`]: a label
/// in `accent`, then `text` with the cursor visualised inline. Used for
/// every single-line input bar in the bottom status row.
pub(super) fn render_input_bar(
    label: &str,
    accent: Color,
    text: &str,
    cursor: usize,
) -> Paragraph<'static> {
    let mut spans = vec![
        Span::raw(" ".to_string()),
        Span::styled(
            label.to_string(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(input_spans_with_cursor(text, cursor, accent));
    Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray))
}

/// Render `text` with the cursor visualised as an inverted shed.
/// `accent` is the colour the cursor shed uses for its background;
/// the text under it renders in black.
pub(super) fn input_spans_with_cursor(
    text: &str,
    cursor: usize,
    accent: Color,
) -> Vec<Span<'static>> {
    let cursor = cursor.min(text.len());
    let before = text[..cursor].to_string();
    let (at, after) = if cursor >= text.len() {
        (" ".to_string(), String::new())
    } else {
        let c_len = text[cursor..].chars().next().expect("non-empty").len_utf8();
        (
            text[cursor..cursor + c_len].to_string(),
            text[cursor + c_len..].to_string(),
        )
    };
    let cursor_style = Style::default()
        .fg(Color::Black)
        .bg(accent)
        .add_modifier(Modifier::BOLD);
    vec![
        Span::raw(before),
        Span::styled(at, cursor_style),
        Span::raw(after),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn tf_insert_inserts_at_cursor_and_advances() {
        let mut text = "ad".to_string();
        let mut cursor = 1;
        tf_insert_char(&mut text, &mut cursor, 'b');
        assert_eq!(text, "abd");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn tf_backspace_removes_char_before_cursor() {
        let mut text = "abc".to_string();
        let mut cursor = 2;
        tf_backspace(&mut text, &mut cursor);
        assert_eq!(text, "ac");
        assert_eq!(cursor, 1);
        cursor = 0;
        tf_backspace(&mut text, &mut cursor);
        assert_eq!(text, "ac");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn tf_delete_removes_char_after_cursor() {
        let mut text = "abc".to_string();
        let mut cursor = 1;
        tf_delete(&mut text, &mut cursor);
        assert_eq!(text, "ac");
        assert_eq!(cursor, 1);
        cursor = text.len();
        tf_delete(&mut text, &mut cursor);
        assert_eq!(text, "ac");
    }

    #[test]
    fn tf_kill_to_beginning_and_end() {
        let mut text = "hello world".to_string();
        let mut cursor = 6;
        tf_kill_to_beginning(&mut text, &mut cursor);
        assert_eq!(text, "world");
        assert_eq!(cursor, 0);

        let mut text = "hello world".to_string();
        let mut cursor = 5;
        tf_kill_to_end(&mut text, &mut cursor);
        assert_eq!(text, "hello");
        assert_eq!(cursor, 5);
    }

    #[test]
    fn tf_kill_word_back_eats_one_word_and_trailing_ws() {
        let mut text = "git checkout main".to_string();
        let mut cursor = text.len();
        tf_kill_word_back(&mut text, &mut cursor);
        assert_eq!(text, "git checkout ");
        assert_eq!(cursor, "git checkout ".len());
        tf_kill_word_back(&mut text, &mut cursor);
        assert_eq!(text, "git ");
    }

    #[test]
    fn tf_word_left_right_jump_by_word() {
        let text = "one two  three";
        let mut cursor = text.len();
        tf_word_left(text, &mut cursor);
        assert_eq!(&text[cursor..], "three");
        tf_word_left(text, &mut cursor);
        assert_eq!(&text[cursor..], "two  three");

        let mut cursor = 0;
        tf_word_right(text, &mut cursor);
        assert_eq!(&text[..cursor], "one");
        tf_word_right(text, &mut cursor);
        assert_eq!(&text[..cursor], "one two");
    }

    #[test]
    fn apply_readline_handles_basic_keys() {
        let mut t = String::from("ab");
        let mut c = 2;
        assert!(apply_readline_edit(
            &mut t,
            &mut c,
            &key(KeyCode::Char('c'))
        ));
        assert_eq!(t, "abc");
        assert_eq!(c, 3);

        assert!(apply_readline_edit(&mut t, &mut c, &ctrl('a')));
        assert_eq!(c, 0);
        assert!(apply_readline_edit(&mut t, &mut c, &ctrl('e')));
        assert_eq!(c, 3);
        assert!(apply_readline_edit(&mut t, &mut c, &ctrl('u')));
        assert_eq!(t, "");
        assert_eq!(c, 0);

        assert!(!apply_readline_edit(&mut t, &mut c, &key(KeyCode::Enter)));
        assert!(!apply_readline_edit(&mut t, &mut c, &key(KeyCode::Esc)));
        assert!(!apply_readline_edit(&mut t, &mut c, &key(KeyCode::Tab)));
    }

    #[test]
    fn apply_readline_alt_word_movement() {
        let mut t = String::from("one two three");
        let mut c = t.len();
        assert!(apply_readline_edit(&mut t, &mut c, &alt('b')));
        assert_eq!(&t[c..], "three");
        assert!(apply_readline_edit(
            &mut t,
            &mut c,
            &KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
        ));
        assert_eq!(&t[c..], "two three");
    }

    #[test]
    fn input_outcome_routes_enter_esc_and_edits() {
        let mut t = String::from("ab");
        let mut c = 2;
        assert_eq!(
            handle_text_input(&mut t, &mut c, &key(KeyCode::Enter)),
            InputOutcome::Commit
        );
        assert_eq!(
            handle_text_input(&mut t, &mut c, &key(KeyCode::Esc)),
            InputOutcome::Cancel
        );
        assert_eq!(
            handle_text_input(&mut t, &mut c, &key(KeyCode::Char('c'))),
            InputOutcome::Continue
        );
        assert_eq!(t, "abc");
    }
}
