//! Tab lifecycle and ordering.

use super::*;

impl SessionManager {
    /// A tab was created (a different client, or via `TabCreate`). The
    /// event carries the tab index but not its full layout, so refresh.
    pub(super) fn on_tab_created(&mut self, word_id: &str, tab: TabInfo) -> Vec<SessionEvent> {
        let tab_index = tab.tab_index;
        if let Some(entry) = self
            .session_list
            .iter_mut()
            .find(|e| e.meta.word_id == word_id)
            && !entry.tabs.iter().any(|t| t.tab_index == tab.tab_index)
        {
            entry.tabs.push(tab);
        }
        // If this is our active session, switch to the new tab.
        if self.active_session.as_deref() == Some(word_id) {
            self.select_tab(tab_index);
        }
        Vec::new()
    }

    /// A tab died. The requester gets the reply; every client gets the
    /// broadcast, which the daemon sends with no accompanying
    /// `LayoutUpdate` — this arm is the only reconciliation there is.
    pub(super) fn on_tab_closed(
        &mut self,
        word_id: &str,
        tab_index: TabIndex,
    ) -> Vec<SessionEvent> {
        self.on_tab_gone(word_id, tab_index);
        Vec::new()
    }

    /// A tab was renamed (by this or another client). Update the cached
    /// name; the frontend's tab strip reconciles from it next tick.
    pub(super) fn on_event_tab_renamed(
        &mut self,
        word_id: &str,
        tab_index: TabIndex,
        name: String,
    ) -> Vec<SessionEvent> {
        if let Some(entry) = self
            .session_list
            .iter_mut()
            .find(|e| e.meta.word_id == word_id)
            && let Some(tab) = entry.tabs.iter_mut().find(|t| t.tab_index == tab_index)
        {
            tab.name = name;
        }
        Vec::new()
    }

    /// Handle a `Event` frame carrying `SessionEventMsg::TabsReordered`.
    pub(super) fn on_event_tabs_reordered(
        &mut self,
        word_id: &str,
        tab_indices: &[TabIndex],
    ) -> Vec<SessionEvent> {
        if let Some(entry) = self
            .session_list
            .iter_mut()
            .find(|entry| entry.meta.word_id == word_id)
        {
            entry.tabs.sort_by_key(|tab| {
                tab_indices
                    .iter()
                    .position(|index| *index == tab.tab_index)
                    .unwrap_or(usize::MAX)
            });
        }
        Vec::new()
    }

    /// A tab was created by some client. The broadcast carries the index
    /// but no `TabInfo`, so the tree can only come from a fresh list.
    pub(super) fn on_event_tab_created(
        &mut self,
        word_id: &str,
        tab_index: TabIndex,
    ) -> Vec<SessionEvent> {
        let cached = self
            .session_list
            .iter()
            .find(|e| e.meta.word_id == word_id)
            .is_none_or(|e| e.tabs.iter().any(|t| t.tab_index == tab_index));
        self.resync_unless_cached(cached);
        Vec::new()
    }
}
