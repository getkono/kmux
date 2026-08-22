//! Actions that change how the terminal is drawn or streamed, rather than
//! what it contains.

use super::super::{AppCore, KeyResult};

impl AppCore {
    /// Handle [`Action::ResetRenderer`](crate::mode::Action::ResetRenderer).
    pub(super) fn on_reset_renderer(&mut self) -> KeyResult {
        tracing::info!(
            target: "kmux::render_debug",
            "ResetRenderer requested: rebuilding renderer + atlas, full repaint"
        );
        // Force a full re-pack/repaint; the frontend rebuilds its own
        // renderer/atlas on the resulting effect (it owns that object).
        self.force_clear = true;
        KeyResult::ResetRenderer
    }

    /// Handle [`Action::ToggleSnapshotMode`](crate::mode::Action::ToggleSnapshotMode).
    pub(super) fn on_toggle_snapshot_mode(&mut self) -> KeyResult {
        self.force_snapshot_mode = !self.force_snapshot_mode;
        self.mgr.set_snapshot_mode(self.force_snapshot_mode);
        KeyResult::Continue
    }
}
