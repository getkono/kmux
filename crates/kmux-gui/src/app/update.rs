use std::time::{Duration, Instant};

use iced::Task;
use iced::widget::text_input;
use tracing::{info, warn};

use kmux_client::grid::{GridPos, Selection, SelectionMode};
use kmux_client::input::encode_mouse_scroll;
use kmux_client::session_manager::SessionEvent;

use crate::session_bar;
use crate::shortcut::{self, LEADER_TIMEOUT, LeaderState, ShortcutAction};

use super::{Message, Screen, command_palette_input_id, kmuxApp};

impl kmuxApp {
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            //  Connect form
            Message::HostChanged(v) => {
                self.host = v;
                Task::none()
            }
            Message::PortChanged(v) => {
                self.port = v;
                Task::none()
            }
            Message::TokenChanged(v) => {
                self.token = v;
                Task::none()
            }

            Message::ConnectPressed => {
                let port = self.port.parse().unwrap_or(8443);
                self.connect_params = Some(super::ConnectParams {
                    host: self.host.clone(),
                    port,
                    token: self.token.clone(),
                    accept_invalid_certs: self.accept_invalid_certs,
                });
                self.mgr
                    .set_connection_params(self.host.clone(), port, self.token.clone());
                self.mgr.set_status_msg("Connecting...".to_string());
                Task::none()
            }

            Message::DisconnectPressed => {
                self.connect_params = None;
                self.screen = Screen::Connect;
                self.leader_state = LeaderState::Idle;
                self.mgr.disconnect();
                Task::none()
            }

            //  Async events
            Message::Connected(sender) => {
                self.screen = Screen::Terminal;
                self.mgr.set_ws_sender(sender);
                self.mgr.request_session_list();
                if let Some(p) = &self.connect_params {
                    self.mgr
                        .set_status_msg(format!("Connected to {}:{}", p.host, p.port));
                }
                info!("Connected to kmuxd");
                Task::none()
            }

            Message::ConnectionFailed(reason) => {
                self.connect_params = None;
                self.mgr
                    .set_status_msg(format!("Connection failed: {reason}"));
                Task::none()
            }

            Message::ServerMsgBatch(msgs) => {
                self.mgr.metrics.record_batch(msgs.len());
                let tasks: Vec<Task<Message>> = msgs
                    .into_iter()
                    .map(|msg| {
                        let events = self.mgr.handle_server_message(msg);
                        self.handle_session_events(events)
                    })
                    .collect();
                Task::batch(tasks)
            }

            //  Session management
            Message::SelectSession(name) => {
                self.mgr.select_session(name);
                Task::none()
            }

            Message::CreateSessionPressed => {
                if !self.mgr.is_connected() {
                    warn!("CreateSessionPressed: no active connection, ignoring");
                    self.mgr
                        .set_status_msg("Not connected -- cannot create session".to_string());
                    return Task::none();
                }
                self.mgr.set_status_msg("Creating session...".to_string());
                self.mgr
                    .create_session(None, None, self.current_term_size());
                Task::none()
            }

            Message::CloseSession(name) => {
                // Close button on tab triggers confirm flow
                self.leader_state = LeaderState::ConfirmClose { session: name };
                Task::none()
            }

            //  Keyboard input -- leader key interception
            Message::RawKeyEvent {
                key,
                modifiers,
                text,
            } => self.handle_key_event(key, modifiers, text),

            Message::Disconnected => {
                self.mgr.mark_connection_lost();
                self.last_connect_params = self.connect_params.take();
                self.mgr
                    .set_status_msg("Connection lost \u{2014} reconnecting in 3s...".to_string());
                self.disconnect_toast = Some(Instant::now());
                warn!("Connection lost, scheduling reconnect");
                Task::batch([
                    Task::perform(
                        async { tokio::time::sleep(Duration::from_secs(3)).await },
                        |_| Message::Reconnect,
                    ),
                    Task::perform(
                        async { tokio::time::sleep(Duration::from_secs(5)).await },
                        |_| Message::DismissDisconnectToast,
                    ),
                ])
            }

            Message::Reconnect => {
                if let Some(params) = self.last_connect_params.take() {
                    self.mgr.set_connection_params(
                        params.host.clone(),
                        params.port,
                        params.token.clone(),
                    );
                    self.connect_params = Some(params);
                    self.mgr.set_status_msg("Reconnecting...".to_string());
                }
                Task::none()
            }

            Message::DismissDisconnectToast => {
                self.disconnect_toast = None;
                Task::none()
            }

            Message::ToggleHud => {
                self.hud_visible = !self.hud_visible;
                Task::none()
            }

            Message::ToggleSnapshotMode => {
                self.force_snapshot_mode = !self.force_snapshot_mode;
                self.mgr.set_snapshot_mode(self.force_snapshot_mode);
                Task::none()
            }

            // Clipboard paste
            Message::ClipboardPaste(contents) => {
                if let Some(text) = contents
                    && !text.is_empty()
                {
                    if self.mgr.active_session().is_some() {
                        self.mgr.send_paste(text);
                    } else {
                        self.mgr.set_status_msg(
                            "No active session -- press Ctrl+B then c to create one".to_string(),
                        );
                    }
                }
                Task::none()
            }

            //  Scroll terminal
            Message::ScrollTerminal(delta) => {
                if let Some(grid) = self.mgr.active_grid_mut() {
                    if delta > 0 {
                        grid.scroll_up(delta as usize);
                    } else if delta < 0 {
                        grid.scroll_down((-delta) as usize);
                    }
                }
                Task::none()
            }

            // Forward mouse scroll to PTY
            Message::ForwardMouseScroll { col, row, lines } => {
                if let Some(name) = self.mgr.active_session().map(|s| s.to_string())
                    && let Some(grid) = self.mgr.buffer(&name)
                {
                    let sgr = grid.modes().sgr_mouse();
                    let bytes = encode_mouse_scroll(col, row, lines, sgr);
                    if !bytes.is_empty() {
                        self.mgr.send_input(bytes);
                    }
                }
                Task::none()
            }

            //  Terminal resize
            Message::TerminalResized { rows, cols } => {
                self.last_term_rows = rows;
                self.last_term_cols = cols;
                if let Some(pane_id) = self.mgr.active_pane_id().map(|s| s.to_string()) {
                    self.mgr.send_resize(&pane_id, rows, cols);
                }
                Task::none()
            }

            // Leader timeout check
            Message::LeaderTimeout => {
                if let LeaderState::AwaitingAction { entered_at } = &self.leader_state
                    && entered_at.elapsed() >= LEADER_TIMEOUT
                {
                    self.leader_state = LeaderState::Idle;
                }
                Task::none()
            }

            // Rename
            Message::RenameInput(new_val) => {
                if let LeaderState::RenameEditing { buffer, .. } = &mut self.leader_state {
                    *buffer = new_val;
                }
                Task::none()
            }

            Message::RenameSubmit => {
                if let LeaderState::RenameEditing { buffer, session } =
                    std::mem::replace(&mut self.leader_state, LeaderState::Idle)
                {
                    let new_name = buffer.trim().to_string();
                    self.mgr.rename_session(&session, &new_name);
                }
                Task::none()
            }

            // Command palette
            Message::CommandPaletteInput(query) => {
                if let LeaderState::CommandPalette { query: q, selected } = &mut self.leader_state {
                    *q = query;
                    *selected = 0;
                }
                Task::none()
            }

            Message::CommandPaletteNavigate(delta) => {
                if let LeaderState::CommandPalette { query, selected } = &mut self.leader_state {
                    let filtered = shortcut::filter_commands(query);
                    if !filtered.is_empty() {
                        let len = filtered.len() as i32;
                        *selected = ((*selected as i32 + delta).rem_euclid(len)) as usize;
                    }
                }
                Task::none()
            }

            Message::CommandPaletteSelect => {
                if let LeaderState::CommandPalette { query, selected } =
                    std::mem::replace(&mut self.leader_state, LeaderState::Idle)
                {
                    let filtered = shortcut::filter_commands(&query);
                    if let Some(entry) = filtered.into_iter().nth(selected) {
                        return self.dispatch_action(entry.action);
                    }
                }
                Task::none()
            }

            Message::CommandPaletteClose => {
                self.leader_state = LeaderState::Idle;
                Task::none()
            }

            // ── Text selection ──
            Message::SelectionStart { pos, mode } => {
                if let Some(grid) = self.mgr.active_grid_mut() {
                    let sel = match mode {
                        SelectionMode::Word => {
                            let (start, end) = grid.find_word_boundaries(pos);
                            Selection {
                                anchor: start,
                                end,
                                mode,
                            }
                        }
                        SelectionMode::Line => Selection {
                            anchor: GridPos {
                                row: pos.row,
                                col: 0,
                            },
                            end: GridPos {
                                row: pos.row,
                                col: grid.cols.saturating_sub(1),
                            },
                            mode,
                        },
                        SelectionMode::Normal => Selection {
                            anchor: pos,
                            end: pos,
                            mode,
                        },
                    };
                    grid.set_selection(Some(sel));
                }
                Task::none()
            }

            Message::SelectionUpdate { pos } => {
                if let Some(grid) = self.mgr.active_grid_mut()
                    && let Some(sel) = grid.selection().copied()
                {
                    let new_end = match sel.mode {
                        SelectionMode::Word => {
                            let (_, word_end) = grid.find_word_boundaries(pos);
                            // Extend to the farther word boundary from anchor
                            if (pos.row, pos.col) >= (sel.anchor.row, sel.anchor.col) {
                                word_end
                            } else {
                                let (word_start, _) = grid.find_word_boundaries(pos);
                                word_start
                            }
                        }
                        SelectionMode::Line => {
                            if pos.row >= sel.anchor.row {
                                GridPos {
                                    row: pos.row,
                                    col: grid.cols.saturating_sub(1),
                                }
                            } else {
                                GridPos {
                                    row: pos.row,
                                    col: 0,
                                }
                            }
                        }
                        SelectionMode::Normal => pos,
                    };
                    let mut updated = sel;
                    updated.end = new_end;
                    grid.set_selection(Some(updated));
                }
                Task::none()
            }

            Message::SelectionEnd => {
                // Selection stays. Nothing to do.
                Task::none()
            }

            Message::SelectionAutoScroll(direction) => {
                if let Some(grid) = self.mgr.active_grid_mut() {
                    if direction > 0 {
                        grid.scroll_up(direction as usize);
                    } else if direction < 0 {
                        grid.scroll_down((-direction) as usize);
                    }
                    // Update selection end to viewport edge
                    if let Some(sel) = grid.selection().copied() {
                        let sb_len = grid.scrollback_len();
                        let new_end = if direction > 0 {
                            // Scrolled up, extend to top of viewport
                            GridPos {
                                row: sb_len.saturating_sub(grid.scroll_offset()),
                                col: 0,
                            }
                        } else {
                            // Scrolled down, extend to bottom of viewport
                            GridPos {
                                row: sb_len.saturating_sub(grid.scroll_offset()) + grid.rows - 1,
                                col: grid.cols.saturating_sub(1),
                            }
                        };
                        let mut updated = sel;
                        updated.end = new_end;
                        grid.set_selection(Some(updated));
                    }
                }
                Task::none()
            }
        }
    }

    /// React to `SessionEvent`s returned from `SessionManager::handle_server_message`.
    pub(super) fn handle_session_events(&mut self, events: Vec<SessionEvent>) -> Task<Message> {
        for event in events {
            match event {
                SessionEvent::AuthFailed { .. } => {
                    self.connect_params = None;
                    self.screen = Screen::Connect;
                }
                SessionEvent::AuthOk => {
                    info!("Auth succeeded");
                    self.write_connection_log();
                }
                _ => {}
            }
        }
        Task::none()
    }

    /// Dispatch a ShortcutAction from the leader key system.
    pub(super) fn dispatch_action(&mut self, action: ShortcutAction) -> Task<Message> {
        match action {
            ShortcutAction::CreateSession => {
                self.leader_state = LeaderState::Idle;
                self.update(Message::CreateSessionPressed)
            }
            ShortcutAction::CloseSession => {
                if let Some(session) = self.mgr.active_session().map(|s| s.to_string()) {
                    self.leader_state = LeaderState::ConfirmClose { session };
                } else {
                    self.leader_state = LeaderState::Idle;
                }
                Task::none()
            }
            ShortcutAction::NextSession => {
                self.leader_state = LeaderState::Idle;
                self.mgr.cycle_session(1);
                Task::none()
            }
            ShortcutAction::PrevSession => {
                self.leader_state = LeaderState::Idle;
                self.mgr.cycle_session(-1);
                Task::none()
            }
            ShortcutAction::JumpToSession(idx) => {
                self.leader_state = LeaderState::Idle;
                if idx < self.mgr.session_list().len() {
                    let name = self.mgr.session_list()[idx].meta.name.clone();
                    self.update(Message::SelectSession(name))
                } else {
                    Task::none()
                }
            }
            ShortcutAction::RenameSession => {
                if let Some(session) = self.mgr.active_session().map(|s| s.to_string()) {
                    self.leader_state = LeaderState::RenameEditing {
                        buffer: session.clone(),
                        session,
                    };
                    text_input::focus(session_bar::rename_input_id())
                } else {
                    self.leader_state = LeaderState::Idle;
                    Task::none()
                }
            }
            ShortcutAction::Disconnect => {
                self.leader_state = LeaderState::Idle;
                self.update(Message::DisconnectPressed)
            }
            ShortcutAction::ShowSignalMenu => {
                if let Some(session) = self.mgr.active_session().map(|s| s.to_string()) {
                    self.leader_state = LeaderState::SignalMenu { session };
                } else {
                    self.leader_state = LeaderState::Idle;
                }
                Task::none()
            }
            ShortcutAction::ToggleInputLock => {
                self.leader_state = LeaderState::Idle;
                self.mgr.toggle_input_lock();
                Task::none()
            }
            ShortcutAction::ShowHelp => {
                self.leader_state = LeaderState::HelpVisible;
                Task::none()
            }
            ShortcutAction::ToggleHud => {
                self.leader_state = LeaderState::Idle;
                self.hud_visible = !self.hud_visible;
                Task::none()
            }
            ShortcutAction::ToggleSnapshotMode => {
                self.leader_state = LeaderState::Idle;
                self.force_snapshot_mode = !self.force_snapshot_mode;
                self.mgr.set_snapshot_mode(self.force_snapshot_mode);
                info!(
                    "Snapshot mode {}",
                    if self.force_snapshot_mode {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                Task::none()
            }
            ShortcutAction::OpenCommandPalette => {
                self.leader_state = LeaderState::CommandPalette {
                    query: String::new(),
                    selected: 0,
                };
                text_input::focus(command_palette_input_id())
            }
            ShortcutAction::SendLiteralLeader => {
                self.leader_state = LeaderState::Idle;
                // Send Ctrl+B (0x02) to PTY
                self.mgr.send_input(vec![0x02]);
                Task::none()
            }
            ShortcutAction::ScrollPageUp => {
                self.leader_state = LeaderState::Idle;
                if let Some(grid) = self.mgr.active_grid_mut() {
                    grid.scroll_up(grid.rows);
                }
                Task::none()
            }
            ShortcutAction::ScrollPageDown => {
                self.leader_state = LeaderState::Idle;
                if let Some(grid) = self.mgr.active_grid_mut() {
                    grid.scroll_down(grid.rows);
                }
                Task::none()
            }
        }
    }
}
