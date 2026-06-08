//! Tab + multi-pane attachment for the client session manager.
//!
//! A session is viewed one **tab** at a time (`active_tab`, client-local). A tab
//! arranges one or more **panes** (PTYs) in a layout tree; the client attaches
//! to *all* leaves of the active tab simultaneously (the "visible set") and
//! renders them tiled. `active_pane` is the single **focused** leaf — the input
//! target — within that set. The layout tree + shared focus live on the server;
//! the client mirrors them from `LayoutUpdate` / the session list.

use kmux_protocol::messages::{ClientMessage, LayoutNode, PaneId, TabInfo, TermSize};

use super::SessionManager;

impl SessionManager {
    // ── Accessors ───────────────────────────────────────────────────────────

    /// The tab index currently viewed in the active session.
    pub fn active_tab(&self) -> Option<u32> {
        self.active_tab
    }

    /// The panes attached + visible in the active tab (the tiling set).
    pub fn visible_panes(&self) -> &[PaneId] {
        &self.visible_panes
    }

    /// The active tab's layout tree — pass this to `kmux_app::layout` to lay out
    /// the visible panes. `None` when there is no active tab.
    pub fn active_layout(&self) -> Option<&LayoutNode> {
        let word = self.active_session.as_deref()?;
        let tab = self.active_tab?;
        self.tab_layout(word, tab)
    }

    /// The tabs of the active session.
    pub fn active_session_tabs(&self) -> &[TabInfo] {
        self.active_session
            .as_ref()
            .and_then(|w| self.session_list.iter().find(|e| e.meta.word_id == *w))
            .map(|e| e.tabs.as_slice())
            .unwrap_or(&[])
    }

    /// The cached layout tree of `(word_id, tab_index)`.
    pub(super) fn tab_layout(&self, word_id: &str, tab_index: u32) -> Option<&LayoutNode> {
        self.session_list
            .iter()
            .find(|e| e.meta.word_id == word_id)
            .and_then(|e| e.tabs.iter().find(|t| t.tab_index == tab_index))
            .map(|t| &t.layout)
    }

    /// `(focused pane_index, visible pane_ids)` for `(word_id, tab_index)`.
    pub(super) fn tab_view(&self, word_id: &str, tab_index: u32) -> Option<(u32, Vec<PaneId>)> {
        let entry = self
            .session_list
            .iter()
            .find(|e| e.meta.word_id == word_id)?;
        let tab = entry.tabs.iter().find(|t| t.tab_index == tab_index)?;
        let visible = tab
            .layout
            .leaves()
            .into_iter()
            .map(|i| format!("{word_id}/{i}"))
            .collect();
        Some((tab.focused_pane, visible))
    }

    /// Locate the `(word_id, tab_index)` that contains `pane_id`, via the cached
    /// tab layouts.
    pub(super) fn locate_pane(&self, pane_id: &str) -> Option<(String, u32)> {
        let (word, idx) = pane_id.rsplit_once('/')?;
        let idx: u32 = idx.parse().ok()?;
        let entry = self.session_list.iter().find(|e| e.meta.word_id == word)?;
        let tab = entry
            .tabs
            .iter()
            .find(|t| t.layout.leaves().contains(&idx))?;
        Some((word.to_string(), tab.tab_index))
    }

    // ── Per-pane sizing (driven by the frontend's resolver) ──────────────────

    /// Set the resolved per-pane sizes for the visible set and `Resize` any
    /// attached pane whose size changed. The frontend computes these via
    /// `kmux_app::layout::resolve_layout` whenever the window or layout changes.
    pub fn set_pane_sizes(&mut self, sizes: Vec<(PaneId, TermSize)>) {
        for (pane_id, size) in sizes {
            let changed = self
                .pane_sizes
                .get(&pane_id)
                .map(|s| s.rows != size.rows || s.cols != size.cols)
                .unwrap_or(true);
            self.pane_sizes.insert(pane_id.clone(), size);
            if changed && self.pane_sync.contains_key(&pane_id) {
                self.send_ws(ClientMessage::Resize { pane_id, size });
            }
        }
    }

    // ── Visible-set management ───────────────────────────────────────────────

    /// Make `new_visible` the attached/visible set: detach panes no longer in
    /// it, attach newly-visible ones, and record the result.
    pub(super) fn set_visible_set(&mut self, new_visible: Vec<PaneId>) {
        let old = std::mem::take(&mut self.visible_panes);
        for p in &old {
            if !new_visible.contains(p) {
                self.send_ws(ClientMessage::Detach { pane_id: p.clone() });
                self.pane_sync.remove(p);
            }
        }
        for p in &new_visible {
            if !old.contains(p) {
                if let Some(buf) = self.buffers.get_mut(p) {
                    buf.clear();
                }
                self.attach_fresh(p.clone());
            }
        }
        self.visible_panes = new_visible;
    }

    /// Set `active_pane` to the tab's focused leaf (or the first visible pane).
    pub(super) fn focus_from_tab(&mut self, word_id: &str, focus_idx: u32) {
        let focus_pane = format!("{word_id}/{focus_idx}");
        self.active_pane = if self.visible_panes.contains(&focus_pane) {
            Some(focus_pane)
        } else {
            self.visible_panes.first().cloned()
        };
    }

    // ── Tab navigation + focus ───────────────────────────────────────────────

    /// Switch the active session's viewed tab.
    pub fn select_tab(&mut self, tab_index: u32) {
        let Some(word_id) = self.active_session.clone() else {
            return;
        };
        let Some((focus_idx, visible)) = self.tab_view(&word_id, tab_index) else {
            return;
        };
        self.active_tab = Some(tab_index);
        self.set_visible_set(visible);
        self.focus_from_tab(&word_id, focus_idx);
    }

    /// Cycle the active session's viewed tab by `offset` (wrapping).
    pub fn cycle_tab(&mut self, offset: i32) {
        let Some(word_id) = self.active_session.clone() else {
            return;
        };
        let tabs: Vec<u32> = self
            .session_list
            .iter()
            .find(|e| e.meta.word_id == word_id)
            .map(|e| e.tabs.iter().map(|t| t.tab_index).collect())
            .unwrap_or_default();
        if tabs.is_empty() {
            return;
        }
        let cur = self
            .active_tab
            .and_then(|t| tabs.iter().position(|&x| x == t))
            .unwrap_or(0);
        let n = tabs.len() as i32;
        let new = ((cur as i32 + offset).rem_euclid(n)) as usize;
        self.select_tab(tabs[new]);
    }

    /// Focus a pane within the active tab, publishing the shared focus to the
    /// server. If the pane is not currently visible, route it as a full select.
    pub fn focus_pane(&mut self, pane_id: String) {
        if !self.visible_panes.contains(&pane_id) {
            self.select_pane(pane_id);
            return;
        }
        self.active_pane = Some(pane_id.clone());
        if let (Some(word_id), Some(tab_index)) = (self.active_session.clone(), self.active_tab)
            && let Some(idx) = pane_id
                .rsplit_once('/')
                .and_then(|(_, i)| i.parse::<u32>().ok())
        {
            self.send_ws(ClientMessage::SetFocus {
                word_id,
                tab_index,
                pane_index: idx,
            });
        }
    }

    // Geometric focus movement (FocusLeft/Right/Up/Down) lives in `kmux-app`,
    // which owns the resolver: it computes the target pane via
    // `layout::focus_neighbor` and calls `focus_pane` above.

    // ── Split / new-tab intents ──────────────────────────────────────────────

    /// Split the focused pane in the active tab, spawning a new pane in `dir`.
    pub fn split_focused(&mut self, dir: kmux_protocol::messages::SplitDir) {
        let (Some(word_id), Some(tab_index)) = (self.active_session.clone(), self.active_tab)
        else {
            return;
        };
        let Some(from_pane) = self
            .active_pane
            .as_ref()
            .and_then(|p| p.rsplit_once('/'))
            .and_then(|(_, i)| i.parse::<u32>().ok())
        else {
            return;
        };
        let rid = self.next_rid();
        self.send_ws(ClientMessage::PaneSplit {
            request_id: rid,
            word_id,
            tab_index,
            from_pane,
            dir,
            program: None,
            args: vec![],
            size: self.last_term_size,
        });
    }

    /// Create a new tab (with one fresh pane) in the active session.
    pub fn create_tab(&mut self) {
        let Some(word_id) = self.active_session.clone() else {
            return;
        };
        let rid = self.next_rid();
        self.send_ws(ClientMessage::TabCreate {
            request_id: rid,
            word_id,
            program: None,
            args: vec![],
            size: self.last_term_size,
        });
    }

    /// Close the active tab.
    pub fn close_tab(&mut self) {
        if let Some(tab_index) = self.active_tab {
            self.close_tab_index(tab_index);
        }
    }

    /// Close a specific tab of the active session by index.
    pub fn close_tab_index(&mut self, tab_index: u32) {
        let Some(word_id) = self.active_session.clone() else {
            return;
        };
        let rid = self.next_rid();
        self.send_ws(ClientMessage::TabClose {
            request_id: rid,
            word_id,
            tab_index,
        });
    }
}
