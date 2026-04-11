use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iced::futures::SinkExt as _;
use iced::widget::{Space, button, column, container, row, text, text_input};
use iced::{Element, Event, Font, Length, Subscription, Task, Theme};
use kmux_protocol::messages::{ClientCapabilities, ClientMessage, PROTOCOL_VERSION, ServerMessage};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use kmux_client::connect;
use kmux_client::grid::{GridPos, Selection, SelectionMode};
use kmux_client::input::encode_mouse_scroll;
use kmux_client::session_manager::{SessionEvent, SessionManager};
use kmux_client::token::read_local_token;

use crate::shortcut::{self, LEADER_TIMEOUT, LeaderState, ShortcutAction};
use crate::{session_bar, status_bar, terminal_view, theme};

/// Connection parameters used as a subscription ID (triggers reconnect on change).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ConnectParams {
    host: String,
    port: u16,
    token: String,
    accept_invalid_certs: bool,
}

/// Which screen is currently shown.
#[derive(Default)]
enum Screen {
    #[default]
    Connect,
    Terminal,
}

/// Messages flowing through the iced update loop.
#[derive(Debug, Clone)]
pub enum Message {
    // Connect form
    HostChanged(String),
    PortChanged(String),
    TokenChanged(String),
    ConnectPressed,
    DisconnectPressed,

    // Async connection events (emitted by subscription)
    Connected(mpsc::UnboundedSender<ClientMessage>),
    ConnectionFailed(String),
    ServerMsgBatch(Vec<ServerMessage>),

    // Session management
    SelectSession(String),
    CreateSessionPressed,
    CloseSession(String),

    // Raw keyboard event from subscription
    RawKeyEvent {
        key: iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
        text: Option<String>,
    },

    // Connection lost / reconnect
    Disconnected,
    Reconnect,
    DismissDisconnectToast,

    // Toggle the HUD overlay (F12) — dispatched internally via leader key
    #[allow(dead_code)]
    ToggleHud,

    // Toggle full-snapshot mode
    #[allow(dead_code)]
    ToggleSnapshotMode,

    // Terminal canvas resize detected
    TerminalResized {
        rows: u16,
        cols: u16,
    },

    // Leader key system
    LeaderTimeout,

    // Rename
    RenameInput(String),
    RenameSubmit,

    // Command palette
    CommandPaletteInput(String),
    CommandPaletteSelect,
    CommandPaletteNavigate(i32),
    #[allow(dead_code)]
    CommandPaletteClose,

    /// Scroll the terminal by the given number of lines.
    /// Positive = scroll up (into history), negative = scroll down.
    ScrollTerminal(i32),

    /// Forward mouse scroll to the PTY as escape-encoded bytes.
    /// `col` and `row` are 1-based terminal coordinates.
    ForwardMouseScroll {
        col: u16,
        row: u16,
        lines: i32,
    },

    /// Clipboard contents received for paste.
    ClipboardPaste(Option<String>),

    // Text selection
    SelectionStart {
        pos: GridPos,
        mode: SelectionMode,
    },
    SelectionUpdate {
        pos: GridPos,
    },
    SelectionEnd,
    SelectionAutoScroll(i32),
}

/// Top-level application state.
#[allow(non_camel_case_types)]
pub struct kmuxApp {
    mgr: SessionManager,
    screen: Screen,
    connect_params: Option<ConnectParams>,

    // Connect form fields (kept for iced text_input widgets)
    host: String,
    port: String,
    token: String,
    accept_invalid_certs: bool,

    // Reconnection state
    last_connect_params: Option<ConnectParams>,
    disconnect_toast: Option<Instant>,

    // Observability
    hud_visible: bool,

    // Full-snapshot mode: server sends complete grid snapshots instead of diffs.
    force_snapshot_mode: bool,

    // Leader key state machine
    leader_state: LeaderState,

    /// Unique ID for this client process, written to the connection log on auth success.
    instance_id: String,
}

/// Static capability profile for the GUI client.
///
/// - `truecolor`: always true — iced renders with full 24-bit RGB.
/// - `kitty_graphics`/`kitty_keyboard`: false — not yet wired through the cell grid.
fn gui_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        truecolor: true,
        kitty_graphics: false,
        kitty_keyboard: false,
        term: None,
        term_program: Some("kmux-gui".into()),
    }
}

impl kmuxApp {
    pub fn new(accept_invalid_certs: bool, instance_id: String) -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: "8443".to_string(),
            token: read_local_token().unwrap_or_default(),
            accept_invalid_certs,
            mgr: SessionManager::new(
                "127.0.0.1".to_string(),
                8443,
                String::new(),
                accept_invalid_certs,
                gui_capabilities(),
            ),
            screen: Screen::Connect,
            connect_params: None,
            last_connect_params: None,
            disconnect_toast: None,
            hud_visible: false,
            force_snapshot_mode: false,
            leader_state: LeaderState::Idle,
            instance_id,
        }
    }

    /// Get the host:port string for display.
    fn host_port_display(&self) -> String {
        self.mgr.host_port_display()
    }

    /// Dispatch a ShortcutAction from the leader key system.
    fn dispatch_action(&mut self, action: ShortcutAction) -> Task<Message> {
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
                self.connect_params = Some(ConnectParams {
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
                self.mgr.create_session();
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
                if let Some(name) = self.mgr.active_session().map(|s| s.to_string()) {
                    self.mgr.send_resize(&name, rows, cols);
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
    fn handle_session_events(&mut self, events: Vec<SessionEvent>) -> Task<Message> {
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

    /// Handle a raw key event with leader key interception.
    fn handle_key_event(
        &mut self,
        key: iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
        text_val: Option<String>,
    ) -> Task<Message> {
        use iced::keyboard::Key;
        use iced::keyboard::key::Named;

        match &self.leader_state {
            LeaderState::Idle => {
                // Shift+PageUp / Shift+PageDown scroll by one page.
                if modifiers.shift() && key == Key::Named(Named::PageUp) {
                    if let Some(grid) = self.mgr.active_grid_mut() {
                        grid.scroll_up(grid.rows);
                    }
                    return Task::none();
                }
                if modifiers.shift() && key == Key::Named(Named::PageDown) {
                    if let Some(grid) = self.mgr.active_grid_mut() {
                        grid.scroll_down(grid.rows);
                    }
                    return Task::none();
                }

                // Ctrl+Shift+H or F12 toggles HUD
                if key == Key::Named(Named::F12)
                    || (matches!(&key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("h"))
                        && modifiers.shift()
                        && modifiers.control())
                {
                    self.hud_visible = !self.hud_visible;
                    return Task::none();
                }

                // Ctrl+B → enter leader mode
                if shortcut::is_leader_key(&key, modifiers) {
                    self.leader_state = LeaderState::AwaitingAction {
                        entered_at: Instant::now(),
                    };
                    return Task::none();
                }

                // Cmd+C (macOS) or Ctrl+Shift+C (Linux/Windows) → copy selection
                if matches!(&key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("c")) {
                    let is_copy = if cfg!(target_os = "macos") {
                        modifiers.logo()
                    } else {
                        modifiers.control() && modifiers.shift()
                    };
                    if is_copy {
                        if let Some(grid) = self.mgr.active_grid()
                            && let Some(text) = grid.selected_text()
                        {
                            return iced::clipboard::write(text);
                        }
                        return Task::none();
                    }
                }

                // Ctrl+Shift+V (Linux/Windows) or Cmd+V (macOS) → clipboard paste
                if matches!(&key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("v")) {
                    let is_paste = if cfg!(target_os = "macos") {
                        modifiers.logo()
                    } else {
                        modifiers.control() && modifiers.shift()
                    };
                    if is_paste {
                        return iced::clipboard::read().map(Message::ClipboardPaste);
                    }
                }

                // Escape clears selection (and still forwards to PTY below)
                if key == Key::Named(Named::Escape)
                    && let Some(grid) = self.mgr.active_grid_mut()
                {
                    grid.clear_selection();
                }

                // Normal key → forward to PTY
                self.forward_key_to_pty(key, modifiers, text_val)
            }

            LeaderState::AwaitingAction { .. } => {
                // Ignore modifier-only key presses (Shift, Ctrl, Alt, Super)
                // so that shifted characters like '?' (Shift+/) work.
                if matches!(
                    key,
                    Key::Named(
                        Named::Shift | Named::Control | Named::Alt | Named::Super | Named::Meta
                    )
                ) {
                    return Task::none();
                }

                if let Some(action) = shortcut::resolve_key(&key, modifiers) {
                    self.dispatch_action(action)
                } else {
                    // Unrecognized key in leader mode → cancel and discard
                    self.leader_state = LeaderState::Idle;
                    Task::none()
                }
            }

            LeaderState::RenameEditing { .. } => {
                // Escape cancels rename
                if key == Key::Named(Named::Escape) {
                    self.leader_state = LeaderState::Idle;
                    return Task::none();
                }
                // Enter and text input handled by iced text_input widget messages
                Task::none()
            }

            LeaderState::ConfirmClose { .. } => {
                let session = if let LeaderState::ConfirmClose { session } = &self.leader_state {
                    session.clone()
                } else {
                    unreachable!()
                };

                match &key {
                    Key::Character(c) if c.as_str() == "y" => {
                        self.leader_state = LeaderState::Idle;
                        self.mgr.close_session(&session);
                        Task::none()
                    }
                    _ => {
                        // Any other key (including 'n') cancels
                        self.leader_state = LeaderState::Idle;
                        Task::none()
                    }
                }
            }

            LeaderState::SignalMenu { .. } => {
                let session = if let LeaderState::SignalMenu { session } = &self.leader_state {
                    session.clone()
                } else {
                    unreachable!()
                };

                if key == Key::Named(Named::Escape) {
                    self.leader_state = LeaderState::Idle;
                    return Task::none();
                }

                if let Some(signal) = shortcut::resolve_signal_key(&key) {
                    self.leader_state = LeaderState::Idle;
                    self.mgr.send_signal(&session, signal);
                } else {
                    self.leader_state = LeaderState::Idle;
                }
                Task::none()
            }

            LeaderState::HelpVisible => {
                // Any key closes help
                self.leader_state = LeaderState::Idle;
                Task::none()
            }

            LeaderState::CommandPalette { .. } => {
                // Escape closes
                if key == Key::Named(Named::Escape) {
                    self.leader_state = LeaderState::Idle;
                    return Task::none();
                }
                // Arrow keys navigate
                if key == Key::Named(Named::ArrowUp) {
                    return self.update(Message::CommandPaletteNavigate(-1));
                }
                if key == Key::Named(Named::ArrowDown) {
                    return self.update(Message::CommandPaletteNavigate(1));
                }
                // Enter selects
                if key == Key::Named(Named::Enter) {
                    return self.update(Message::CommandPaletteSelect);
                }
                // Other keys are handled by the text_input widget
                Task::none()
            }
        }
    }

    /// Forward a key to the PTY as bytes.
    fn forward_key_to_pty(
        &mut self,
        key: iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
        text_val: Option<String>,
    ) -> Task<Message> {
        // Snap to bottom on any keypress while scrolled.
        if let Some(grid) = self.mgr.active_grid_mut() {
            grid.scroll_to_bottom();
        }

        let app_cursor = self
            .mgr
            .active_grid()
            .map(|b| b.app_cursor())
            .unwrap_or(false);
        let bytes = key_to_bytes(key, modifiers, text_val, app_cursor);
        if let Some(bytes) = bytes {
            if self.mgr.active_session().is_some() {
                if !self.mgr.send_input(bytes) {
                    // input_locked; status_msg already updated by mgr
                }
            } else {
                self.mgr.set_status_msg(
                    "No active session -- press Ctrl+B then c to create one".to_string(),
                );
            }
        }
        Task::none()
    }

    pub fn view(&self) -> Element<'_, Message> {
        match self.screen {
            Screen::Connect => self.view_connect(),
            Screen::Terminal => self.view_terminal(),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let Some(params) = self.connect_params.clone() else {
            return Subscription::none();
        };

        let kbd_sub = match self.screen {
            Screen::Terminal => iced::event::listen_with(keyboard_filter),
            Screen::Connect => Subscription::none(),
        };

        let conn_sub = Subscription::run_with_id(
            params.clone(),
            iced::stream::channel::<Message, _>(100, move |mut output| async move {
                let (srv_tx, mut srv_rx) = mpsc::unbounded_channel::<ServerMessage>();

                let result = connect::connect(
                    params.host.clone(),
                    params.port,
                    params.token.clone(),
                    params.accept_invalid_certs,
                    srv_tx,
                    gui_capabilities(),
                )
                .await;

                match result {
                    connect::ConnectResult::Connected(sender) => {
                        let _ = output.send(Message::Connected(sender)).await;
                        while let Some(msg) = srv_rx.recv().await {
                            let mut batch = vec![msg];
                            while let Ok(msg) = srv_rx.try_recv() {
                                batch.push(msg);
                            }
                            if output.send(Message::ServerMsgBatch(batch)).await.is_err() {
                                break;
                            }
                        }
                        let _ = output.send(Message::Disconnected).await;
                    }
                    connect::ConnectResult::Failed(e) => {
                        let _ = output.send(Message::ConnectionFailed(e)).await;
                    }
                }
            }),
        );

        // Leader timeout subscription: poll every 100ms when awaiting action
        let leader_sub = if self.leader_state.is_awaiting_action() {
            iced::time::every(Duration::from_millis(100)).map(|_| Message::LeaderTimeout)
        } else {
            Subscription::none()
        };

        Subscription::batch([conn_sub, kbd_sub, leader_sub])
    }

    pub fn theme(&self) -> Theme {
        theme::default()
    }

    //  Private helpers

    fn view_connect(&self) -> Element<'_, Message> {
        let title = text("kmux").size(28).color(theme::ACCENT).font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::MONOSPACE
        });
        let subtitle = text("remote terminal v0.1.0").size(12).color(theme::FG_DIM);

        let status_msg = self.mgr.status_msg();
        let form = column![
            title,
            subtitle,
            Space::with_height(16),
            text("Host").size(13).color(theme::FG),
            text_input("127.0.0.1", &self.host)
                .on_input(Message::HostChanged)
                .style(theme::connect_input),
            text("Port").size(13).color(theme::FG),
            text_input("8443", &self.port)
                .on_input(Message::PortChanged)
                .style(theme::connect_input),
            text("Auth Token").size(13).color(theme::FG),
            text_input("paste token here", &self.token)
                .on_input(Message::TokenChanged)
                .on_submit(Message::ConnectPressed)
                .secure(true)
                .style(theme::connect_input),
            Space::with_height(8),
            button(
                text("Connect")
                    .size(14)
                    .width(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Center)
            )
            .style(theme::connect_button)
            .on_press(Message::ConnectPressed)
            .padding([8, 16])
            .width(Length::Fill),
            {
                let msg_text = text(status_msg).size(12);
                if status_msg.starts_with("Connection failed")
                    || status_msg.starts_with("Auth failed")
                {
                    msg_text.color(theme::RED)
                } else {
                    msg_text.color(theme::FG_DIM)
                }
            },
        ]
        .spacing(6)
        .padding(32)
        .max_width(380);

        let styled_form = container(form).style(theme::connect_container);

        container(styled_form)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(theme::BG)),
                ..Default::default()
            })
            .into()
    }

    fn view_terminal(&self) -> Element<'_, Message> {
        let names: Vec<String> = self
            .mgr
            .session_list()
            .iter()
            .map(|s| s.meta.name.clone())
            .collect();
        let active_ref = self.mgr.active_session();

        let rename_state =
            if let LeaderState::RenameEditing { buffer, session } = &self.leader_state {
                Some((session.as_str(), buffer.as_str()))
            } else {
                None
            };

        let bar = session_bar::view(
            &names,
            active_ref,
            self.leader_state.is_leader_active(),
            rename_state,
            Message::SelectSession,
            Message::CloseSession,
            Message::CreateSessionPressed,
            Message::RenameInput,
            Message::RenameSubmit,
        );

        let (metrics, diag) = if self.hud_visible {
            (
                Some(self.mgr.metrics.snapshot(self.force_snapshot_mode)),
                Some(self.mgr.metrics.diag_snapshot()),
            )
        } else {
            (None, None)
        };

        let terminal_area: Element<Message> = if let Some(name) = self.mgr.active_session() {
            if let Some(buf) = self.mgr.buffer(name) {
                terminal_view::view(buf, name, metrics, diag)
            } else {
                text("No output yet").color(theme::FG_DIM).into()
            }
        } else {
            text("No active session -- press Ctrl+B then c to create one")
                .color(theme::FG_DIM)
                .into()
        };

        let status = status_bar::view(
            &self.host_port_display(),
            self.mgr.session_list().len(),
            &self.leader_state,
            self.mgr.active_input_locked(),
            self.mgr.active_term_size(),
            Message::DisconnectPressed,
        );

        let mut content = column![bar, terminal_area, status]
            .width(Length::Fill)
            .height(Length::Fill);

        // Disconnect toast
        if self.disconnect_toast.is_some() {
            content = content.push(
                container(
                    text("Connection lost \u{2014} reconnecting...")
                        .size(14)
                        .color(iced::Color::WHITE),
                )
                .width(Length::Fill)
                .padding(8)
                .style(theme::toast_error),
            );
        }

        // Overlay: help, command palette
        let base: Element<Message> = content.into();

        match &self.leader_state {
            LeaderState::HelpVisible => {
                let help = self.view_help_overlay();
                iced::widget::stack![base, help].into()
            }
            LeaderState::CommandPalette { query, selected } => {
                let palette = self.view_command_palette(query, *selected);
                iced::widget::stack![base, palette].into()
            }
            _ => base,
        }
    }

    fn view_help_overlay(&self) -> Element<'_, Message> {
        let entries = shortcut::shortcut_help_entries();

        let mut col = column![
            text("Keyboard Shortcuts")
                .size(18)
                .color(theme::ACCENT)
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::MONOSPACE
                }),
            Space::with_height(12),
        ]
        .spacing(4);

        for (key, desc) in &entries {
            col = col.push(
                row![
                    text(format!("  {key:>10}"))
                        .size(13)
                        .color(theme::GREEN)
                        .font(Font::MONOSPACE),
                    text(format!("  {desc}")).size(13).color(theme::FG),
                ]
                .spacing(8),
            );
        }

        col = col.push(Space::with_height(12));
        col = col.push(text("Press any key to close").size(11).color(theme::FG_DIM));

        let help_box = container(col.padding(24))
            .style(theme::command_palette_container)
            .max_width(480);

        container(
            container(help_box)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::overlay_container)
        .into()
    }

    fn view_command_palette(&self, query: &str, selected: usize) -> Element<'_, Message> {
        let filtered = shortcut::filter_commands(query);

        let input = text_input("Type a command...", query)
            .on_input(Message::CommandPaletteInput)
            .on_submit(Message::CommandPaletteSelect)
            .size(14)
            .style(theme::connect_input)
            .id(command_palette_input_id());

        let mut items_col = column![].spacing(0);
        for (i, entry) in filtered.iter().take(10).enumerate() {
            let is_selected = i == selected;
            let style = if is_selected {
                theme::command_palette_item_selected
            } else {
                theme::command_palette_item
            };
            let text_color = if is_selected {
                iced::Color::WHITE
            } else {
                theme::FG
            };
            let hint_color = if is_selected {
                iced::Color::from_rgba(1.0, 1.0, 1.0, 0.6)
            } else {
                theme::FG_DIM
            };

            let label = entry.label.clone();
            let hint = entry.shortcut_hint.clone();
            let item = container(
                row![
                    text(label).size(13).color(text_color),
                    Space::with_width(Length::Fill),
                    text(hint).size(11).color(hint_color),
                ]
                .padding([4, 8])
                .width(Length::Fill),
            )
            .width(Length::Fill)
            .style(style);

            items_col = items_col.push(item);
        }

        let palette = column![input, items_col]
            .spacing(4)
            .padding(12)
            .max_width(400);

        let palette_box = container(palette).style(theme::command_palette_container);

        // Position at top center
        let positioned = column![
            Space::with_height(60),
            container(palette_box)
                .width(Length::Fill)
                .center_x(Length::Fill),
        ]
        .width(Length::Fill)
        .height(Length::Fill);

        container(positioned)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::overlay_container)
            .into()
    }

    /// Write a per-connection metadata log on first successful authentication.
    fn write_connection_log(&self) {
        let connected_at = {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let (y, mo, d, h, mi, s) = epoch_secs_to_ymd_hms(secs);
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
        };
        let content = format!(
            "instance_id: {}\nclient_version: {}\nserver_version: {}\nprotocol_version: {}\ndestination: {}:{}\ntransport: QUIC\nconnected_at: {}\n",
            self.instance_id,
            env!("CARGO_PKG_VERSION"),
            self.mgr.server_version.as_deref().unwrap_or("unknown"),
            PROTOCOL_VERSION,
            self.mgr.host(),
            self.mgr.port(),
            connected_at,
        );
        match kmux_protocol::dirs::connection_log_path(&self.instance_id) {
            Ok(path) => {
                if let Err(e) = std::fs::write(&path, &content) {
                    warn!("Failed to write connection log {}: {e}", path.display());
                }
            }
            Err(e) => warn!("Failed to get connection log path: {e}"),
        }
    }
}

fn command_palette_input_id() -> text_input::Id {
    text_input::Id::new("command-palette-input")
}

/// Convert Unix timestamp (seconds) to (year, month, day, hour, minute, second) UTC.
fn epoch_secs_to_ymd_hms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = secs / 86400;
    let time = secs % 86400;
    let h = (time / 3600) as u32;
    let mi = ((time % 3600) / 60) as u32;
    let s = (time % 60) as u32;
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y } as u32;
    (y, mo, d, h, mi, s)
}

fn keyboard_filter(
    event: Event,
    status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    match event {
        Event::Keyboard(ref kbd_event) => {
            debug!(
                ?kbd_event,
                ?status,
                "keyboard_filter: received keyboard event"
            );
            match kbd_event {
                iced::keyboard::Event::KeyPressed {
                    key,
                    modifiers,
                    text,
                    ..
                } => {
                    let text_owned = text.as_ref().map(|t| t.to_string());
                    Some(Message::RawKeyEvent {
                        key: key.clone(),
                        modifiers: *modifiers,
                        text: text_owned,
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn key_to_bytes(
    key: iced::keyboard::Key,
    modifiers: iced::keyboard::Modifiers,
    text: Option<String>,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    use iced::keyboard::Key;
    use iced::keyboard::key::Named;

    match key {
        Key::Character(c) => {
            let s = c.as_str();
            if modifiers.control()
                && let Some(ch) = s.chars().next()
            {
                let lower = ch.to_ascii_lowercase();
                if lower.is_ascii_alphabetic() {
                    return Some(vec![lower as u8 - b'a' + 1]);
                }
            }
            if let Some(t) = text {
                Some(t.as_bytes().to_vec())
            } else {
                Some(s.as_bytes().to_vec())
            }
        }
        Key::Named(named) => {
            let bytes: &[u8] = match named {
                Named::Space => b" ",
                Named::Enter => b"\r",
                Named::Tab => b"\t",
                Named::Backspace => b"\x7f",
                Named::Escape => b"\x1b",
                Named::Delete => b"\x1b[3~",
                Named::ArrowUp => {
                    if app_cursor {
                        b"\x1bOA"
                    } else {
                        b"\x1b[A"
                    }
                }
                Named::ArrowDown => {
                    if app_cursor {
                        b"\x1bOB"
                    } else {
                        b"\x1b[B"
                    }
                }
                Named::ArrowRight => {
                    if app_cursor {
                        b"\x1bOC"
                    } else {
                        b"\x1b[C"
                    }
                }
                Named::ArrowLeft => {
                    if app_cursor {
                        b"\x1bOD"
                    } else {
                        b"\x1b[D"
                    }
                }
                Named::Home => {
                    if app_cursor {
                        b"\x1bOH"
                    } else {
                        b"\x1b[H"
                    }
                }
                Named::End => {
                    if app_cursor {
                        b"\x1bOF"
                    } else {
                        b"\x1b[F"
                    }
                }
                Named::PageUp => b"\x1b[5~",
                Named::PageDown => b"\x1b[6~",
                Named::F1 => b"\x1bOP",
                Named::F2 => b"\x1bOQ",
                Named::F3 => b"\x1bOR",
                Named::F4 => b"\x1bOS",
                Named::F5 => b"\x1b[15~",
                Named::F6 => b"\x1b[17~",
                Named::F7 => b"\x1b[18~",
                Named::F8 => b"\x1b[19~",
                Named::F9 => b"\x1b[20~",
                Named::F10 => b"\x1b[21~",
                Named::F11 => b"\x1b[23~",
                Named::F12 => b"\x1b[24~",
                Named::Insert => b"\x1b[2~",
                _ => return None,
            };
            Some(bytes.to_vec())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::Named;
    use iced::keyboard::{Key, Modifiers};

    fn no_mods() -> Modifiers {
        Modifiers::empty()
    }

    #[test]
    fn escape_produces_0x1b() {
        let result = key_to_bytes(Key::Named(Named::Escape), no_mods(), None, false);
        assert_eq!(result, Some(vec![0x1b]));
    }

    #[test]
    fn insert_produces_csi_2_tilde() {
        let result = key_to_bytes(Key::Named(Named::Insert), no_mods(), None, false);
        assert_eq!(result, Some(b"\x1b[2~".to_vec()));
    }

    #[test]
    fn arrow_up_normal_mode() {
        let result = key_to_bytes(Key::Named(Named::ArrowUp), no_mods(), None, false);
        assert_eq!(result, Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn arrow_up_app_cursor_mode() {
        let result = key_to_bytes(Key::Named(Named::ArrowUp), no_mods(), None, true);
        assert_eq!(result, Some(b"\x1bOA".to_vec()));
    }

    #[test]
    fn ctrl_c_produces_etx() {
        let result = key_to_bytes(Key::Character("c".into()), Modifiers::CTRL, None, false);
        assert_eq!(result, Some(vec![0x03]));
    }

    #[test]
    fn enter_produces_cr() {
        let result = key_to_bytes(Key::Named(Named::Enter), no_mods(), None, false);
        assert_eq!(result, Some(vec![0x0d]));
    }

    #[test]
    fn backspace_produces_del() {
        let result = key_to_bytes(Key::Named(Named::Backspace), no_mods(), None, false);
        assert_eq!(result, Some(vec![0x7f]));
    }
}
