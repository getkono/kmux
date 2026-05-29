use crossterm::event::{MouseEvent, MouseEventKind};
use kmux_client::input::encode_mouse_scroll;

use crate::mode::Mode;
use crate::recent_servers::ServerKind;

use super::{App, KeyResult, SwitchTarget, TopBarAction};

impl App {
    /// Handle a mouse event. Returns a `KeyResult` when the mouse action
    /// should propagate to the event loop (Reconnect, SwitchServer); otherwise
    /// `None` for a purely local state change that the next redraw covers.
    pub(super) fn handle_mouse(&mut self, event: MouseEvent) -> Option<KeyResult> {
        // Picker overlays consume mouse input first: hover to highlight,
        // click on an item to select, click outside to dismiss.
        if matches!(
            self.mode,
            Mode::SessionPicker | Mode::ServerPicker | Mode::DirectoryPicker
        ) && let Some(result) = self.handle_picker_mouse(&event)
        {
            return result;
        }

        // Row 0 is the top bar. Dispatch Down clicks via the single hit-box
        // list recorded during the last render.
        if event.row == 0 && matches!(event.kind, MouseEventKind::Down(_)) {
            return self.dispatch_top_bar_click(event.column);
        }

        // Scroll wheel inside a pane: either forward to the PTY (when mouse
        // reporting is on) or scroll the local scrollback.
        let pane_id = self.mgr.active_pane_id().map(|s| s.to_string())?;
        match event.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_pane(&pane_id, event.column, event.row, 3);
            }
            MouseEventKind::ScrollDown => {
                self.scroll_pane(&pane_id, event.column, event.row, -3);
            }
            _ => {}
        }
        None
    }

    /// Translate a top-bar click at `col` into the corresponding action.
    fn dispatch_top_bar_click(&mut self, col: u16) -> Option<KeyResult> {
        let action = self.top_bar_hits.action_at(col).cloned()?;
        match action {
            TopBarAction::OpenServerPicker => {
                self.server_picker_selected = 0;
                self.server_picker_search.clear();
                self.mode = Mode::ServerPicker;
                None
            }
            TopBarAction::Reconnect => Some(KeyResult::Reconnect),
            TopBarAction::OpenSessionPicker => {
                self.session_picker_selected = 0;
                self.session_picker_search.clear();
                self.mode = Mode::SessionPicker;
                None
            }
            TopBarAction::SelectPane(pane_id) => {
                self.mgr.select_pane(pane_id);
                None
            }
            TopBarAction::CreatePane => {
                self.mgr.create_pane(App::current_term_size());
                None
            }
        }
    }

    /// Returns `Some(result)` when the event was handled by the picker layer.
    /// The outer option distinguishes "handled" from "not handled"; the inner
    /// `Option<KeyResult>` carries the optional propagation target.
    fn handle_picker_mouse(&mut self, event: &MouseEvent) -> Option<Option<KeyResult>> {
        let rect = self.picker_hits.rect?;
        let col = event.column;
        let row = event.row;
        let inside = col >= rect.x
            && col < rect.x + rect.width
            && row >= rect.y
            && row < rect.y + rect.height;

        match event.kind {
            MouseEventKind::Moved => {
                if inside && let Some(idx) = self.picker_item_at_row(row) {
                    self.set_picker_selected(idx);
                }
                Some(None)
            }
            MouseEventKind::Down(_) => {
                if !inside {
                    // Click outside the overlay dismisses the picker.
                    self.mode = Mode::Normal;
                    return Some(None);
                }
                if let Some(idx) = self.picker_item_at_row(row) {
                    self.set_picker_selected(idx);
                    return Some(self.activate_picker_selection());
                }
                Some(None)
            }
            // Scroll/other events pass through so the underlying grid's scroll
            // handling still works while a picker is open.
            _ => None,
        }
    }

    fn picker_item_at_row(&self, row: u16) -> Option<usize> {
        self.picker_hits.item_rows.iter().position(|&r| r == row)
    }

    fn set_picker_selected(&mut self, idx: usize) {
        match self.mode {
            Mode::SessionPicker => self.session_picker_selected = idx,
            Mode::ServerPicker => self.server_picker_selected = idx,
            Mode::DirectoryPicker => self.dir_picker_selected = idx,
            _ => {}
        }
    }

    /// Dispatch the same side effects as pressing Enter on the current picker
    /// selection. Mirrors the logic in `key_handler.rs` for
    /// `SelectPickerEntry`, `ServerPickerSelect`, and `DirPickerSubmit`.
    fn activate_picker_selection(&mut self) -> Option<KeyResult> {
        match self.mode {
            Mode::SessionPicker => {
                // Mirror key_handler::Action::SelectPickerEntry. Index 0 is the
                // synthetic "[+] New session" affordance.
                if self.session_picker_selected == 0 {
                    self.dir_picker_buffer = self.initial_cwd.clone();
                    self.dir_picker_selected = 0;
                    self.mode = Mode::DirectoryPicker;
                } else {
                    let search = self.session_picker_search.to_lowercase();
                    let word_id = self
                        .mgr
                        .session_list()
                        .iter()
                        .filter(|e| {
                            search.is_empty()
                                || e.meta.name.to_lowercase().contains(&search)
                                || e.meta.word_id.to_lowercase().contains(&search)
                        })
                        .nth(self.session_picker_selected - 1)
                        .map(|e| e.meta.word_id.clone());
                    if let Some(word_id) = word_id {
                        self.mgr.select_session(word_id);
                    }
                    self.mode = Mode::Normal;
                }
                None
            }
            Mode::ServerPicker => {
                let servers = self.filtered_servers();
                let choice = servers.get(self.server_picker_selected).cloned();
                self.mode = Mode::Normal;
                let server = choice?;
                if server.server_string == self.server_string {
                    return None;
                }
                let target = match server.kind {
                    ServerKind::Local => SwitchTarget::Local,
                    ServerKind::Ssh {
                        user,
                        host,
                        ssh_port,
                    } => SwitchTarget::Ssh(kmux_client::ssh::RemoteTarget {
                        user,
                        host,
                        ssh_port,
                    }),
                };
                Some(KeyResult::SwitchServer(target))
            }
            Mode::DirectoryPicker => {
                let matches = self.dir_picker_matches();
                if let Some(entry) = matches.get(self.dir_picker_selected) {
                    let word_id = entry.meta.word_id.clone();
                    self.mgr.select_session(word_id);
                }
                self.mode = Mode::Normal;
                None
            }
            _ => None,
        }
    }

    /// Apply a signed scroll delta to a pane in local scrollback mode.
    /// Positive = scroll up (towards history), negative = scroll down.
    pub(super) fn apply_local_scroll_delta(&mut self, pane_id: &str, delta: i32) {
        if let Some(grid) = self.mgr.buffer_mut(pane_id) {
            if delta > 0 {
                grid.scroll_up(delta as usize);
            } else if delta < 0 {
                grid.scroll_down((-delta) as usize);
            }
        }
    }

    fn scroll_pane(&mut self, pane_id: &str, col: u16, row: u16, lines: i32) {
        let use_pty = self
            .mgr
            .buffer(pane_id)
            .map(|g| g.modes().mouse_report())
            .unwrap_or(false);
        if use_pty {
            let sgr = self
                .mgr
                .buffer(pane_id)
                .map(|g| g.modes().sgr_mouse())
                .unwrap_or(false);
            let bytes = encode_mouse_scroll(col + 1, row + 1, lines, sgr);
            if !bytes.is_empty() {
                self.mgr.send_input(bytes);
            }
        } else if let Some(grid) = self.mgr.buffer_mut(pane_id) {
            if lines > 0 {
                grid.scroll_up(lines as usize);
            } else {
                grid.scroll_down((-lines) as usize);
            }
        }
    }
}
