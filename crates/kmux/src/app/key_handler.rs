use crossterm::event::KeyEvent;
use kmux_client::input::key_to_bytes;

use crate::cmd;
use crate::key_convert;
use crate::mode::{self, Action, ConnectField, Mode};
use crate::recent_servers::ServerKind;

use super::{App, COMMAND_HISTORY_CAP, KeyResult, SwitchTarget};

impl App {
    /// Handle a key event. Returns the appropriate `KeyResult` for the event loop.
    pub(super) async fn handle_key(&mut self, key_event: KeyEvent) -> KeyResult {
        let (key, mods) = key_convert::convert(&key_event);
        let (new_mode, action) = mode::resolve(&self.mode, &key, mods);

        if let Some(m) = new_mode {
            self.mode = m;
        }

        self.dispatch_action(action, Some(&key_event)).await
    }

    /// Apply an `Action` to the app. Used both by the key path and by the
    /// command palette so a single source of truth governs behavior.
    ///
    /// `src_event` is only used by `Action::ForwardKey` for raw byte encoding;
    /// command-issued actions pass `None`.
    pub(crate) async fn dispatch_action(
        &mut self,
        action: Action,
        src_event: Option<&KeyEvent>,
    ) -> KeyResult {
        match action {
            Action::ForwardKey => {
                // ForwardKey requires the original event to encode bytes; it is
                // only emitted from the key path, so `src_event` must be Some.
                let Some(key_event) = src_event else {
                    return KeyResult::Continue;
                };
                let (key, mods) = key_convert::convert(key_event);

                // Snap to bottom on keypress
                if let Some(grid) = self.mgr.active_grid_mut() {
                    grid.scroll_to_bottom();
                }

                let app_cursor = self
                    .mgr
                    .active_grid()
                    .map(|b| b.app_cursor())
                    .unwrap_or(false);
                let text = key_convert::text_from_event(key_event);
                let bytes = key_to_bytes(&key, mods, text.as_deref(), app_cursor);
                if let Some(bytes) = bytes {
                    self.mgr.send_input(bytes);
                }
            }
            Action::CreateSession => {
                self.mgr
                    .create_session(None, None, Self::current_term_size());
            }
            Action::CreatePane => {
                self.mgr.create_pane(Self::current_term_size());
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
            Action::NextPane => self.mgr.cycle_pane(1),
            Action::PrevPane => self.mgr.cycle_pane(-1),
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
                if let Mode::RenameSession { buffer, .. } = &mut self.mode {
                    buffer.push(ch);
                }
            }
            Action::RenameBackspace => {
                if let Mode::RenameSession { buffer, .. } = &mut self.mode {
                    buffer.pop();
                }
            }
            Action::RenameSubmit => {
                if let Mode::RenameSession { buffer, word_id } =
                    std::mem::replace(&mut self.mode, Mode::Normal)
                {
                    let new_name = buffer.trim().to_string();
                    self.mgr.rename_session(&word_id, &new_name);
                }
            }
            Action::CloseSessionPicker => {
                self.mode = Mode::Normal;
            }
            Action::SelectPickerEntry => {
                let search = self.session_picker_search.to_lowercase();
                let matches: Vec<_> = self
                    .mgr
                    .session_list()
                    .iter()
                    .filter(|e| {
                        search.is_empty()
                            || e.meta.name.to_lowercase().contains(&search)
                            || e.meta.word_id.to_lowercase().contains(&search)
                    })
                    .map(|e| e.meta.word_id.clone())
                    .collect();
                if let Some(word_id) = matches.get(self.session_picker_selected) {
                    self.mgr.select_session(word_id.clone());
                }
                self.mode = Mode::Normal;
            }
            Action::PickerUp => {
                if self.session_picker_selected > 0 {
                    self.session_picker_selected -= 1;
                }
            }
            Action::PickerDown => {
                let count = self
                    .mgr
                    .session_list()
                    .iter()
                    .filter(|e| {
                        let s = self.session_picker_search.to_lowercase();
                        s.is_empty()
                            || e.meta.name.to_lowercase().contains(&s)
                            || e.meta.word_id.to_lowercase().contains(&s)
                    })
                    .count();
                if count > 0 && self.session_picker_selected + 1 < count {
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
                let servers = self.filtered_servers();
                if let Some(server) = servers.get(self.server_picker_selected).cloned() {
                    // If already connected to this server, just close the picker.
                    if server.server_string == self.server_string {
                        return KeyResult::Continue;
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
                        ServerKind::Direct { host, port } => SwitchTarget::Direct { host, port },
                    };
                    return KeyResult::SwitchServer(target);
                }
            }
            Action::Disconnect => {
                self.mgr.disconnect();
                self.mode = Mode::Connect {
                    field: ConnectField::Host,
                };
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
            Action::ToggleSnapshotMode => {
                self.force_snapshot_mode = !self.force_snapshot_mode;
                self.mgr.set_snapshot_mode(self.force_snapshot_mode);
            }
            Action::ToggleInputLock => {
                self.mgr.toggle_input_lock();
            }
            Action::CopySelection => {
                if let Some(text) = self.mgr.active_grid().and_then(|g| g.selected_text()) {
                    tokio::task::spawn_blocking(move || {
                        if let Ok(mut cb) = arboard::Clipboard::new() {
                            let _ = cb.set_text(text);
                        }
                    });
                }
            }
            Action::Paste => {
                if let Some(tx) = self.paste_tx.clone() {
                    tokio::task::spawn_blocking(move || {
                        if let Ok(mut cb) = arboard::Clipboard::new()
                            && let Ok(text) = cb.get_text()
                        {
                            let _ = tx.send(text);
                        }
                    });
                }
            }
            Action::ConnectSubmit => {
                return KeyResult::Reconnect;
            }
            Action::ConnectNextField => {
                self.mode = match &self.mode {
                    Mode::Connect {
                        field: ConnectField::Host,
                    } => Mode::Connect {
                        field: ConnectField::Port,
                    },
                    Mode::Connect {
                        field: ConnectField::Port,
                    } => Mode::Connect {
                        field: ConnectField::Token,
                    },
                    Mode::Connect {
                        field: ConnectField::Token,
                    } => Mode::Connect {
                        field: ConnectField::Host,
                    },
                    other => other.clone(),
                };
            }
            Action::ConnectPrevField => {
                self.mode = match &self.mode {
                    Mode::Connect {
                        field: ConnectField::Host,
                    } => Mode::Connect {
                        field: ConnectField::Token,
                    },
                    Mode::Connect {
                        field: ConnectField::Port,
                    } => Mode::Connect {
                        field: ConnectField::Host,
                    },
                    Mode::Connect {
                        field: ConnectField::Token,
                    } => Mode::Connect {
                        field: ConnectField::Port,
                    },
                    other => other.clone(),
                };
            }
            Action::ConnectChar(ch) => {
                if let Mode::Connect { field } = &self.mode {
                    match field {
                        ConnectField::Host => self.connect_host.push(ch),
                        ConnectField::Port => self.connect_port.push(ch),
                        ConnectField::Token => self.connect_token.push(ch),
                    }
                }
            }
            Action::ConnectBackspace => {
                if let Mode::Connect { field } = &self.mode {
                    match field {
                        ConnectField::Host => {
                            self.connect_host.pop();
                        }
                        ConnectField::Port => {
                            self.connect_port.pop();
                        }
                        ConnectField::Token => {
                            self.connect_token.pop();
                        }
                    }
                }
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
                            self.mgr
                                .create_session(None, Some(&cwd), Self::current_term_size());
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
}

/// Apply a hint's replacement to a buffer, returning the resulting buffer.
/// Shared by Tab (live edit) and Enter (submit-with-fallback when the typed
/// buffer doesn't parse to a known command).
fn apply_hint_to_buffer(buffer: &str, hint: &cmd::hint::Hint) -> String {
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
}
