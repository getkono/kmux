//! What a pane asks the user to look at: titles, the bell, shell progress,
//! OSC 52 clipboard writes, and `kmux notify`.

use super::*;

impl SessionManager {
    /// Handle a `Event` frame carrying `SessionEventMsg::PaneTitleChanged`.
    pub(super) fn on_event_pane_title_changed(
        &mut self,
        pane_id: PaneId,
        title: String,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        for entry in &mut self.session_list {
            if let Some(pane) = entry.panes.iter_mut().find(|p| p.pane_id == pane_id) {
                pane.title = title.clone();
                break;
            }
        }
        events.push(SessionEvent::PaneTitleChanged { pane_id, title });
        events
    }

    /// Handle a `Event` frame carrying `SessionEventMsg::PaneBell`.
    pub(super) fn on_event_pane_bell(&mut self, pane_id: PaneId) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        self.attention_panes.insert(pane_id.clone());
        events.push(SessionEvent::PaneBell { pane_id });
        events
    }

    /// Handle a `Event` frame carrying `SessionEventMsg::PaneProgressChanged`.
    pub(super) fn on_event_pane_progress_changed(
        &mut self,
        pane_id: PaneId,
        state: PaneProgressState,
        progress: Option<u8>,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        // Update the cached snapshot so the frontend's per-pane progress
        // bar repaints from `PaneInfo` on the next render tick.
        for entry in &mut self.session_list {
            if let Some(pane) = entry.panes.iter_mut().find(|p| p.pane_id == pane_id) {
                pane.progress_state = state;
                pane.progress = progress;
                break;
            }
        }
        events.push(SessionEvent::PaneProgressChanged {
            pane_id,
            state,
            progress,
        });
        events
    }

    /// Handle a `Event` frame carrying `SessionEventMsg::PaneClipboardCopy`.
    pub(super) fn on_event_pane_clipboard_copy(
        pane_id: PaneId,
        selection: String,
        data: String,
    ) -> Vec<SessionEvent> {
        // Pure relay: the app layer applies the active-pane policy and
        // decodes the base64 payload at the clipboard leaf.
        vec![SessionEvent::ClipboardCopy {
            pane_id,
            selection,
            data,
        }]
    }

    /// Handle a `Event` frame carrying `SessionEventMsg::PaneAttention`.
    pub(super) fn on_event_pane_attention(
        pane_id: PaneId,
        kind: AttentionKind,
        title: String,
        body: String,
        attention_id: u64,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        // The session word the GUI focuses on click; derive it from the
        // pane id (already local — federation rewrote it upstream).
        let word_id = kmux_protocol::pane_word(&pane_id)
            .unwrap_or(&pane_id)
            .to_string();
        events.push(SessionEvent::PaneAttention {
            word_id,
            pane_id,
            kind,
            title,
            body,
            attention_id,
        });
        events
    }
}
