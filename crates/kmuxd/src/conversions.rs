use kmux_protocol::messages::TermSize;
use kmux_pty::config::WindowSize;

/// Convert a protocol `TermSize` to a PTY `WindowSize`.
pub fn term_size_to_window(t: TermSize) -> WindowSize {
    WindowSize {
        rows: t.rows,
        cols: t.cols,
    }
}
