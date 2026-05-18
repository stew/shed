//! OSC 52 clipboard writing.
//!
//! Used by the right-click context menu and Shift/Ctrl-click shortcut to
//! get text onto the system clipboard from inside the TUI. OSC 52 is
//! fire-and-forget — the receiving terminal must support it and (when
//! relevant) be configured to accept it. See
//! [`write_clipboard_osc52`] for terminal-specific notes.

use std::io::{self, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Build the OSC 52 escape sequence for writing `text` to the system
/// clipboard. Targets both the CLIPBOARD and PRIMARY selections (`cp`)
/// so terminals that distinguish them (X11) put the value where pasting
/// expects it. Terminator is BEL (`\x07`), accepted by every OSC 52
/// implementation in the wild.
///
/// Returns the raw escape sequence string; [`write_clipboard_osc52`]
/// just flushes it to stdout. Split out so the format can be unit-tested
/// without touching the real terminal.
pub(super) fn osc52_sequence(text: &str) -> String {
    let payload = STANDARD.encode(text.as_bytes());
    format!("\x1b]52;cp;{payload}\x07")
}

/// Write `text` to the system clipboard via OSC 52.
///
/// Inside tmux this relies on `set -g set-clipboard on` — tmux then
/// forwards OSC 52 from applications to the outer terminal natively.
/// (We deliberately do NOT use DCS passthrough wrapping: that would
/// additionally require `set -g allow-passthrough on`, which is off by
/// default in tmux ≥ 3.3, and would *break* the common case.) The outer
/// terminal must also support OSC 52 — kitty, iTerm2, wezterm, alacritty,
/// foot, and modern xterm do; many older terminals silently ignore it.
pub(super) fn write_clipboard_osc52(text: &str) -> io::Result<()> {
    let seq = osc52_sequence(text);
    let mut out = io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_sequence_uses_cp_target_and_bel_terminator() {
        let s = osc52_sequence("hello");
        assert!(s.starts_with("\x1b]52;cp;"), "got: {s:?}");
        assert!(s.ends_with("\x07"), "got: {s:?}");
    }

    #[test]
    fn osc52_sequence_base64_encodes_the_payload() {
        // RFC 4648 vector — `hello` → `aGVsbG8=`.
        let s = osc52_sequence("hello");
        assert!(s.contains("aGVsbG8="), "got: {s:?}");
    }

    #[test]
    fn osc52_sequence_handles_empty_input() {
        let s = osc52_sequence("");
        assert_eq!(s, "\x1b]52;cp;\x07");
    }

    #[test]
    fn osc52_sequence_handles_non_ascii() {
        // UTF-8 bytes are base64-encoded as-is.
        let s = osc52_sequence("✓");
        // `✓` = E2 9C 93, base64 = `4pyT`.
        assert!(s.contains("4pyT"), "got: {s:?}");
    }
}
