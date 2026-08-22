//! Actions aimed at the focused pane's contents.

use super::super::{AppCore, KeyResult};

impl AppCore {
    /// Handle [`Action::SendSignal`](crate::mode::Action::SendSignal).
    pub(super) fn on_send_signal(&mut self, signal: i32) -> KeyResult {
        if let Some(pane_id) = self.mgr.active_pane_id().map(ToString::to_string) {
            self.mgr.send_signal(&pane_id, signal);
        }
        KeyResult::Continue
    }

    /// Handle [`Action::CopySelection`](crate::mode::Action::CopySelection).
    pub(super) fn on_copy_selection(&mut self) -> KeyResult {
        if let Some(text) = self
            .mgr
            .active_grid()
            .and_then(kmux_client::grid::CellGrid::selected_text)
        {
            return KeyResult::CopyToClipboard(text);
        }
        KeyResult::Continue
    }
}
