//! Action dispatch on [`AppCore`]: the single source of truth for how a
//! resolved [`Action`] mutates client state, shared by the key path and the
//! command palette.
//!
//! Two arms that require toolkit I/O are *not* here: `Action::ForwardKey`
//! (needs the raw toolkit key event to encode under live Ghostty mode state —
//! the frontend handles it before calling dispatch) and clipboard copy/paste
//! (emitted as [`KeyResult::CopyToClipboard`] / [`KeyResult::RequestPaste`]
//! effects that the frontend performs).

use crate::cmd;
use crate::mode::{Action, Mode};
use crate::recent_servers::ServerKind;

use super::{AppCore, COMMAND_HISTORY_CAP, KeyResult, SwitchTarget, TopBarAction};

impl AppCore {
    /// Apply an [`Action`] to the core. Used both by the key path and by the
    /// command palette so a single source of truth governs behavior.
    pub async fn dispatch_action(&mut self, action: Action) -> KeyResult {
        match action {
            // ForwardKey is handled frontend-side (it needs the raw toolkit
            // event); it never reaches the core dispatch.
            Action::ForwardKey => {}
            Action::CreateSession => {
                self.mgr.create_session(None, None, self.term_size);
            }
            Action::CreatePane => {
                self.mgr.create_pane(self.term_size);
            }
            Action::CloseSession => {
                if let Some(word_id) = self.mgr.active_session().map(|s| s.to_string()) {
                    self.mode = Mode::ConfirmCloseSession { word_id };
                }
            }
            Action::ClosePane => {
                self.mgr.close_pane();
            }
            Action::ConfirmCloseYes => {
                if let Mode::ConfirmCloseSession { word_id } =
                    std::mem::replace(&mut self.mode, Mode::Normal)
                {
                    self.mgr.close_session(&word_id);
                }
            }
            Action::NextSession => self.mgr.cycle_session(1),
            Action::PrevSession => self.mgr.cycle_session(-1),
            Action::NextPane => self.mgr.cycle_tab(1),
            Action::PrevPane => self.mgr.cycle_tab(-1),
            Action::CloseTab => self.mgr.close_tab(),
            Action::RenameTab => {
                if let (Some(word_id), Some(tab_index)) = (
                    self.mgr.active_session().map(|s| s.to_string()),
                    self.mgr.active_tab(),
                ) {
                    let buffer = self.mgr.active_tab_name().unwrap_or_default();
                    self.mode = Mode::RenameTab {
                        word_id,
                        tab_index,
                        buffer,
                    };
                }
            }
            Action::SplitRight => self
                .mgr
                .split_focused(kmux_protocol::messages::SplitDir::Horizontal),
            Action::SplitDown => self
                .mgr
                .split_focused(kmux_protocol::messages::SplitDir::Vertical),
            Action::FocusLeft => self.focus_dir(crate::layout::FocusDir::Left),
            Action::FocusRight => self.focus_dir(crate::layout::FocusDir::Right),
            Action::FocusUp => self.focus_dir(crate::layout::FocusDir::Up),
            Action::FocusDown => self.focus_dir(crate::layout::FocusDir::Down),
            Action::ResizeLeft => self.resize(crate::layout::FocusDir::Left),
            Action::ResizeRight => self.resize(crate::layout::FocusDir::Right),
            Action::ResizeUp => self.resize(crate::layout::FocusDir::Up),
            Action::ResizeDown => self.resize(crate::layout::FocusDir::Down),
            Action::SwapNext => self.mgr.swap_focused(1),
            Action::SwapPrev => self.mgr.swap_focused(-1),
            Action::CycleLayout => self.mgr.cycle_layout(),
            Action::ToggleZoom => self.mgr.toggle_zoom(),
            Action::FocusPaneAt(i) => self.focus_pane_at(i),
            Action::JumpToSession(idx) => {
                if idx < self.mgr.session_list().len() {
                    let word_id = self.mgr.session_list()[idx].meta.word_id.clone();
                    self.mgr.select_session(word_id);
                }
            }
            Action::RenameSession => {
                if let Some(word_id) = self.mgr.active_session().map(|s| s.to_string()) {
                    let current_name = self
                        .mgr
                        .session_list()
                        .iter()
                        .find(|e| e.meta.word_id == word_id)
                        .map(|e| e.meta.name.clone())
                        .unwrap_or_default();
                    self.mode = Mode::RenameSession {
                        buffer: current_name,
                        word_id,
                    };
                }
            }
            Action::RenameChar(ch) => {
                if let Mode::RenameSession { buffer, .. } | Mode::RenameTab { buffer, .. } =
                    &mut self.mode
                {
                    buffer.push(ch);
                }
            }
            Action::RenameBackspace => {
                if let Mode::RenameSession { buffer, .. } | Mode::RenameTab { buffer, .. } =
                    &mut self.mode
                {
                    buffer.pop();
                }
            }
            Action::RenameSubmit => match std::mem::replace(&mut self.mode, Mode::Normal) {
                Mode::RenameSession { buffer, word_id } => {
                    let new_name = buffer.trim().to_string();
                    self.mgr.rename_session(&word_id, &new_name);
                }
                Mode::RenameTab {
                    tab_index, buffer, ..
                } => {
                    let new_name = buffer.trim().to_string();
                    if !new_name.is_empty() {
                        self.mgr.rename_tab(tab_index, &new_name);
                    }
                }
                _ => {}
            },
            Action::CloseSessionPicker => {
                self.mode = Mode::Normal;
            }
            Action::SelectPickerEntry => {
                self.select_session_picker_entry();
            }
            Action::PickerUp => {
                if self.session_picker_selected > 0 {
                    self.session_picker_selected -= 1;
                }
            }
            Action::PickerDown => {
                // total rows = 1 ("[+] New session") + filtered sessions.
                let total = self.session_picker_matches().len() + 1;
                if self.session_picker_selected + 1 < total {
                    self.session_picker_selected += 1;
                }
            }
            Action::PickerSearchChar(ch) => {
                self.session_picker_search.push(ch);
                self.session_picker_selected = 0;
            }
            Action::PickerSearchBackspace => {
                self.session_picker_search.pop();
                self.session_picker_selected = 0;
            }
            Action::ServerPickerChar(ch) => {
                self.server_picker_search.push(ch);
                self.server_picker_selected = 0;
            }
            Action::ServerPickerBackspace => {
                self.server_picker_search.pop();
                self.server_picker_selected = 0;
            }
            Action::ServerPickerUp => {
                self.server_picker_selected = self.server_picker_selected.saturating_sub(1);
            }
            Action::ServerPickerDown => {
                let count = self.filtered_servers().len();
                if count > 0 && self.server_picker_selected + 1 < count {
                    self.server_picker_selected += 1;
                }
            }
            Action::ServerPickerClose => {}
            Action::ServerPickerSelect => {
                // A different server switches; the same server (or no selection)
                // just closes the picker (resolve already set Mode::Normal).
                if let Some(target) = self.server_picker_switch_target() {
                    return KeyResult::SwitchServer(target);
                }
            }
            Action::Disconnect => {
                self.mgr.disconnect();
                self.mode = Mode::Normal;
            }
            Action::SendSignal(signal) => {
                if let Some(pane_id) = self.mgr.active_pane_id().map(|s| s.to_string()) {
                    self.mgr.send_signal(&pane_id, signal);
                }
            }
            Action::ScrollUp(n) => {
                if let Some(grid) = self.mgr.active_grid_mut() {
                    grid.scroll_up(n);
                }
            }
            Action::ScrollDown(n) => {
                if let Some(grid) = self.mgr.active_grid_mut() {
                    grid.scroll_down(n);
                }
            }
            Action::ScrollPageUp => {
                if let Some(grid) = self.mgr.active_grid_mut() {
                    let rows = grid.rows;
                    grid.scroll_up(rows);
                }
            }
            Action::ScrollPageDown => {
                if let Some(grid) = self.mgr.active_grid_mut() {
                    let rows = grid.rows;
                    grid.scroll_down(rows);
                }
            }
            Action::ToggleHud => {
                self.hud_visible = !self.hud_visible;
            }
            Action::ToggleMetrics => {
                self.metrics_overlay_visible = !self.metrics_overlay_visible;
            }
            Action::ToggleConnection => {
                self.connection_overlay_visible = !self.connection_overlay_visible;
            }
            Action::ToggleSnapshotMode => {
                self.force_snapshot_mode = !self.force_snapshot_mode;
                self.mgr.set_snapshot_mode(self.force_snapshot_mode);
            }
            Action::ToggleInputLock => {
                self.mgr.toggle_input_lock();
            }
            Action::CopySelection => {
                if let Some(text) = self.mgr.active_grid().and_then(|g| g.selected_text()) {
                    return KeyResult::CopyToClipboard(text);
                }
            }
            Action::Paste => {
                return KeyResult::RequestPaste;
            }
            Action::ExitToNormal => {
                self.mode = Mode::Normal;
            }
            Action::DirPickerChar(ch) => {
                self.dir_picker_buffer.push(ch);
                self.dir_picker_selected = 0;
            }
            Action::DirPickerBackspace => {
                self.dir_picker_buffer.pop();
                self.dir_picker_selected = 0;
            }
            Action::DirPickerUp => {
                self.dir_picker_selected = self.dir_picker_selected.saturating_sub(1);
            }
            Action::DirPickerDown => {
                let count = self.dir_picker_matches().len();
                if count > 0 && self.dir_picker_selected + 1 < count {
                    self.dir_picker_selected += 1;
                }
            }
            Action::DirPickerSubmit => {
                let matches = self.dir_picker_matches();
                if let Some(entry) = matches.get(self.dir_picker_selected) {
                    let word_id = entry.meta.word_id.clone();
                    self.mgr.select_session(word_id);
                } else {
                    let cwd = self.dir_picker_buffer.trim().to_string();
                    if !cwd.is_empty() {
                        if let Some(word_id) = self.mgr.find_session_by_cwd(&cwd) {
                            self.mgr.select_session(word_id);
                        } else {
                            self.mgr.create_session(None, Some(&cwd), self.term_size);
                        }
                    }
                }
            }
            Action::DirPickerCancel => {}
            Action::CancelBootstrap => {
                // Dropping the sender triggers the oneshot in the bootstrap task,
                // which causes it to abort. The outcome arm handles the None.
                let _ = self.cancel_tx.take();
            }
            Action::Quit => {
                return KeyResult::Quit;
            }
            Action::Reconnect => {
                return KeyResult::Reconnect;
            }
            Action::ForceRedraw => {
                self.force_clear = true;
            }

            // ── Command palette editing ──────────────────────────────────────
            Action::CommandChar(ch) => {
                if let Mode::Command(state) = &mut self.mode {
                    let pos = state.cursor.min(state.buffer.len());
                    state.buffer.insert(pos, ch);
                    state.cursor = pos + ch.len_utf8();
                    state.selected = 0;
                    state.history_pos = None;
                }
            }
            Action::CommandBackspace => {
                if let Mode::Command(state) = &mut self.mode {
                    let pos = state.cursor.min(state.buffer.len());
                    if pos > 0 {
                        // Find the previous char boundary so we delete a full
                        // grapheme rather than splitting a multi-byte char.
                        let mut new_pos = pos - 1;
                        while !state.buffer.is_char_boundary(new_pos) && new_pos > 0 {
                            new_pos -= 1;
                        }
                        state.buffer.replace_range(new_pos..pos, "");
                        state.cursor = new_pos;
                        state.selected = 0;
                        state.history_pos = None;
                    }
                }
            }
            Action::CommandLeft => {
                if let Mode::Command(state) = &mut self.mode
                    && state.cursor > 0
                {
                    let mut new_pos = state.cursor - 1;
                    while !state.buffer.is_char_boundary(new_pos) && new_pos > 0 {
                        new_pos -= 1;
                    }
                    state.cursor = new_pos;
                }
            }
            Action::CommandRight => {
                if let Mode::Command(state) = &mut self.mode
                    && state.cursor < state.buffer.len()
                {
                    let mut new_pos = state.cursor + 1;
                    while new_pos < state.buffer.len() && !state.buffer.is_char_boundary(new_pos) {
                        new_pos += 1;
                    }
                    state.cursor = new_pos;
                }
            }
            Action::CommandHome => {
                if let Mode::Command(state) = &mut self.mode {
                    state.cursor = 0;
                }
            }
            Action::CommandEnd => {
                if let Mode::Command(state) = &mut self.mode {
                    state.cursor = state.buffer.len();
                }
            }
            Action::CommandHintUp => {
                self.command_hint_up();
            }
            Action::CommandHintDown => {
                self.command_hint_down();
            }
            Action::CommandClearLine => {
                if let Mode::Command(state) = &mut self.mode {
                    state.buffer.clear();
                    state.cursor = 0;
                    state.selected = 0;
                    state.history_pos = None;
                }
            }
            Action::CommandDeleteWordBack => {
                if let Mode::Command(state) = &mut self.mode {
                    let mut end = state.cursor.min(state.buffer.len());
                    // Skip trailing whitespace.
                    while end > 0 {
                        let prev = state.buffer[..end].chars().next_back();
                        match prev {
                            Some(c) if c.is_whitespace() => {
                                end -= c.len_utf8();
                            }
                            _ => break,
                        }
                    }
                    let mut start = end;
                    while start > 0 {
                        let prev = state.buffer[..start].chars().next_back();
                        match prev {
                            Some(c) if !c.is_whitespace() => {
                                start -= c.len_utf8();
                            }
                            _ => break,
                        }
                    }
                    state.buffer.replace_range(start..state.cursor, "");
                    state.cursor = start;
                    state.selected = 0;
                    state.history_pos = None;
                }
            }
            Action::CommandComplete => {
                self.command_apply_completion();
            }
            Action::CommandSubmit => {
                // Compute hints BEFORE we extract the state — they depend on
                // the live `Mode::Command` and we'll fall back to the selected
                // hint if the typed buffer doesn't parse cleanly.
                let hints = cmd::hint::build_hints(self);
                let state =
                    if let Mode::Command(s) = std::mem::replace(&mut self.mode, Mode::Normal) {
                        s
                    } else {
                        return KeyResult::Continue;
                    };
                let typed = state.buffer.trim().to_string();
                if typed.is_empty() {
                    return KeyResult::Continue;
                }
                // Pick the buffer to actually run. If the typed text already
                // resolves to a known command, run it. Otherwise, if there's a
                // highlighted hint that completes a command name, apply it
                // (matches user expectation: "press Enter on the highlighted
                // suggestion"). Falls back to typed on no hints.
                let parses_cleanly = cmd::parse::parse(&typed, cmd::registry::ALL).is_ok();
                let buf = if parses_cleanly {
                    typed.clone()
                } else if let Some(hint) =
                    hints.get(state.selected.min(hints.len().saturating_sub(1)))
                {
                    apply_hint_to_buffer(&state.buffer, hint).trim().to_string()
                } else {
                    typed.clone()
                };
                // Push the *typed* form into history (so users can recall what
                // they actually pressed, not the auto-completed expansion).
                if self.command_history.back().map(|s| s.as_str()) != Some(typed.as_str()) {
                    self.command_history.push_back(typed.clone());
                    while self.command_history.len() > COMMAND_HISTORY_CAP {
                        self.command_history.pop_front();
                    }
                }
                let outcome = cmd::exec::run(self, &buf);
                match outcome {
                    cmd::exec::Outcome::Continue => {}
                    cmd::exec::Outcome::Quit => return KeyResult::Quit,
                    cmd::exec::Outcome::Reconnect => return KeyResult::Reconnect,
                    cmd::exec::Outcome::SwitchServer(t) => return KeyResult::SwitchServer(t),
                }
            }

            Action::None => {}
        }

        KeyResult::Continue
    }

    /// Move keyboard focus to the tiled pane in `dir` from the focused pane,
    /// resolving the active tab's layout against the current content size and
    /// picking the geometric neighbor.
    fn focus_dir(&mut self, dir: crate::layout::FocusDir) {
        let Some(layout) = self.mgr.active_layout().cloned() else {
            return;
        };
        let Some(focused) = self
            .mgr
            .active_pane_id()
            .and_then(|p| p.rsplit_once('/'))
            .and_then(|(_, i)| i.parse::<u32>().ok())
        else {
            return;
        };
        let rects = crate::layout::resolve_layout(
            &layout,
            self.term_size.cols,
            self.term_size.rows,
            &crate::layout::LayoutConfig::default(),
        );
        if let Some(target) = crate::layout::focus_neighbor(&rects, focused, dir)
            && let Some(word) = self.mgr.active_session().map(|s| s.to_string())
        {
            self.mgr.focus_pane(format!("{word}/{target}"));
        }
    }

    /// Focus the `index`-th pane (0-based) in the active tab's leaf order
    /// (depth-first, left-to-right — the order the tiles are laid out). No-op
    /// when there is no such pane.
    fn focus_pane_at(&mut self, index: u32) {
        let Some(pane_index) = self
            .mgr
            .active_layout()
            .and_then(|l| l.leaves().get(index as usize).copied())
        else {
            return;
        };
        if let Some(word) = self.mgr.active_session().map(|s| s.to_string()) {
            self.mgr.focus_pane(format!("{word}/{pane_index}"));
        }
    }

    /// Resize the focused pane's enclosing split in `dir` by one step. Computes
    /// the new ratios from the shared (resolution-independent) tree and sends
    /// them to the server, which clamps, renormalizes, and broadcasts the
    /// authoritative `LayoutUpdate` back to every client viewing the tab.
    fn resize(&mut self, dir: crate::layout::FocusDir) {
        let Some(layout) = self.mgr.active_layout().cloned() else {
            return;
        };
        let Some(focused) = self
            .mgr
            .active_pane_id()
            .and_then(|p| p.rsplit_once('/'))
            .and_then(|(_, i)| i.parse::<u32>().ok())
        else {
            return;
        };
        if let Some((path, ratios)) =
            crate::layout::resize_split(&layout, focused, dir, crate::layout::RESIZE_STEP_PERMILLE)
        {
            self.mgr.set_layout_ratios(path, ratios);
        }
    }

    fn command_hint_up(&mut self) {
        let state = match &mut self.mode {
            Mode::Command(s) => s,
            _ => return,
        };
        if state.selected > 0 {
            state.selected -= 1;
        }
    }

    fn command_hint_down(&mut self) {
        let count = cmd::hint::build_hints(self).len();
        if let Mode::Command(state) = &mut self.mode
            && count > 0
            && state.selected + 1 < count
        {
            state.selected += 1;
        }
    }

    fn command_apply_completion(&mut self) {
        let hints = cmd::hint::build_hints(self);
        let Mode::Command(state) = &mut self.mode else {
            return;
        };
        let idx = state.selected.min(hints.len().saturating_sub(1));
        let Some(hint) = hints.get(idx) else {
            return;
        };
        state.buffer = apply_hint_to_buffer(&state.buffer, hint);
        state.cursor = state.buffer.len();
        state.selected = 0;
        state.history_pos = None;
    }

    // ── Pointer-driven interaction policy ────────────────────────────────────
    //
    // Top-bar clicks and picker-item clicks do not pass through `mode::resolve`,
    // so these methods perform their own mode transitions. They are the single
    // source of truth for that behavior, shared by every frontend (the TUI
    // hit-tests a click to one of these; a GUI binds it to a widget) so no
    // frontend re-implements it.

    /// Apply the session-picker selection. Index 0 is the synthetic
    /// "[+] New session" affordance (hands off to the directory picker so the
    /// user picks a path); indices 1..N select the matching filtered session.
    /// Shared by the keyboard `SelectPickerEntry` action and the pointer-driven
    /// [`activate_picker_selection`](Self::activate_picker_selection).
    fn select_session_picker_entry(&mut self) {
        if self.session_picker_selected == 0 {
            self.dir_picker_buffer = self.initial_cwd.clone();
            self.dir_picker_selected = 0;
            self.mode = Mode::DirectoryPicker;
            return;
        }
        let word_id = self
            .session_picker_matches()
            .get(self.session_picker_selected - 1)
            .map(|e| e.meta.word_id.clone());
        if let Some(word_id) = word_id {
            self.mgr.select_session(word_id);
        }
        self.mode = Mode::Normal;
    }

    /// The switch target for the current server-picker selection, or `None` when
    /// nothing is selected or the selection is the already-connected server (in
    /// which case the caller just closes the picker). Shared by the keyboard
    /// `ServerPickerSelect` action and the pointer-driven activation.
    fn server_picker_switch_target(&self) -> Option<SwitchTarget> {
        let server = self
            .filtered_servers()
            .get(self.server_picker_selected)
            .cloned()?;
        if server.server_string == self.server_string {
            return None;
        }
        Some(match server.kind {
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
        })
    }

    /// Apply a clickable top-bar action (server/connection/session badges, pane
    /// tabs, the `+` button). Frontend-neutral: the TUI hit-tests a column to a
    /// [`TopBarAction`], a GUI binds it to a widget; both then call this.
    /// Returns a [`KeyResult`] for actions that must reach the run loop.
    pub fn apply_top_bar_action(&mut self, action: TopBarAction) -> Option<KeyResult> {
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
                self.mgr.create_pane(self.term_size);
                None
            }
        }
    }

    /// Set the selected index of the currently open picker (e.g. hover-to-
    /// highlight). No-op when no picker is open.
    pub fn set_picker_selected(&mut self, idx: usize) {
        match self.mode {
            Mode::SessionPicker => self.session_picker_selected = idx,
            Mode::ServerPicker => self.server_picker_selected = idx,
            Mode::DirectoryPicker => self.dir_picker_selected = idx,
            _ => {}
        }
    }

    /// Set the active picker's search/filter text in one shot (from a native text
    /// entry that owns its own editing), resetting the selection to the first row
    /// — the same reset the per-keystroke `PickerSearchChar`/`DirPickerChar`
    /// actions perform. No-op when no picker is open. Lets a GUI drive the
    /// pickers without routing every character through the action path.
    pub fn set_picker_search(&mut self, text: String) {
        match self.mode {
            Mode::SessionPicker => {
                self.session_picker_search = text;
                self.session_picker_selected = 0;
            }
            Mode::ServerPicker => {
                self.server_picker_search = text;
                self.server_picker_selected = 0;
            }
            Mode::DirectoryPicker => {
                self.dir_picker_buffer = text;
                self.dir_picker_selected = 0;
            }
            _ => {}
        }
    }

    /// Activate the current picker's selection (a click on a list item). Mirrors
    /// the keyboard Enter path but performs its own mode transition because it
    /// does not pass through `mode::resolve`. Returns a [`KeyResult`] only for
    /// the server picker, which may switch servers. Note: a directory-picker
    /// click only *selects an existing* session — creating from a typed path is
    /// the keyboard `DirPickerSubmit` path.
    pub fn activate_picker_selection(&mut self) -> Option<KeyResult> {
        match self.mode {
            Mode::SessionPicker => {
                self.select_session_picker_entry();
                None
            }
            Mode::ServerPicker => {
                let target = self.server_picker_switch_target();
                self.mode = Mode::Normal;
                target.map(KeyResult::SwitchServer)
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
}

/// Apply a hint's replacement to a buffer, returning the resulting buffer.
/// Shared by Tab (live edit) and Enter (submit-with-fallback when the typed
/// buffer doesn't parse to a known command).
pub(crate) fn apply_hint_to_buffer(buffer: &str, hint: &cmd::hint::Hint) -> String {
    let split = hint.replace_from.min(buffer.len());
    let head = &buffer[..split];
    if hint.append_space {
        format!("{head}{} ", hint.replacement)
    } else {
        format!("{head}{}", hint.replacement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::hint::Hint;
    use kmux_client::session_manager::SessionManager;
    use kmux_protocol::messages::ClientCapabilities;

    fn fixture_core() -> AppCore {
        let mgr = SessionManager::new(
            "127.0.0.1".into(),
            0,
            String::new(),
            true,
            ClientCapabilities::default(),
        );
        AppCore::for_test(mgr)
    }

    #[tokio::test]
    async fn rename_tab_mode_edits_buffer_and_submits_to_normal() {
        let mut core = fixture_core();
        core.mode = Mode::RenameTab {
            word_id: "w".into(),
            tab_index: 3,
            buffer: String::new(),
        };
        core.dispatch_action(Action::RenameChar('h')).await;
        core.dispatch_action(Action::RenameChar('i')).await;
        match &core.mode {
            Mode::RenameTab {
                buffer, tab_index, ..
            } => {
                assert_eq!(buffer, "hi");
                assert_eq!(*tab_index, 3);
            }
            other => panic!("expected RenameTab, got {other:?}"),
        }
        core.dispatch_action(Action::RenameBackspace).await;
        if let Mode::RenameTab { buffer, .. } = &core.mode {
            assert_eq!(buffer, "h");
        }
        // Submitting a non-empty name leaves the rename mode for Normal.
        core.dispatch_action(Action::RenameSubmit).await;
        assert_eq!(core.mode, Mode::Normal);
    }

    #[test]
    fn apply_hint_replaces_trailing_token() {
        let h = Hint {
            display: String::new(),
            summary: "",
            replacement: "session new".into(),
            replace_from: 0,
            append_space: true,
        };
        assert_eq!(apply_hint_to_buffer("sess", &h), "session new ");
    }

    #[test]
    fn apply_hint_at_end_of_buffer() {
        let h = Hint {
            display: String::new(),
            summary: "",
            replacement: "dracula".into(),
            replace_from: 6, // after "theme "
            append_space: true,
        };
        assert_eq!(apply_hint_to_buffer("theme ", &h), "theme dracula ");
    }

    #[test]
    fn apply_hint_no_trailing_space_when_append_false() {
        let h = Hint {
            display: String::new(),
            summary: "",
            replacement: "quit".into(),
            replace_from: 0,
            append_space: false,
        };
        assert_eq!(apply_hint_to_buffer("qu", &h), "quit");
    }

    // ── Pointer-driven interaction policy ────────────────────────────────────

    #[test]
    fn top_bar_reconnect_propagates_keyresult() {
        let mut core = fixture_core();
        assert!(matches!(
            core.apply_top_bar_action(TopBarAction::Reconnect),
            Some(KeyResult::Reconnect)
        ));
    }

    #[test]
    fn top_bar_open_server_picker_resets_state_and_enters_mode() {
        let mut core = fixture_core();
        core.server_picker_search = "stale".into();
        core.server_picker_selected = 4;
        assert!(
            core.apply_top_bar_action(TopBarAction::OpenServerPicker)
                .is_none()
        );
        assert_eq!(core.mode, Mode::ServerPicker);
        assert!(core.server_picker_search.is_empty());
        assert_eq!(core.server_picker_selected, 0);
    }

    #[test]
    fn top_bar_open_session_picker_resets_state_and_enters_mode() {
        let mut core = fixture_core();
        core.session_picker_search = "stale".into();
        core.session_picker_selected = 2;
        assert!(
            core.apply_top_bar_action(TopBarAction::OpenSessionPicker)
                .is_none()
        );
        assert_eq!(core.mode, Mode::SessionPicker);
        assert!(core.session_picker_search.is_empty());
        assert_eq!(core.session_picker_selected, 0);
    }

    #[test]
    fn set_picker_selected_targets_the_active_picker_only() {
        let mut core = fixture_core();
        core.mode = Mode::ServerPicker;
        core.set_picker_selected(3);
        assert_eq!(core.server_picker_selected, 3);

        core.mode = Mode::SessionPicker;
        core.set_picker_selected(2);
        assert_eq!(core.session_picker_selected, 2);

        // Outside a picker it is a no-op (does not clobber a picker index).
        core.mode = Mode::Normal;
        core.set_picker_selected(9);
        assert_eq!(core.session_picker_selected, 2);
        assert_eq!(core.server_picker_selected, 3);
    }

    #[test]
    fn set_picker_search_targets_active_picker_and_resets_selection() {
        let mut core = fixture_core();

        core.mode = Mode::SessionPicker;
        core.session_picker_selected = 5;
        core.set_picker_search("foo".into());
        assert_eq!(core.session_picker_search, "foo");
        assert_eq!(core.session_picker_selected, 0);

        core.mode = Mode::ServerPicker;
        core.server_picker_selected = 3;
        core.set_picker_search("bar".into());
        assert_eq!(core.server_picker_search, "bar");
        assert_eq!(core.server_picker_selected, 0);

        core.mode = Mode::DirectoryPicker;
        core.dir_picker_selected = 2;
        core.set_picker_search("/tmp".into());
        assert_eq!(core.dir_picker_buffer, "/tmp");
        assert_eq!(core.dir_picker_selected, 0);
    }

    #[test]
    fn set_picker_search_is_noop_outside_a_picker() {
        let mut core = fixture_core();
        core.mode = Mode::Normal;
        core.set_picker_search("x".into());
        assert!(core.session_picker_search.is_empty());
        assert!(core.server_picker_search.is_empty());
        assert!(core.dir_picker_buffer.is_empty());
    }

    #[test]
    fn activate_session_picker_index_zero_opens_directory_picker() {
        let mut core = fixture_core();
        core.mode = Mode::SessionPicker;
        core.session_picker_selected = 0;
        core.initial_cwd = "/home/u/proj".into();
        assert!(core.activate_picker_selection().is_none());
        assert_eq!(core.mode, Mode::DirectoryPicker);
        assert_eq!(core.dir_picker_buffer, "/home/u/proj");
        assert_eq!(core.dir_picker_selected, 0);
    }

    #[test]
    fn activate_outside_a_picker_is_noop() {
        let mut core = fixture_core();
        core.mode = Mode::Normal;
        assert!(core.activate_picker_selection().is_none());
        assert_eq!(core.mode, Mode::Normal);
    }
}
