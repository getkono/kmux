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

#[cfg(test)]
mod tests {
    use super::super::testing::fixture_core;
    use super::*;

    /// The frontend owns the renderer object, so the core cannot rebuild it —
    /// it asks, via the return value, and sets the repaint flag it does own.
    #[test]
    fn resetting_the_renderer_asks_the_frontend_and_forces_a_repaint() {
        let mut core = fixture_core();
        core.force_clear = false;
        assert_eq!(core.on_reset_renderer(), KeyResult::ResetRenderer);
        assert!(core.force_clear, "the next frame must be a full repaint");
    }

    /// The flag is the core's; the daemon learns about it through the manager.
    /// Toggling has to do both, in both directions — a toggle that only ever
    /// turns snapshot mode *on* would pass a one-way test.
    #[test]
    fn snapshot_mode_toggles_in_both_directions() {
        let mut core = fixture_core();
        assert!(!core.force_snapshot_mode, "off by default");

        assert_eq!(core.on_toggle_snapshot_mode(), KeyResult::Continue);
        assert!(core.force_snapshot_mode);

        core.on_toggle_snapshot_mode();
        assert!(!core.force_snapshot_mode, "and back off again");
    }
}
