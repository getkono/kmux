//! Terminal-size helpers that stay frontend-side: they query crossterm and
//! account for the TUI's chrome. The connection/session orchestration that used
//! to live here moved to `kmux_app::core` (see `AppCore`).

use kmux_protocol::messages::TermSize;

use super::App;

impl App {
    /// Subtract UI chrome (3 rows) from raw terminal dimensions.
    ///
    /// The 3 rows are: session bar (1) + status bar (1) + hint bar (1).
    /// This is the single place that knows the chrome height so future
    /// layout changes only need to be made here.
    pub(super) fn compute_pane_size(rows: u16, cols: u16) -> TermSize {
        TermSize {
            rows: rows.saturating_sub(3),
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// Query the current terminal size, accounting for UI chrome.
    pub(crate) fn current_term_size() -> TermSize {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        Self::compute_pane_size(rows, cols)
    }
}
