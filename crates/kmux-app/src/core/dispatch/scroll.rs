//! Scrollback movement within the focused pane.

use super::super::{AppCore, KeyResult};

impl AppCore {
    /// Handle [`Action::ScrollUp`](crate::mode::Action::ScrollUp).
    pub(super) fn on_scroll_up(&mut self, n: usize) -> KeyResult {
        if let Some(grid) = self.mgr.active_grid_mut() {
            grid.scroll_up(n);
        }
        KeyResult::Continue
    }

    /// Handle [`Action::ScrollDown`](crate::mode::Action::ScrollDown).
    pub(super) fn on_scroll_down(&mut self, n: usize) -> KeyResult {
        if let Some(grid) = self.mgr.active_grid_mut() {
            grid.scroll_down(n);
        }
        KeyResult::Continue
    }

    /// Handle [`Action::ScrollPageUp`](crate::mode::Action::ScrollPageUp).
    pub(super) fn on_scroll_page_up(&mut self) -> KeyResult {
        if let Some(grid) = self.mgr.active_grid_mut() {
            let rows = grid.rows;
            grid.scroll_up(rows);
        }
        KeyResult::Continue
    }

    /// Handle [`Action::ScrollPageDown`](crate::mode::Action::ScrollPageDown).
    pub(super) fn on_scroll_page_down(&mut self) -> KeyResult {
        if let Some(grid) = self.mgr.active_grid_mut() {
            let rows = grid.rows;
            grid.scroll_down(rows);
        }
        KeyResult::Continue
    }
}
