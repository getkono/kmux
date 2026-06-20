//! Action dispatch on [`AppCore`]: the single source of truth for how a
//! resolved [`Action`] mutates client state, shared by the key path and the
//! command palette.
//!
//! Two arms that require toolkit I/O are *not* here: `Action::ForwardKey`
//! (needs the raw toolkit key event to encode under live Ghostty mode state —
//! the frontend handles it before calling dispatch) and clipboard copy/paste
//! (emitted as [`KeyResult::CopyToClipboard`] / [`KeyResult::RequestPaste`]
//! effects that the frontend performs).

use std::time::Instant;

use crate::cmd;
use crate::mode::{Action, Mode};

use super::{
    AppCore, COMMAND_HISTORY_CAP, DirBrowserRow, KeyResult, LaunchRow, PendingClose,
    SOFT_CLOSE_GRACE, TopBarAction,
};

impl AppCore {
    /// Apply an [`Action`] to the core. Used both by the key path and by the
    /// command palette so a single source of truth governs behavior.
    pub async fn dispatch_action(&mut self, action: Action) -> KeyResult {
        match action {
            // ForwardKey is handled frontend-side (it needs the raw toolkit
            // event); it never reaches the core dispatch.
            Action::ForwardKey => {}
            Action::CreateSession => {
                // Never assume where a new session opens: default to the focused
                // session's cwd, falling back to the app's initial cwd. A bare
                // create with no cwd would resolve against the *daemon's* working
                // directory, not the user's.
                let cwd = self
                    .active_session_cwd()
                    .unwrap_or_else(|| self.initial_cwd.clone());
                self.mgr.create_session(None, Some(&cwd), self.term_size);
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
                self.soft_close_active_pane();
            }
            Action::UndoClose => {
                self.undo_soft_close();
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
            Action::ToggleProcessOverview => {
                self.toggle_process_overview();
            }
            Action::ToggleConnection => {
                self.connection_overlay_visible = !self.connection_overlay_visible;
            }
            Action::ToggleRenderDebug => {
                self.render_debug_visible = !self.render_debug_visible;
            }
            Action::ResetRenderer => {
                tracing::info!(
                    target: "kmux::render_debug",
                    "ResetRenderer requested: rebuilding renderer + atlas, full repaint"
                );
                // Force a full re-pack/repaint; the frontend rebuilds its own
                // renderer/atlas on the resulting effect (it owns that object).
                self.force_clear = true;
                return KeyResult::ResetRenderer;
            }
            Action::ToggleSnapshotMode => {
                self.force_snapshot_mode = !self.force_snapshot_mode;
                self.mgr.set_snapshot_mode(self.force_snapshot_mode);
            }
            Action::ToggleInputLock => {
                self.mgr.toggle_input_lock();
            }
            Action::TogglePause => {
                self.toggle_manual_pause();
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
                let count = self.dir_browser_rows().len();
                if count > 0 && self.dir_picker_selected + 1 < count {
                    self.dir_picker_selected += 1;
                }
            }
            Action::DirPickerSubmit => {
                self.submit_dir_browser_row();
            }
            Action::DirPickerCancel => {}
            Action::LaunchSearchChar(ch) => {
                self.launch_search.push(ch);
                self.launch_selected = 0;
            }
            Action::LaunchSearchBackspace => {
                self.launch_search.pop();
                self.launch_selected = 0;
            }
            Action::LaunchUp => {
                self.launch_selected = self.launch_selected.saturating_sub(1);
            }
            Action::LaunchDown => {
                let count = self.launch_rows().len();
                if count > 0 && self.launch_selected + 1 < count {
                    self.launch_selected += 1;
                }
            }
            Action::LaunchSelect => {
                self.submit_launch_row();
            }
            Action::LaunchClose | Action::LaunchOverlayCancel => {
                self.mode = Mode::Normal;
            }
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
            // "+ New session" opens the directory browser (seeded at the active
            // session's cwd) so the user picks where the session is created.
            self.open_directory_browser();
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

    /// Act on the directory browser's currently-selected row (Enter / click /
    /// activate). Shared by the keyboard [`Action::DirPickerSubmit`] and the
    /// pointer-driven [`activate_picker_selection`](Self::activate_picker_selection):
    ///
    /// - [`DirBrowserRow::CreateHere`] creates a session in the browsed
    ///   directory and returns to [`Mode::Normal`].
    /// - [`DirBrowserRow::Up`] / [`DirBrowserRow::Enter`] navigate (request a
    ///   fresh listing for the target) and keep the browser open.
    ///
    /// Power feature: when the filter is a non-empty **absolute** path that does
    /// not match any listed row, Enter navigates to that typed path instead.
    fn submit_dir_browser_row(&mut self) {
        let rows = self.dir_browser_rows();
        match rows.get(self.dir_picker_selected).cloned() {
            Some(DirBrowserRow::CreateHere { cwd }) => {
                // A typed absolute path that isn't a listed subdir is treated as
                // "navigate there" rather than "create in the current dir", so a
                // user can jump straight to a path they typed.
                let typed = self.dir_picker_buffer.trim();
                if typed.starts_with('/') && !self.filter_matches_listed_subdir() {
                    self.navigate_directory_browser(typed.to_string());
                    return;
                }
                self.mgr.create_session(None, Some(&cwd), self.term_size);
                self.mode = Mode::Normal;
            }
            Some(DirBrowserRow::Up { parent }) => self.navigate_directory_browser(parent),
            Some(DirBrowserRow::Enter { path, .. }) => self.navigate_directory_browser(path),
            None => {}
        }
    }

    /// Open the unified session launcher (issue #121), resetting its filter and
    /// selection. The entry point for the GUI new-session button.
    pub fn open_launch_picker(&mut self) {
        self.launch_selected = 0;
        self.launch_search.clear();
        self.mode = Mode::LaunchPicker;
    }

    /// Open the process overview (issue #122) and fire an immediate snapshot
    /// request so the view has data before the driver's periodic poll kicks in.
    pub fn open_process_overview(&mut self) {
        self.mode = Mode::ProcessOverview;
        self.mgr.request_process_overview();
    }

    /// Toggle the process overview: open it (with an immediate refresh) when
    /// elsewhere, or return to the terminal when already open.
    pub fn toggle_process_overview(&mut self) {
        if matches!(self.mode, Mode::ProcessOverview) {
            self.mode = Mode::Normal;
        } else {
            self.open_process_overview();
        }
    }

    /// Open the add-a-remote form. The frontend renders native fields and calls
    /// [`AppCore::submit_add_remote`].
    pub fn open_add_remote(&mut self) {
        self.mode = Mode::AddRemote;
    }

    /// Open the "new session on a remote" path prompt for `peer`. The frontend
    /// renders the field and calls [`AppCore::submit_remote_new_session`].
    pub fn open_remote_new_session(&mut self, peer: String) {
        self.mode = Mode::RemoteNewSession { peer };
    }

    /// Activate the selected launcher row (issue #121). Mirrors the dir-browser
    /// submit: existing-session rows attach and close; the local-new row opens
    /// the directory browser; a remote header toggles expand (connecting on
    /// focus); the remote-new and add-remote rows open their forms.
    fn submit_launch_row(&mut self) {
        let rows = self.launch_rows();
        let Some(row) = rows.get(self.launch_selected).cloned() else {
            return;
        };
        match row {
            LaunchRow::LocalNewSession { .. } => {
                // The directory browser already creates a local session at the
                // chosen directory, seeded from the focused session's cwd.
                self.open_directory_browser();
            }
            LaunchRow::LocalExisting { word_id, .. }
            | LaunchRow::RemoteExisting { word_id, .. } => {
                self.mgr.select_session(word_id);
                self.mode = Mode::Normal;
            }
            LaunchRow::Remote { peer, expanded, .. } => {
                if expanded {
                    self.collapse_remote(&peer);
                } else {
                    self.expand_remote(peer);
                }
                // Stay in the launcher so the section visibly expands/collapses.
                self.mode = Mode::LaunchPicker;
            }
            LaunchRow::RemoteNewSession { peer } => self.open_remote_new_session(peer),
            LaunchRow::AddRemote => self.open_add_remote(),
        }
    }

    /// Whether the current filter text matches at least one listed subdirectory
    /// (used to decide whether Enter on a typed absolute path should navigate).
    fn filter_matches_listed_subdir(&self) -> bool {
        self.dir_browser_rows()
            .iter()
            .any(|r| matches!(r, DirBrowserRow::Enter { .. }))
    }

    /// Apply a clickable top-bar action (server/connection/session badges, pane
    /// tabs, the `+` button). Frontend-neutral: the TUI hit-tests a column to a
    /// [`TopBarAction`], a GUI binds it to a widget; both then call this.
    /// Returns a [`KeyResult`] for actions that must reach the run loop.
    pub fn apply_top_bar_action(&mut self, action: TopBarAction) -> Option<KeyResult> {
        match action {
            TopBarAction::Reconnect => Some(KeyResult::Reconnect),
            TopBarAction::OpenSessionPicker => {
                self.session_picker_selected = 0;
                self.session_picker_search.clear();
                self.mode = Mode::SessionPicker;
                None
            }
            TopBarAction::OpenLaunchPicker => {
                self.open_launch_picker();
                None
            }
            TopBarAction::SelectPane(pane_id) => {
                // Re-selecting a pane within its soft-close window restores it:
                // the live shell was never killed, so just cancel the close (#86).
                self.cancel_pending_close(&pane_id);
                self.mgr.select_pane(pane_id);
                None
            }
            TopBarAction::CreatePane => {
                self.mgr.create_pane(self.term_size);
                None
            }
        }
    }

    // ── Soft-close (issue #86) ────────────────────────────────────────────────

    /// Request a deferred ("soft") close of the active pane. A healthy pane's
    /// `PaneClose` is withheld for [`SOFT_CLOSE_GRACE`] so an accidental close
    /// can be undone; an already-exited pane closes immediately. Re-requesting a
    /// close that is already pending keeps the existing deadline.
    pub fn soft_close_active_pane(&mut self) {
        let Some(pane_id) = self.mgr.active_pane_id().map(str::to_string) else {
            return;
        };
        if self.pending_closes.iter().any(|p| p.pane_id == pane_id) {
            return;
        }
        // An unhealthy (already-exited) pane has no live shell to protect.
        if !self.mgr.is_pane_running(&pane_id) {
            self.mgr.close_pane_id(&pane_id);
            return;
        }
        self.pending_closes.push(PendingClose {
            pane_id,
            deadline: Instant::now() + SOFT_CLOSE_GRACE,
        });
        self.soft_close_nonce = self.soft_close_nonce.wrapping_add(1);
        self.mgr
            .set_status_msg("Closing pane in 3s — undo to keep it".into());
    }

    /// Cancel the most recently scheduled soft-close (the toast/keyboard "Undo").
    /// The live shell was never touched, so the pane simply stays.
    pub fn undo_soft_close(&mut self) {
        if self.pending_closes.pop().is_some() {
            self.mgr.set_status_msg("Pane close cancelled".into());
        }
    }

    /// Cancel a specific pane's pending soft-close (e.g. the user re-focused it
    /// within the window). Returns whether one was cancelled.
    pub fn cancel_pending_close(&mut self, pane_id: &str) -> bool {
        let before = self.pending_closes.len();
        self.pending_closes.retain(|p| p.pane_id != pane_id);
        self.pending_closes.len() != before
    }

    /// Fire every soft-close whose grace window has elapsed (driven by the
    /// frontend pump). Returns whether any close was sent, so the caller can
    /// schedule a render. Cheap when nothing is pending.
    pub fn fire_due_closes(&mut self, now: Instant) -> bool {
        if self.pending_closes.is_empty() {
            return false;
        }
        let mut due: Vec<String> = Vec::new();
        self.pending_closes.retain(|p| {
            let expired = p.deadline <= now;
            if expired {
                due.push(p.pane_id.clone());
            }
            !expired
        });
        for pane_id in &due {
            self.mgr.close_pane_id(pane_id);
        }
        !due.is_empty()
    }

    /// Whether `pane_id` is awaiting a deferred close (for a "closing…" hint).
    pub fn is_pane_pending_close(&self, pane_id: &str) -> bool {
        self.pending_closes.iter().any(|p| p.pane_id == pane_id)
    }

    /// Whether any pane is awaiting a deferred close (drives the Undo affordance).
    pub fn has_pending_close(&self) -> bool {
        !self.pending_closes.is_empty()
    }

    /// Open the command palette pre-filled with `transport ` so the user picks
    /// from the completer (Auto + each protocol). Bound to the protocol
    /// indicator (double-click) in the GUIs (issue #69).
    pub fn open_transport_chooser(&mut self) {
        const PREFILL: &str = "transport ";
        self.mode = Mode::Command(crate::mode::CommandState {
            buffer: PREFILL.to_string(),
            cursor: PREFILL.len(),
            selected: 0,
            history_pos: None,
        });
    }

    /// Set the selected index of the currently open picker (e.g. hover-to-
    /// highlight). No-op when no picker is open.
    pub fn set_picker_selected(&mut self, idx: usize) {
        match self.mode {
            Mode::SessionPicker => self.session_picker_selected = idx,
            Mode::DirectoryPicker => self.dir_picker_selected = idx,
            Mode::LaunchPicker => self.launch_selected = idx,
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
            Mode::DirectoryPicker => {
                self.dir_picker_buffer = text;
                self.dir_picker_selected = 0;
            }
            Mode::LaunchPicker => {
                self.launch_search = text;
                self.launch_selected = 0;
            }
            _ => {}
        }
    }

    /// Activate the current picker's selection (a click on a list item). Mirrors
    /// the keyboard Enter path but performs its own mode transition because it
    /// does not pass through `mode::resolve`. For the directory browser this
    /// navigates (Up / into a subdir) or creates a session (create-here),
    /// identical to the keyboard `DirPickerSubmit` path; the launcher submits its
    /// row. Returns a [`KeyResult`] only when an activation reaches the run loop.
    pub fn activate_picker_selection(&mut self) -> Option<KeyResult> {
        match self.mode {
            Mode::SessionPicker => {
                self.select_session_picker_entry();
                None
            }
            Mode::DirectoryPicker => {
                // A click on a browser row: create-here returns to Normal;
                // navigating into a folder keeps the browser open (it refreshes
                // in place when the new listing arrives).
                self.submit_dir_browser_row();
                None
            }
            Mode::LaunchPicker => {
                self.submit_launch_row();
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

    /// A connected core with one active pane carrying `status` (issue #86 tests).
    fn core_with_active_pane(status: kmux_protocol::messages::SessionStatus) -> AppCore {
        use kmux_protocol::messages::{
            LayoutNode, PaneInfo, SessionEntry, SessionMeta, TabInfo, TermSize,
        };
        let mut core = fixture_core();
        core.mgr.connected = true;
        core.mgr.session_list = vec![SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: "eagle".into(),
                name: "eagle".into(),
                cwd: "/".into(),
            },
            panes: vec![PaneInfo {
                pane_id: "eagle/0".into(),
                pane_index: 0,
                program: String::new(),
                size: TermSize::default(),
                attached_clients: vec![],
                status,
                title: String::new(),
            }],
            tabs: vec![TabInfo {
                tab_index: 0,
                name: "1".into(),
                layout: LayoutNode::single(0),
                focused_pane: 0,
            }],
            active_tab: 0,
            peer: None,
        }];
        core.mgr.active_pane = Some("eagle/0".into());
        core
    }

    #[tokio::test]
    async fn close_pane_defers_then_undo_keeps_the_pane() {
        use kmux_protocol::messages::SessionStatus;
        let mut core = core_with_active_pane(SessionStatus::Running);
        let nonce = core.soft_close_nonce;

        // Close → deferred, not killed immediately.
        core.dispatch_action(Action::ClosePane).await;
        assert!(core.is_pane_pending_close("eagle/0"));
        assert_eq!(core.soft_close_nonce, nonce + 1);

        // A second close keeps the existing deadline (no duplicate).
        core.dispatch_action(Action::ClosePane).await;
        assert_eq!(core.pending_closes.len(), 1);

        // Undo → cancelled; the shell was never touched.
        core.dispatch_action(Action::UndoClose).await;
        assert!(!core.has_pending_close());
    }

    #[tokio::test]
    async fn toggle_render_debug_flips_flag() {
        let mut core = fixture_core();
        assert!(!core.render_debug_visible);
        core.dispatch_action(Action::ToggleRenderDebug).await;
        assert!(core.render_debug_visible);
        core.dispatch_action(Action::ToggleRenderDebug).await;
        assert!(!core.render_debug_visible);
    }

    #[tokio::test]
    async fn reset_renderer_signals_keyresult_and_forces_clear() {
        let mut core = fixture_core();
        let result = core.dispatch_action(Action::ResetRenderer).await;
        assert!(matches!(result, KeyResult::ResetRenderer));
        assert!(core.force_clear); // full re-pack/repaint on the next tick
    }

    #[test]
    fn soft_close_fires_only_after_the_grace_window() {
        use kmux_protocol::messages::SessionStatus;
        let mut core = core_with_active_pane(SessionStatus::Running);
        core.soft_close_active_pane();
        assert!(core.has_pending_close());
        // Before the deadline: nothing fires.
        assert!(!core.fire_due_closes(Instant::now()));
        assert!(core.has_pending_close());
        // After the grace window: the close fires and the pending list drains.
        let later = Instant::now() + SOFT_CLOSE_GRACE + std::time::Duration::from_millis(1);
        assert!(core.fire_due_closes(later));
        assert!(!core.has_pending_close());
    }

    #[test]
    fn exited_pane_closes_immediately_without_grace() {
        use kmux_protocol::messages::SessionStatus;
        let mut core = core_with_active_pane(SessionStatus::Exited {
            code: Some(0),
            signal: None,
        });
        core.soft_close_active_pane();
        // An already-dead shell needs no grace: no pending close is scheduled.
        assert!(!core.has_pending_close());
    }

    #[test]
    fn reselecting_a_pane_cancels_its_pending_close() {
        use kmux_protocol::messages::SessionStatus;
        let mut core = core_with_active_pane(SessionStatus::Running);
        core.soft_close_active_pane();
        assert!(core.is_pane_pending_close("eagle/0"));
        // "Re-opening" the pane within the window restores it.
        core.apply_top_bar_action(TopBarAction::SelectPane("eagle/0".into()));
        assert!(!core.has_pending_close());
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
        core.mode = Mode::LaunchPicker;
        core.set_picker_selected(3);
        assert_eq!(core.launch_selected, 3);

        core.mode = Mode::SessionPicker;
        core.set_picker_selected(2);
        assert_eq!(core.session_picker_selected, 2);

        // Outside a picker it is a no-op (does not clobber a picker index).
        core.mode = Mode::Normal;
        core.set_picker_selected(9);
        assert_eq!(core.session_picker_selected, 2);
        assert_eq!(core.launch_selected, 3);
    }

    #[test]
    fn set_picker_search_targets_active_picker_and_resets_selection() {
        let mut core = fixture_core();

        core.mode = Mode::SessionPicker;
        core.session_picker_selected = 5;
        core.set_picker_search("foo".into());
        assert_eq!(core.session_picker_search, "foo");
        assert_eq!(core.session_picker_selected, 0);

        core.mode = Mode::LaunchPicker;
        core.launch_selected = 3;
        core.set_picker_search("bar".into());
        assert_eq!(core.launch_search, "bar");
        assert_eq!(core.launch_selected, 0);

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
        assert!(core.launch_search.is_empty());
        assert!(core.dir_picker_buffer.is_empty());
    }

    #[test]
    fn activate_session_picker_index_zero_opens_directory_browser() {
        // Activating the synthetic "+ New session" row opens the directory
        // browser. With no active session it seeds the browse dir from
        // initial_cwd and starts row 0 with an empty filter.
        let mut core = fixture_core();
        core.mode = Mode::SessionPicker;
        core.session_picker_selected = 0;
        core.initial_cwd = "/home/u/proj".into();
        assert!(core.activate_picker_selection().is_none());
        assert_eq!(core.mode, Mode::DirectoryPicker);
        assert_eq!(core.dir_browser_cwd, "/home/u/proj");
        assert!(core.dir_picker_buffer.is_empty());
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
