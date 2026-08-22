//! Pane lifecycle and the tab layout tree they sit in.

use super::*;

impl SessionManager {
    /// Handle a `PaneCreated` frame.
    pub(super) fn on_pane_created(
        &mut self,
        pane_id: PaneId,
        session_word_id: &str,
        size: TermSize,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        self.ensure_pane(&pane_id);
        // Record the new pane in the flat list for immediate chrome.
        if let Some(entry) = self
            .session_list
            .iter_mut()
            .find(|e| e.meta.word_id == session_word_id)
        {
            let pane_index = kmux_protocol::pane_index(&pane_id).unwrap_or(0);
            if !entry.panes.iter().any(|p| p.pane_id == pane_id) {
                entry.panes.push(PaneInfo {
                    pane_id: pane_id.clone(),
                    pane_index,
                    program: String::new(),
                    size,
                    attached_clients: vec![],
                    status: SessionStatus::Running,
                    title: String::new(),
                    progress_state: Default::default(),
                    progress: None,
                });
            }
        }
        // `PaneCreate` creates a new tab server-side; its layout arrives
        // with the refreshed session list. Defer focusing the new pane
        // until then.
        self.pending_select_pane = Some(pane_id.clone());
        self.request_session_list();
        events.push(SessionEvent::PaneCreated { pane_id });
        events
    }

    /// A pane died. The requester gets the reply (with the exit code);
    /// every client gets the PTY bus's broadcast of the same close.
    pub(super) fn on_pane_closed(&mut self, pane_id: PaneId) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        events.extend(self.on_pane_gone(pane_id));
        events
    }

    /// The dedicated split reply: a new pane + the tab's new tree. Attach
    /// the new pane (without detaching siblings) when it's our active tab.
    pub(super) fn on_pane_split(
        &mut self,
        word_id: &WordId,
        tab_index: TabIndex,
        new_pane: PaneInfo,
        layout: LayoutNode,
    ) -> Vec<SessionEvent> {
        self.ensure_pane(&new_pane.pane_id);
        let new_idx = new_pane.pane_index;
        if let Some(entry) = self
            .session_list
            .iter_mut()
            .find(|e| e.meta.word_id == *word_id)
        {
            if !entry.panes.iter().any(|p| p.pane_id == new_pane.pane_id) {
                entry.panes.push(new_pane);
            }
            if let Some(tab) = entry.tabs.iter_mut().find(|t| t.tab_index == tab_index) {
                tab.layout = layout;
                tab.focused_pane = new_idx;
            }
        }
        if self.active_session.as_deref() == Some(word_id.as_str())
            && self.active_tab == Some(tab_index)
            && let Some((focus_idx, visible)) = self.tab_view(word_id, tab_index)
        {
            self.set_visible_set(visible);
            self.focus_from_tab(word_id, focus_idx);
        }
        Vec::new()
    }

    /// ── Tab / layout reconciliation ─────────────────────────────────
    /// `LayoutUpdate` is the authoritative tree (+ shared focus) broadcast
    /// after any mutation. Update the cache, and when it targets the tab
    /// this client is viewing, reconcile the visible set + focus.
    pub(super) fn on_layout_update(
        &mut self,
        word_id: &WordId,
        tab_index: TabIndex,
        layout: LayoutNode,
        focused_pane: u32,
    ) -> Vec<SessionEvent> {
        if let Some(tab) = self
            .session_list
            .iter_mut()
            .find(|e| e.meta.word_id == *word_id)
            .and_then(|e| e.tabs.iter_mut().find(|t| t.tab_index == tab_index))
        {
            tab.layout = layout;
            tab.focused_pane = focused_pane;
        }
        if self.active_session.as_deref() == Some(word_id.as_str())
            && self.active_tab == Some(tab_index)
            && let Some((focus_idx, visible)) = self.tab_view(word_id, tab_index)
        {
            self.set_visible_set(visible);
            self.focus_from_tab(word_id, focus_idx);
        }
        Vec::new()
    }

    /// Never sent: `kmuxd` constructs no `LayoutChanged`, and the
    /// authoritative `LayoutUpdate` supersedes it. Kept as an arm rather
    /// than a `..` catch-all so a new `SessionEventMsg` variant fails to
    /// compile here instead of being silently dropped (docs/testing.md R4).
    pub(super) fn on_event_layout_changed() -> Vec<SessionEvent> {
        Vec::new()
    }

    /// Handle a `Event` frame carrying `SessionEventMsg::PaneResized`.
    pub(super) fn on_event_pane_resized(
        &mut self,
        pane_id: &str,
        size: TermSize,
    ) -> Vec<SessionEvent> {
        if let Some(grid) = self.buffers.get_mut(pane_id) {
            grid.resize(size.rows, size.cols);
        }
        Vec::new()
    }

    /// A pane was spawned — by a session/tab create, a split, or a
    /// restore. Same shape: an id, no `PaneInfo`, no layout.
    pub(super) fn on_event_pane_spawned(&mut self, pane_id: &str) -> Vec<SessionEvent> {
        let cached = kmux_protocol::pane_word(pane_id)
            .is_none_or(|word_id| !self.knows_session(word_id))
            || self.knows_pane(pane_id);
        self.resync_unless_cached(cached);
        Vec::new()
    }

    /// The pane's child process exited on its own. The pane keeps its
    /// slot in the layout tree until someone closes it, so this only
    /// records the status.
    pub(super) fn on_event_pane_exited(
        &mut self,
        pane_id: &str,
        code: Option<i32>,
        signal: Option<i32>,
    ) -> Vec<SessionEvent> {
        self.on_pane_exited(pane_id, code, signal);
        Vec::new()
    }

    /// The pane's isolated VT worker crashed (issue #126). The shell is
    /// untouched and the daemon respawns the worker, which resyncs this
    /// client with a fresh snapshot through the normal `TerminalSnapshot`
    /// path — so no sync state is disturbed here, only the UI is told.
    pub(super) fn on_event_pane_faulted(&mut self, pane_id: PaneId) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        warn!(%pane_id, "pane VT worker faulted; the daemon is respawning it");
        self.status_msg = format!("Pane '{pane_id}' is recovering");
        events.push(SessionEvent::PaneFaulted { pane_id });
        events
    }
}
