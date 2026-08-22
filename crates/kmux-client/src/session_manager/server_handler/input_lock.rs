//! The exclusive input lock, which arbitrates between clients on one pane.

use super::*;

impl SessionManager {
    /// Handle a `InputLockGranted` frame.
    pub(super) fn on_input_lock_granted(&mut self, pane_id: PaneId) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        self.input_locked.insert(pane_id.clone(), true);
        self.status_msg = format!("Input lock acquired on '{pane_id}'");
        events.push(SessionEvent::InputLockGranted { pane_id });
        events
    }

    /// Handle a `InputLockDenied` frame.
    pub(super) fn on_input_lock_denied(
        &mut self,
        pane_id: PaneId,
        holder: ClientId,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        let who = self.client_label(holder);
        self.status_msg = format!("Input lock denied on '{pane_id}' (held by {who})");
        events.push(SessionEvent::InputLockDenied { pane_id, holder });
        events
    }

    /// Handle a `InputLockReleased` frame.
    pub(super) fn on_input_lock_released(&mut self, pane_id: PaneId) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        self.input_locked.insert(pane_id.clone(), false);
        self.status_msg = format!("Input lock released on '{pane_id}'");
        events.push(SessionEvent::InputLockReleased { pane_id });
        events
    }
}
