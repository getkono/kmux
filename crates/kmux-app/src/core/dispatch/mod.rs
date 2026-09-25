//! Action dispatch on [`AppCore`]: the single source of truth for how a
//! resolved [`Action`] mutates client state, shared by the key path and the
//! command palette.
//!
//! Two arms that require toolkit I/O are *not* here: `Action::ForwardKey`
//! (needs the raw toolkit key event to encode under live Ghostty mode state —
//! the frontend handles it before calling dispatch) and clipboard copy/paste
//! (emitted as [`KeyResult::CopyToClipboard`] / [`KeyResult::RequestPaste`]
//! effects that the frontend performs).

mod browse;
mod command;
mod pane;
mod picker;
mod rename;
mod render;
mod scroll;
mod session;

use std::time::Instant;

use kmux_protocol::{format_pane_id, pane_index};

use crate::cmd;
use crate::mode::{Action, Mode};

use super::{
    AppCore, DirBrowserRow, KeyResult, LaunchRow, PendingClose, SOFT_CLOSE_GRACE, TopBarAction,
};

impl AppCore {
    /// Apply an [`Action`] to the core. Used both by the key path and by the
    /// command palette so a single source of truth governs behavior.
    ///
    /// Synchronous. It was `async` for its whole life without ever awaiting
    /// anything: every effect it produces is a `KeyResult` the caller acts on,
    /// and the one path that needs a runtime (`FrontendDriver::reconnect` ->
    /// `start_bootstrap`) spawns from the driver, above this. The `async` cost
    /// 14 `block_on` calls in kmux-gtk, 6 in kmux-ffi, and a dependency.
    pub fn dispatch_action(&mut self, action: Action) -> KeyResult {
        match action {
            // ForwardKey is handled frontend-side (it needs the raw toolkit
            // event); it never reaches the core dispatch.
            Action::ForwardKey => {}
            Action::CreateSession => return self.on_create_session(),
            Action::CreatePane => {
                self.mgr.create_pane(self.term_size);
            }
            Action::CloseSession => {
                self.confirm_close_active_session();
            }
            Action::ConfirmCloseSession => return self.on_confirm_close_session(),
            Action::ClosePane => {
                self.soft_close_active_pane();
            }
            Action::UndoClose => {
                self.undo_soft_close();
            }
            Action::NextSession => self.mgr.cycle_session(1),
            Action::PrevSession => self.mgr.cycle_session(-1),
            Action::NextTab => self.mgr.cycle_tab(1),
            Action::PrevTab => self.mgr.cycle_tab(-1),
            Action::NextPaneInTab => self.cycle_pane_in_tab(1),
            Action::PrevPaneInTab => self.cycle_pane_in_tab(-1),
            Action::CloseTab => self.mgr.close_tab(),
            Action::RenameTab => return self.on_rename_tab(),
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
            Action::JumpToSession(idx) => return self.on_jump_to_session(idx),
            Action::RenameSession => return self.on_rename_session(),
            Action::RenameChar(ch) => return self.on_rename_char(ch),
            Action::RenameBackspace => return self.on_rename_backspace(),
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
            Action::PickerUp => return self.on_picker_up(),
            Action::PickerDown => return self.on_picker_down(),
            Action::PickerSearchChar(ch) => return self.on_picker_search_char(ch),
            Action::PickerSearchBackspace => return self.on_picker_search_backspace(),
            Action::Disconnect => return self.on_disconnect(),
            Action::SendSignal(signal) => return self.on_send_signal(signal),
            Action::ScrollUp(n) => return self.on_scroll_up(n),
            Action::ScrollDown(n) => return self.on_scroll_down(n),
            Action::ScrollPageUp => return self.on_scroll_page_up(),
            Action::ScrollPageDown => return self.on_scroll_page_down(),
            Action::ToggleHud => {
                self.hud_visible = !self.hud_visible;
            }
            Action::ToggleMetrics => {
                self.metrics_overlay_visible = !self.metrics_overlay_visible;
            }
            Action::ToggleProcessOverview => {
                self.toggle_process_overview();
            }
            Action::ToggleConnectedClients => {
                self.toggle_connected_clients();
            }
            Action::ToggleConnection => {
                self.connection_overlay_visible = !self.connection_overlay_visible;
            }
            Action::ToggleRenderDebug => {
                self.render_debug_visible = !self.render_debug_visible;
            }
            Action::ResetRenderer => return self.on_reset_renderer(),
            Action::ToggleSnapshotMode => return self.on_toggle_snapshot_mode(),
            Action::ToggleInputLock => {
                self.mgr.toggle_input_lock();
            }
            Action::TogglePause => {
                self.toggle_manual_pause();
            }
            Action::ToggleFocusedPaneNoAutoPause => {
                self.toggle_focused_pane_no_auto_pause();
            }
            Action::ToggleActiveSessionNoAutoPause => {
                self.toggle_active_session_no_auto_pause();
            }
            Action::CopySelection => return self.on_copy_selection(),
            Action::Paste => {
                return KeyResult::RequestPaste;
            }
            Action::ExitToNormal => {
                self.mode = Mode::Normal;
            }
            Action::DirPickerChar(ch) => return self.on_dir_picker_char(ch),
            Action::DirPickerBackspace => return self.on_dir_picker_backspace(),
            Action::DirPickerUp => {
                self.dir_picker_selected = self.dir_picker_selected.saturating_sub(1);
            }
            Action::DirPickerDown => return self.on_dir_picker_down(),
            Action::DirPickerSubmit => {
                self.submit_dir_browser_row();
            }
            Action::DirPickerCancel => {}
            Action::LaunchSearchChar(ch) => return self.on_launch_search_char(ch),
            Action::LaunchSearchBackspace => return self.on_launch_search_backspace(),
            Action::LaunchUp => {
                self.launch_selected = self.launch_selected.saturating_sub(1);
            }
            Action::LaunchDown => return self.on_launch_down(),
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
            Action::CommandChar(ch) => return self.on_command_char(ch),
            Action::CommandBackspace => return self.on_command_backspace(),
            Action::CommandLeft => return self.on_command_left(),
            Action::CommandRight => return self.on_command_right(),
            Action::CommandHome => return self.on_command_home(),
            Action::CommandEnd => return self.on_command_end(),
            Action::CommandHintUp => {
                self.command_hint_up();
            }
            Action::CommandHintDown => {
                self.command_hint_down();
            }
            Action::CommandClearLine => return self.on_command_clear_line(),
            Action::CommandDeleteWordBack => return self.on_command_delete_word_back(),
            Action::CommandComplete => {
                self.command_apply_completion();
            }
            Action::CommandSubmit => return self.on_command_submit(),
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
        let Some(focused) = self.mgr.active_pane_id().and_then(pane_index) else {
            return;
        };
        let rects = crate::layout::resolve_layout(
            &layout,
            self.term_size.cols,
            self.term_size.rows,
            &crate::layout::LayoutConfig::default(),
        );
        if let Some(target) = crate::layout::focus_neighbor(&rects, focused, dir)
            && let Some(word) = self.mgr.active_session().map(ToString::to_string)
        {
            self.mgr.focus_pane(format_pane_id(&word, target));
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
        if let Some(word) = self.mgr.active_session().map(ToString::to_string) {
            self.mgr.focus_pane(format_pane_id(&word, pane_index));
        }
    }

    /// Cycle the focused pane within the active tab's leaf order by `delta`
    /// (wraps at both ends). No-op when the active tab has no leaves.
    fn cycle_pane_in_tab(&mut self, delta: i32) {
        let Some(leaves) = self.mgr.active_layout().map(|l| l.leaves().clone()) else {
            return;
        };
        if leaves.is_empty() {
            return;
        }
        let focused = self.mgr.active_pane_id().and_then(pane_index);
        let current = focused
            .and_then(|idx| leaves.iter().position(|&p| p == idx))
            .unwrap_or(0);
        let len = leaves.len() as i32;
        let next = (current as i32 + delta).rem_euclid(len) as usize;
        if let Some(word) = self.mgr.active_session().map(ToString::to_string) {
            self.mgr.focus_pane(format_pane_id(&word, leaves[next]));
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
        let Some(focused) = self.mgr.active_pane_id().and_then(pane_index) else {
            return;
        };
        if let Some((path, ratios)) =
            crate::layout::resize_split(&layout, focused, dir, crate::layout::RESIZE_STEP_PERMILLE)
        {
            self.mgr.set_layout_ratios(path, ratios);
        }
    }

    fn command_hint_up(&mut self) {
        let Mode::Command(state) = &mut self.mode else {
            return;
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
        // Refresh the restorable closed-session list so the "Restore" section is
        // populated by the time the overlay renders (issue #64).
        self.mgr.request_closed_sessions();
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

    /// Open the connected-clients view (issue #146) and request the active
    /// session's client list so the view has data before the periodic poll.
    pub fn open_connected_clients(&mut self) {
        self.mode = Mode::ConnectedClients;
        if let Some(word) = self.mgr.active_session.clone() {
            self.mgr.request_client_list(word);
        }
    }

    /// Toggle the connected-clients view: open it (with an immediate refresh) or
    /// return to the terminal when already open.
    pub fn toggle_connected_clients(&mut self) {
        if matches!(self.mode, Mode::ConnectedClients) {
            self.mode = Mode::Normal;
        } else {
            self.open_connected_clients();
        }
    }

    /// Kick a client from the session whose list is currently shown (issue #146).
    /// Uses [`kmux_client::session_manager::SessionManager::client_list_word`] so the kick targets the listed
    /// session even if the active session changed since the list was fetched.
    pub fn kick_listed_client(&mut self, client_id: kmux_protocol::messages::ClientId) {
        if let Some(word) = self.mgr.client_list_word.clone() {
            self.mgr.kick_client(word, client_id);
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
            LaunchRow::ClosedSession { word_id, .. } => {
                // Respawn the closed session; the daemon's SessionCreated reply
                // selects it (issue #64).
                self.mgr.restore_session(&word_id);
                self.mode = Mode::Normal;
            }
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

    /// Ask the frontend to confirm before closing the active session. The
    /// actual `SessionClose` is sent only by [`Action::ConfirmCloseSession`].
    pub fn confirm_close_active_session(&mut self) {
        let Some(word_id) = self.mgr.active_session().map(str::to_string) else {
            return;
        };
        self.confirm_close_session(&word_id);
    }

    /// Ask the frontend to confirm before closing a specific session.
    pub fn confirm_close_session(&mut self, word_id: &str) {
        let Some(entry) = self
            .mgr
            .session_list()
            .iter()
            .find(|e| e.meta.word_id == word_id)
        else {
            return;
        };
        let name = if entry.meta.name.is_empty() {
            word_id.to_string()
        } else {
            entry.meta.name.clone()
        };
        self.mode = Mode::ConfirmCloseSession {
            word_id: word_id.to_string(),
            name,
        };
        self.request_render();
    }

    /// Cancel the most recently scheduled pane soft-close. Nothing was sent to
    /// the daemon, so the reclaim is instant and the live pane is untouched.
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
        let mut fired = false;

        if !self.pending_closes.is_empty() {
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
            fired |= !due.is_empty();
        }

        fired
    }

    /// Whether `pane_id` is awaiting a deferred close (for a "closing…" hint).
    pub fn is_pane_pending_close(&self, pane_id: &str) -> bool {
        self.pending_closes.iter().any(|p| p.pane_id == pane_id)
    }

    /// Whether any pane is awaiting a deferred close (drives the Undo affordance
    /// shown by the frontends).
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
pub(super) mod testing {
    //! Fixtures shared by every action-handler test module.

    use kmux_client::session_manager::SessionManager;
    use kmux_protocol::messages::ClientCapabilities;

    use super::super::AppCore;

    pub(super) fn fixture_core() -> AppCore {
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
    pub(super) fn core_with_active_pane(status: kmux_protocol::messages::SessionStatus) -> AppCore {
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
                progress_state: Default::default(),
                progress: None,
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
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::cmd::hint::Hint;

    #[test]
    fn close_pane_defers_then_undo_keeps_the_pane() {
        use kmux_protocol::messages::SessionStatus;
        let mut core = core_with_active_pane(SessionStatus::Running);
        let nonce = core.soft_close_nonce;

        // Close → deferred, not killed immediately.
        core.dispatch_action(Action::ClosePane);
        assert!(core.is_pane_pending_close("eagle/0"));
        assert_eq!(core.soft_close_nonce, nonce + 1);

        // A second close keeps the existing deadline (no duplicate).
        core.dispatch_action(Action::ClosePane);
        assert_eq!(core.pending_closes.len(), 1);

        // Undo → cancelled; the shell was never touched.
        core.dispatch_action(Action::UndoClose);
        assert!(!core.has_pending_close());
    }

    #[test]
    fn close_session_requires_confirmation() {
        use kmux_protocol::messages::SessionStatus;
        let mut core = core_with_active_pane(SessionStatus::Running);
        core.mgr.active_session = Some("eagle".into());

        // Close → confirmation mode, not killed immediately.
        core.dispatch_action(Action::CloseSession);
        assert!(matches!(
            core.mode,
            Mode::ConfirmCloseSession { ref word_id, .. } if word_id == "eagle"
        ));
        assert!(!core.has_pending_close());

        // Cancel → normal mode; the live session was never touched.
        core.dispatch_action(Action::ExitToNormal);
        assert_eq!(core.mode, Mode::Normal);
    }

    #[test]
    fn confirm_close_session_targets_named_session() {
        use kmux_protocol::messages::SessionStatus;
        let mut core = core_with_active_pane(SessionStatus::Running);
        core.confirm_close_session("eagle");
        assert!(matches!(
            core.mode,
            Mode::ConfirmCloseSession { ref word_id, ref name } if word_id == "eagle" && name == "eagle"
        ));
    }

    #[test]
    fn toggle_render_debug_flips_flag() {
        let mut core = fixture_core();
        assert!(!core.render_debug_visible);
        core.dispatch_action(Action::ToggleRenderDebug);
        assert!(core.render_debug_visible);
        core.dispatch_action(Action::ToggleRenderDebug);
        assert!(!core.render_debug_visible);
    }

    #[test]
    fn reset_renderer_signals_keyresult_and_forces_clear() {
        let mut core = fixture_core();
        let result = core.dispatch_action(Action::ResetRenderer);
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

    #[test]
    fn rename_tab_mode_edits_buffer_and_submits_to_normal() {
        let mut core = fixture_core();
        core.mode = Mode::RenameTab {
            word_id: "w".into(),
            tab_index: 3,
            buffer: String::new(),
        };
        core.dispatch_action(Action::RenameChar('h'));
        core.dispatch_action(Action::RenameChar('i'));
        match &core.mode {
            Mode::RenameTab {
                buffer, tab_index, ..
            } => {
                assert_eq!(buffer, "hi");
                assert_eq!(*tab_index, 3);
            }
            other => panic!("expected RenameTab, got {other:?}"),
        }
        core.dispatch_action(Action::RenameBackspace);
        if let Mode::RenameTab { buffer, .. } = &core.mode {
            assert_eq!(buffer, "h");
        }
        // Submitting a non-empty name leaves the rename mode for Normal.
        core.dispatch_action(Action::RenameSubmit);
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
