//! Tab + multi-pane attachment for the client session manager.
//!
//! A session is viewed one **tab** at a time (`active_tab`, client-local). A tab
//! arranges one or more **panes** (PTYs) in a layout tree; the client attaches
//! to *all* leaves of the active tab simultaneously (the "visible set") and
//! renders them tiled. `active_pane` is the single **focused** leaf — the input
//! target — within that set. The layout tree + shared focus live on the server;
//! the client mirrors them from `LayoutUpdate` / the session list.

use kmux_protocol::messages::{ClientMessage, LayoutNode, LayoutScheme, PaneId, TabInfo, TermSize};
use kmux_protocol::{format_pane_id, pane_index, parse_pane_id};

use super::SessionManager;

/// The preset layouts [`SessionManager::cycle_layout`] rotates through.
const LAYOUT_SCHEMES: [LayoutScheme; 4] = [
    LayoutScheme::EvenHorizontal,
    LayoutScheme::EvenVertical,
    LayoutScheme::MainVertical,
    LayoutScheme::MainHorizontal,
];

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

    /// The display name of the active tab, if any (for pre-filling a rename).
    pub fn active_tab_name(&self) -> Option<String> {
        let tab = self.active_tab?;
        self.active_session_tabs()
            .iter()
            .find(|t| t.tab_index == tab)
            .map(|t| t.name.clone())
    }

    /// The tabs of the active session.
    pub fn active_session_tabs(&self) -> &[TabInfo] {
        self.active_session
            .as_ref()
            .and_then(|w| self.session_list.iter().find(|e| e.meta.word_id == *w))
            .map_or(&[][..], |e| e.tabs.as_slice())
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
            .map(|i| format_pane_id(word_id, i))
            .collect();
        Some((tab.focused_pane, visible))
    }

    /// Locate the `(word_id, tab_index)` that contains `pane_id`, via the cached
    /// tab layouts.
    pub(super) fn locate_pane(&self, pane_id: &str) -> Option<(String, u32)> {
        let (word, idx) = parse_pane_id(pane_id)?;
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
                .is_none_or(|s| s.rows != size.rows || s.cols != size.cols);
            self.pane_sizes.insert(pane_id.clone(), size);
            if changed && self.pane_sync.contains_key(&pane_id) {
                self.send_ws(ClientMessage::Resize { pane_id, size });
            }
        }
    }

    // ── Tiling schemes + zoom ────────────────────────────────────────────────

    /// Apply the next preset layout to the active tab (tmux-style "next-layout").
    pub fn cycle_layout(&mut self) {
        self.layout_scheme_idx = (self.layout_scheme_idx + 1) % LAYOUT_SCHEMES.len();
        self.apply_scheme(LAYOUT_SCHEMES[self.layout_scheme_idx]);
    }

    /// Regenerate the active tab's layout into a preset `scheme`
    /// (server-authoritative: the server rebuilds the tree and broadcasts it).
    pub fn apply_scheme(&mut self, scheme: LayoutScheme) {
        let (Some(word_id), Some(tab_index)) = (self.active_session.clone(), self.active_tab)
        else {
            return;
        };
        self.send_ws(ClientMessage::ApplyLayoutScheme {
            word_id,
            tab_index,
            scheme,
        });
    }

    /// Toggle tmux-style zoom of the focused pane — a client-local view flag that
    /// renders/sizes only the focused pane full-area, without mutating the shared
    /// tree.
    pub fn toggle_zoom(&mut self) {
        self.zoomed = !self.zoomed;
    }

    /// Whether zoom is currently active.
    pub fn is_zoomed(&self) -> bool {
        self.zoomed
    }

    /// The layout the frontend should render/size against: the active tab's tree,
    /// or — when zoomed — a single-leaf layout of just the focused pane. `None`
    /// when there is no active tab. Frontends use this instead of
    /// [`active_layout`](Self::active_layout) for rendering and sizing.
    pub fn render_layout(&self) -> Option<LayoutNode> {
        let layout = self.active_layout()?;
        if self.zoomed
            && let Some(idx) = self.active_pane.as_deref().and_then(pane_index)
        {
            return Some(LayoutNode::single(idx));
        }
        Some(layout.clone())
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
        let focus_pane = format_pane_id(word_id, focus_idx);
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
        for pane_id in &self.visible_panes {
            self.attention_panes.remove(pane_id);
        }
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
        self.attention_panes.remove(&pane_id);
        if let (Some(word_id), Some(tab_index)) = (self.active_session.clone(), self.active_tab)
            && let Some(idx) = pane_index(&pane_id)
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

    /// Resize a split in the active tab: set the child weights of the `Split`
    /// addressed by `path` (a child-index descent from the layout root). The new
    /// ratios are computed by the frontend's resolver (`kmux_app::layout`); the
    /// server clamps to its minimum, renormalizes to 1000, and broadcasts the
    /// authoritative `LayoutUpdate`. No-op when there is no active session/tab.
    pub fn set_layout_ratios(&mut self, path: Vec<u32>, ratios: Vec<u16>) {
        let (Some(word_id), Some(tab_index)) = (self.active_session.clone(), self.active_tab)
        else {
            return;
        };
        self.send_ws(ClientMessage::SetLayoutRatios {
            word_id,
            tab_index,
            path,
            ratios,
        });
    }

    // ── Split / new-tab intents ──────────────────────────────────────────────

    /// Split the focused pane in the active tab, spawning a new pane in `dir`.
    pub fn split_focused(&mut self, dir: kmux_protocol::messages::SplitDir) {
        let (Some(word_id), Some(tab_index)) = (self.active_session.clone(), self.active_tab)
        else {
            return;
        };
        let Some(from_pane) = self.active_pane.as_deref().and_then(pane_index) else {
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

    /// Swap the focused pane with the next (`offset = 1`) or previous
    /// (`offset = -1`) pane in the active tab's leaf order, wrapping. The server
    /// exchanges the two leaves in place and broadcasts the new tree; focus
    /// follows the moved pane to its new slot. No-op for a single-pane tab.
    pub fn swap_focused(&mut self, offset: i32) {
        let (Some(word_id), Some(tab_index)) = (self.active_session.clone(), self.active_tab)
        else {
            return;
        };
        let Some(focused) = self.active_pane.as_deref().and_then(pane_index) else {
            return;
        };
        let Some(leaves) = self.tab_layout(&word_id, tab_index).map(LayoutNode::leaves) else {
            return;
        };
        if leaves.len() < 2 {
            return;
        }
        let Some(pos) = leaves.iter().position(|&p| p == focused) else {
            return;
        };
        let n = leaves.len() as i32;
        let target = leaves[((pos as i32 + offset).rem_euclid(n)) as usize];
        if target == focused {
            return;
        }
        self.send_ws(ClientMessage::PaneSwap {
            word_id,
            tab_index,
            a: focused,
            b: target,
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

    /// Rename a tab of the active session.
    pub fn rename_tab(&mut self, tab_index: u32, new_name: &str) {
        let Some(word_id) = self.active_session.clone() else {
            return;
        };
        let rid = self.next_rid();
        self.send_ws(ClientMessage::TabRename {
            request_id: rid,
            word_id,
            tab_index,
            new_name: new_name.to_string(),
        });
    }

    /// Move a tab to a zero-based position in the active session.
    pub fn reorder_tab(&mut self, tab_index: u32, new_position: u32) {
        let Some(word_id) = self.active_session.clone() else {
            return;
        };
        self.send_ws(ClientMessage::TabReorder {
            word_id,
            tab_index,
            new_position,
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
