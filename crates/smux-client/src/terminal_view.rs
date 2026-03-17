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

    /// Feed raw PTY output bytes into the buffer.
    /// Strips ANSI/VT escape sequences for display.
    pub fn push_bytes(&mut self, data: &[u8]) {
        let raw = String::from_utf8_lossy(data);
        // Simple ANSI escape sequence stripper: remove ESC[ ... m / ESC sequences
        let stripped = strip_ansi(&raw);
        // Normalize PTY line endings: \r\n -> \n, then drop any remaining \r
        let stripped = stripped.replace("\r\n", "\n").replace('\r', "");
        self.text.push_str(&stripped);
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

/// Render the terminal buffer as a scrollable text widget.
pub fn view<'a>(buffer: &'a TerminalBuffer, _session: &'a str) -> Element<'a, Message> {
    use iced::widget::{container, scrollable, text};

    let content = text(&buffer.text).font(iced::Font::MONOSPACE).size(13);

    scrollable(container(content).width(Length::Fill).padding(4))
        .width(Length::Fill)
        .height(Length::Fill)
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
        // Simulates a progress-bar style overwrite (\r moves cursor to line start)
        buf.push_bytes(b"loading\rprompt$ ");
        assert_eq!(buf.text, "loadingprompt$ ");
    }
}
