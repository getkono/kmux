use crossterm::event::KeyEvent;
use kmux_client::input::key_to_bytes;

use crate::key_convert;
use crate::mode::{self, Action, ConnectField, Mode};
use crate::recent_servers::ServerKind;

use super::{App, KeyResult, SwitchTarget};

impl App {
    /// Handle a key event. Returns the appropriate `KeyResult` for the event loop.
    pub(super) async fn handle_key(&mut self, key_event: KeyEvent) -> KeyResult {
        let (key, mods) = key_convert::convert(&key_event);
        let (new_mode, action) = mode::resolve(&self.mode, &key, mods);

        if let Some(m) = new_mode {
            self.mode = m;
        }

        match action {
            Action::ForwardKey => {
                // Snap to bottom on keypress
                if let Some(grid) = self.mgr.active_grid_mut() {
                    grid.scroll_to_bottom();
                }

                let app_cursor = self
                    .mgr
                    .active_grid()
                    .map(|b| b.app_cursor())
                    .unwrap_or(false);
                let text = key_convert::text_from_event(&key_event);
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
                        let _ = cli_clipboard::set_contents(text);
                    });
                }
            }
            Action::Paste => {
                if let Some(tx) = self.paste_tx.clone() {
                    tokio::task::spawn_blocking(move || {
                        if let Ok(text) = cli_clipboard::get_contents() {
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
            Action::None => {}
        }

        KeyResult::Continue
    }
}
