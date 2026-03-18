// TODO: Replace this stub with a full VT terminal renderer once a suitable
// mechanism for feeding remote PTY bytes into a terminal widget is available.
// Options:
//  - alacritty_terminal::Term + custom iced::canvas widget for grid rendering
//  - iced_term with a local PTY bridge proxy program
//
// The TerminalBuffer below is a minimal accumulator of raw bytes (UTF-8 lossy)
// that can be displayed via a scrollable text widget.

use iced::{Element, Length};

use crate::app::Message;

/// A minimal terminal output accumulator.
/// Strips ANSI escape sequences and displays the remaining text.
pub struct TerminalBuffer {
    /// Raw accumulated output text (ANSI stripped, UTF-8 lossy).
    pub text: String,
}

impl TerminalBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
        }
    }

    /// Clear all accumulated output (called before re-attaching so scrollback replay starts fresh).
    pub fn clear(&mut self) {
        self.text.clear();
    }

    /// Feed raw PTY output bytes into the buffer.
    /// Strips ANSI/VT escape sequences for display and interprets basic control characters.
    pub fn push_bytes(&mut self, data: &[u8]) {
        let raw = String::from_utf8_lossy(data);
        // Strip ANSI/VT escape sequences first
        let stripped = strip_ansi(&raw);
        // Normalize PTY line endings: \r\n → \n, then drop any remaining \r
        let stripped = stripped.replace("\r\n", "\n").replace('\r', "");

        // Interpret control characters character-by-character
        for ch in stripped.chars() {
            match ch {
                '\x08' => {
                    // BS: remove last character but don't cross a newline boundary
                    if self.text.chars().last().is_some_and(|c| c != '\n') {
                        self.text.pop();
                    }
                }
                '\x7f' => {
                    // DEL: same destructive behavior as BS for display purposes
                    if self.text.chars().last().is_some_and(|c| c != '\n') {
                        self.text.pop();
                    }
                }
                c if c.is_control() && c != '\n' && c != '\t' => {
                    // Drop other non-printable C0 controls (BEL \x07, FF \x0c, etc.)
                }
                c => {
                    self.text.push(c);
                }
            }
        }

        // Keep buffer bounded to avoid unbounded memory growth
        const MAX_LINES: usize = 100_000;
        let line_count = self.text.lines().count();
        if line_count > MAX_LINES {
            let skip = line_count - MAX_LINES;
            let trimmed: String = self.text.lines().skip(skip).collect::<Vec<_>>().join("\n");
            self.text = trimmed;
        }
    }
}

impl Default for TerminalBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable scrollable ID so `app.rs` can programmatically scroll to the bottom.
pub fn terminal_scroll_id() -> iced::widget::scrollable::Id {
    iced::widget::scrollable::Id::new("terminal-output")
}

/// Render the terminal buffer as a scrollable text widget.
pub fn view<'a>(buffer: &'a TerminalBuffer, _session: &'a str) -> Element<'a, Message> {
    use iced::widget::{container, scrollable, text};

    let content = text(&buffer.text).font(iced::Font::MONOSPACE).size(13);

    scrollable(container(content).width(Length::Fill).padding(4))
        .id(terminal_scroll_id())
        .width(Length::Fill)
        .height(Length::Fill)
        .direction(scrollable::Direction::Vertical(
            scrollable::Scrollbar::default(),
        ))
        .into()
}

/// Minimal ANSI escape sequence stripper.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next(); // consume '['
                    // Skip until we see a letter (the command byte)
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next(); // consume ']'
                    // OSC: skip until BEL or ST
                    for c2 in chars.by_ref() {
                        if c2 == '\x07' || c2 == '\x1b' {
                            break;
                        }
                    }
                }
                _ => {
                    // Other escape: skip one char
                    chars.next();
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_simple_csi() {
        assert_eq!(strip_ansi("\x1b[31mhello\x1b[0m"), "hello");
    }

    #[test]
    fn strip_cursor_movement() {
        assert_eq!(strip_ansi("\x1b[2J\x1b[H"), "");
    }

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn push_bytes_normalizes_crlf() {
        let mut buf = TerminalBuffer::new();
        buf.push_bytes(b"line1\r\nline2\r\n");
        assert_eq!(buf.text, "line1\nline2\n");
    }

    #[test]
    fn push_bytes_strips_bare_cr() {
        let mut buf = TerminalBuffer::new();
        // Bare \r is stripped (not interpreted as carriage-return overwrite)
        buf.push_bytes(b"loading\rprompt$ ");
        assert_eq!(buf.text, "loadingprompt$ ");
    }

    #[test]
    fn push_bytes_handles_backspace() {
        let mut buf = TerminalBuffer::new();
        // Two backspaces should delete the last two chars
        buf.push_bytes(b"hello\x08\x08");
        assert_eq!(buf.text, "hel");
    }

    #[test]
    fn push_bytes_handles_pty_backspace_echo() {
        let mut buf = TerminalBuffer::new();
        // Standard PTY echo for backspace: BS + space (overwrite) + BS (cursor back)
        // Net result: last char is deleted
        buf.push_bytes(b"hello\x08 \x08");
        assert_eq!(buf.text, "hell");
    }

    #[test]
    fn push_bytes_drops_bel() {
        let mut buf = TerminalBuffer::new();
        buf.push_bytes(b"\x07hello\x07");
        assert_eq!(buf.text, "hello");
    }

    #[test]
    fn push_bytes_backspace_stops_at_newline() {
        let mut buf = TerminalBuffer::new();
        // Backspace at the start of a new line must not delete the preceding newline
        buf.push_bytes(b"line1\n\x08");
        assert_eq!(buf.text, "line1\n");
    }
}
