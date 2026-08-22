//! Session lifecycle, and the cached list the pickers read.

use super::*;

impl SessionManager {
    /// Handle a `SessionListResult` frame.
    pub(super) fn on_session_list_result(
        &mut self,
        sessions: &[SessionEntry],
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        self.session_list = sessions.to_vec();
        for entry in sessions {
            for pane in &entry.panes {
                self.ensure_pane(&pane.pane_id);
            }
        }
        // Resolve a deferred "focus the pane I just created" once its tab
        // is known; else pick an initial session; else re-sync the active
        // session's tab after a refresh.
        if let Some(pending) = self.pending_select_pane.take() {
            if self.locate_pane(&pending).is_some() {
                self.select_pane(pending);
            } else {
                self.pending_select_pane = Some(pending);
            }
        } else if self.active_session.is_none() {
            if let Some(first) = sessions.first().map(|e| e.meta.word_id.clone()) {
                self.select_session(first);
            }
        } else if self.visible_panes.is_empty()
            && let Some(word) = self.active_session.clone()
        {
            self.select_session(word);
        }
        events.push(SessionEvent::SessionListReceived);
        events
    }

    /// Handle a `ClosedSessionListResult` frame.
    pub(super) fn on_closed_session_list_result(
        &mut self,
        sessions: Vec<ClosedSessionEntry>,
    ) -> Vec<SessionEvent> {
        // Already ordered most-recently-active first by the daemon. The
        // launcher polls `closed_session_list()` when it opens (issue #64).
        self.closed_sessions = sessions;
        Vec::new()
    }

    /// Handle a `SessionCreated` frame.
    pub(super) fn on_session_created(&mut self, entry: SessionEntry) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        let word_id = entry.meta.word_id.clone();
        for pane in &entry.panes {
            self.ensure_pane(&pane.pane_id);
        }
        self.session_list.push(entry);
        self.status_msg = format!("Session '{word_id}' created");
        // Switch to the new session (detaches the old visible set and
        // attaches the new session's active tab).
        self.select_session(word_id.clone());
        events.push(SessionEvent::SessionCreated { word_id });
        events
    }

    /// A session died. The requester gets the reply; every client gets the
    /// broadcast when the session drained via its last tab or pane.
    pub(super) fn on_session_closed(&mut self, word_id: WordId) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        events.extend(self.on_session_gone(word_id));
        events
    }

    /// Handle a `Event` frame carrying `SessionEventMsg::SessionRenamed`.
    pub(super) fn on_event_session_renamed(
        &mut self,
        word_id: WordId,
        new_name: String,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        for entry in &mut self.session_list {
            if entry.meta.word_id == word_id {
                entry.meta.name = new_name.clone();
                break;
            }
        }
        events.push(SessionEvent::SessionRenamed { word_id, new_name });
        events
    }

    /// A session was restored from the graveyard by some client. The
    /// broadcast names only the word, so a client that does not already
    /// have the entry re-lists; unlike the `SessionCreated` reply it must
    /// not *switch* to it — that would yank the view of every other GUI.
    pub(super) fn on_event_session_created(&mut self, word_id: &str) -> Vec<SessionEvent> {
        let cached = self.knows_session(word_id);
        self.resync_unless_cached(cached);
        Vec::new()
    }

    /// Handle a `SessionKicked` frame.
    pub(super) fn on_session_kicked(
        &mut self,
        word_id: WordId,
        by_label: String,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        warn!("kicked from session {word_id} by {by_label}");
        self.leave_session(&word_id);
        events.push(SessionEvent::KickedFromSession { word_id, by_label });
        events
    }
}
