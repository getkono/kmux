use crossterm::event::{MouseEvent, MouseEventKind};
use kmux_client::input::encode_mouse_scroll;

use crate::mode::Mode;

use super::App;

impl App {
    pub(super) fn handle_mouse(&mut self, event: MouseEvent) {
        // Clicks on row 0 open the server picker (server badge) or session picker (session badge).
        if event.row == 0 && matches!(event.kind, MouseEventKind::Down(_)) {
            // Server badge occupies [0, server_badge_cols).
            if event.column < self.server_badge_cols {
                self.server_picker_selected = 0;
                self.server_picker_search.clear();
                self.mode = Mode::ServerPicker;
                return;
            }
            // Session badge occupies [server_badge_cols + 1, server_badge_cols + 1 + session_badge_cols).
            // (+1 accounts for the separator span between the two badges.)
            let session_start = self.server_badge_cols + 1;
            let session_end = session_start + self.session_badge_cols;
            if event.column >= session_start && event.column < session_end {
                self.session_picker_selected = 0;
                self.session_picker_search.clear();
                self.mode = Mode::SessionPicker;
                return;
            }
        }

        let Some(pane_id) = self.mgr.active_pane_id().map(|s| s.to_string()) else {
            return;
        };
        match event.kind {
            MouseEventKind::ScrollUp => {
                let use_pty = self
                    .mgr
                    .buffer(&pane_id)
                    .map(|g| g.modes().mouse_report())
                    .unwrap_or(false);
                if use_pty {
                    let col = event.column + 1;
                    let row = event.row + 1;
                    let sgr = self
                        .mgr
                        .buffer(&pane_id)
                        .map(|g| g.modes().sgr_mouse())
                        .unwrap_or(false);
                    let bytes = encode_mouse_scroll(col, row, 3, sgr);
                    if !bytes.is_empty() {
                        self.mgr.send_input(bytes);
                    }
                } else if let Some(grid) = self.mgr.buffer_mut(&pane_id) {
                    grid.scroll_up(3);
                }
            }
            MouseEventKind::ScrollDown => {
                let use_pty = self
                    .mgr
                    .buffer(&pane_id)
                    .map(|g| g.modes().mouse_report())
                    .unwrap_or(false);
                if use_pty {
                    let col = event.column + 1;
                    let row = event.row + 1;
                    let sgr = self
                        .mgr
                        .buffer(&pane_id)
                        .map(|g| g.modes().sgr_mouse())
                        .unwrap_or(false);
                    let bytes = encode_mouse_scroll(col, row, -3, sgr);
                    if !bytes.is_empty() {
                        self.mgr.send_input(bytes);
                    }
                } else if let Some(grid) = self.mgr.buffer_mut(&pane_id) {
                    grid.scroll_down(3);
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_resize(&mut self, rows: u16, cols: u16) {
        // Account for session bar (1 row) + status bar (1 row) + hint bar (1 row)
        let term_rows = rows.saturating_sub(3);
        let term_cols = cols;

        if let Some(pane_id) = self.mgr.active_pane_id().map(|s| s.to_string()) {
            self.mgr.send_resize(&pane_id, term_rows, term_cols);
        }
    }
}
