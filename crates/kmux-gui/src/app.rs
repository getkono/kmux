use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iced::futures::SinkExt as _;
use iced::widget::{Space, button, column, container, row, text, text_input};
use iced::{Element, Event, Font, Length, Subscription, Task, Theme};
use kmux_protocol::messages::{
    ClientId, ClientMessage, SequenceNo, ServerMessage, SessionEventMsg, SessionInfo,
    SessionStatus, TermSize, epoch_millis,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use kmux_client::connect;
use kmux_client::grid::{CellGrid, GridPos, Selection, SelectionMode};
use kmux_client::metrics::RenderMetrics;

use crate::shortcut::{self, LEADER_TIMEOUT, LeaderState, ShortcutAction};
use crate::{session_bar, status_bar, terminal_view, theme};

/// Try to read the auth token from `$XDG_RUNTIME_DIR/kmux/token`.
/// Returns `None` if the env var is unset, the file is missing, or any I/O error occurs.
fn read_local_token() -> Option<String> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let path = std::path::Path::new(&runtime_dir)
        .join("kmux")
        .join("token");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

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

/// Per-session synchronisation state for ordering protection.
#[derive(Default)]
enum SessionSync {
    /// Receiving diffs normally; `expected` is the next expected seqno.
    Synced { expected: SequenceNo },
    /// Awaiting a fresh `TerminalSnapshot`; discard any `TerminalUpdate` messages
    /// (they may be stale leftovers from a previous uni stream).
    #[default]
    AwaitingSync,
}

/// Top-level application state.
#[derive(Default)]
#[allow(non_camel_case_types)]
pub struct kmuxApp {
    screen: Screen,
    connect_params: Option<ConnectParams>,
    ws_sender: Option<mpsc::UnboundedSender<ClientMessage>>,

    /// Terminal cell grids, keyed by session name.
    buffers: HashMap<String, CellGrid>,
    active_session: Option<String>,
    session_list: Vec<SessionInfo>,
    next_request_id: u64,

    /// Per-session sync state: tracks expected seqno and whether we are
    /// awaiting a fresh snapshot (discarding stale diffs from old streams).
    session_sync: HashMap<String, SessionSync>,

    // Connect form
    host: String,
    port: String,
    token: String,
    accept_invalid_certs: bool,
    status_msg: String,

    // Reconnection state
    last_connect_params: Option<ConnectParams>,
    disconnect_toast: Option<Instant>,

    // Observability
    metrics: RenderMetrics,
    hud_visible: bool,

    // Full-snapshot mode: server sends complete grid snapshots instead of diffs.
    force_snapshot_mode: bool,

    // Leader key state machine
    leader_state: LeaderState,

    // Per-session input lock tracking
    input_locked: HashMap<String, bool>,

    // Client identity assigned by server
    client_id: Option<ClientId>,
}

impl kmuxApp {
    pub fn new(accept_invalid_certs: bool) -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: "8443".to_string(),
            token: read_local_token().unwrap_or_default(),
            accept_invalid_certs,
            ..Default::default()
        }
    }

    fn next_rid(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    /// Send `Attach { last_seqno: None }` and transition the session to `AwaitingSync`.
    fn attach_fresh(&mut self, session: String) {
        self.session_sync
            .insert(session.clone(), SessionSync::AwaitingSync);
        self.send_ws(ClientMessage::Attach {
            session,
            last_seqno: None,
        });
    }

    fn send_ws(&self, msg: ClientMessage) {
        if let Some(tx) = &self.ws_sender {
            match tx.send(msg) {
                Ok(()) => debug!("send_ws: message sent"),
                Err(e) => warn!("send_ws: channel send failed: {e}"),
            }
        } else {
            debug!("send_ws: no ws_sender available, message dropped");
        }
    }

    /// Get the current terminal size for the active session.
    fn active_term_size(&self) -> Option<(u16, u16)> {
        self.active_session
            .as_ref()
            .and_then(|s| self.buffers.get(s))
            .map(|b| (b.rows as u16, b.cols as u16))
    }

    /// Get the host:port string for display.
    fn host_port_display(&self) -> String {
        if let Some(p) = &self.connect_params {
            format!("{}:{}", p.host, p.port)
        } else if let Some(p) = &self.last_connect_params {
            format!("{}:{}", p.host, p.port)
        } else {
            String::new()
        }
    }

    /// Dispatch a ShortcutAction from the leader key system.
    fn dispatch_action(&mut self, action: ShortcutAction) -> Task<Message> {
        match action {
            ShortcutAction::CreateSession => {
                self.leader_state = LeaderState::Idle;
                self.update(Message::CreateSessionPressed)
            }
            ShortcutAction::CloseSession => {
                if let Some(session) = self.active_session.clone() {
                    self.leader_state = LeaderState::ConfirmClose { session };
                } else {
                    self.leader_state = LeaderState::Idle;
                }
                Task::none()
            }
            ShortcutAction::NextSession => {
                self.leader_state = LeaderState::Idle;
                self.cycle_session(1)
            }
            ShortcutAction::PrevSession => {
                self.leader_state = LeaderState::Idle;
                self.cycle_session(-1)
            }
            ShortcutAction::JumpToSession(idx) => {
                self.leader_state = LeaderState::Idle;
                if idx < self.session_list.len() {
                    let name = self.session_list[idx].name.clone();
                    self.update(Message::SelectSession(name))
                } else {
                    Task::none()
                }
            }
            ShortcutAction::RenameSession => {
                if let Some(session) = self.active_session.clone() {
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
                if let Some(session) = self.active_session.clone() {
                    self.leader_state = LeaderState::SignalMenu { session };
                } else {
                    self.leader_state = LeaderState::Idle;
                }
                Task::none()
            }
            ShortcutAction::ToggleInputLock => {
                self.leader_state = LeaderState::Idle;
                if let Some(session) = self.active_session.clone() {
                    let locked = self.input_locked.get(&session).copied().unwrap_or(false);
                    if locked {
                        self.send_ws(ClientMessage::ReleaseInputLock { session });
                    } else {
                        self.send_ws(ClientMessage::RequestInputLock { session });
                    }
                }
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
                self.send_ws(ClientMessage::SetSnapshotMode {
                    enabled: self.force_snapshot_mode,
                });
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
                if let Some(session) = &self.active_session {
                    self.send_ws(ClientMessage::PtyInput {
                        session: session.clone(),
                        data: vec![0x02],
                    });
                }
                Task::none()
            }
            ShortcutAction::ScrollPageUp => {
                self.leader_state = LeaderState::Idle;
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
                {
                    grid.scroll_up(grid.rows);
                }
                Task::none()
            }
            ShortcutAction::ScrollPageDown => {
                self.leader_state = LeaderState::Idle;
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
                {
                    grid.scroll_down(grid.rows);
                }
                Task::none()
            }
        }
    }

    /// Cycle to the next/prev session by offset.
    fn cycle_session(&mut self, offset: i32) -> Task<Message> {
        if self.session_list.is_empty() {
            return Task::none();
        }
        let current_idx = self
            .active_session
            .as_ref()
            .and_then(|name| self.session_list.iter().position(|s| &s.name == name))
            .unwrap_or(0);
        let len = self.session_list.len() as i32;
        let new_idx = ((current_idx as i32 + offset).rem_euclid(len)) as usize;
        let name = self.session_list[new_idx].name.clone();
        self.update(Message::SelectSession(name))
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
                self.status_msg = "Connecting...".to_string();
                Task::none()
            }

            Message::DisconnectPressed => {
                self.connect_params = None;
                self.ws_sender = None;
                self.screen = Screen::Connect;
                self.buffers.clear();
                self.active_session = None;
                self.session_list.clear();
                self.session_sync.clear();
                self.input_locked.clear();
                self.leader_state = LeaderState::Idle;
                self.status_msg = "Disconnected".to_string();
                Task::none()
            }

            //  Async events
            Message::Connected(sender) => {
                self.ws_sender = Some(sender);
                self.screen = Screen::Terminal;
                if let Some(p) = &self.connect_params {
                    self.status_msg = format!("Connected to {}:{}", p.host, p.port);
                }
                info!("Connected to kmux-server");
                let rid = self.next_rid();
                self.send_ws(ClientMessage::SessionList { request_id: rid });
                Task::none()
            }

            Message::ConnectionFailed(reason) => {
                self.connect_params = None;
                self.ws_sender = None;
                self.status_msg = format!("Connection failed: {reason}");
                Task::none()
            }

            Message::ServerMsgBatch(msgs) => {
                self.metrics.record_batch(msgs.len());
                let tasks: Vec<Task<Message>> = msgs
                    .into_iter()
                    .map(|msg| self.handle_server_message(msg))
                    .collect();
                Task::batch(tasks)
            }

            //  Session management
            Message::SelectSession(name) => {
                if let Some(prev) = self.active_session.take() {
                    self.send_ws(ClientMessage::Detach { session: prev });
                }
                if let Some(buf) = self.buffers.get_mut(&name) {
                    buf.clear();
                }
                self.active_session = Some(name.clone());
                self.attach_fresh(name);
                Task::none()
            }

            Message::CreateSessionPressed => {
                if self.ws_sender.is_none() {
                    warn!("CreateSessionPressed: no active connection, ignoring");
                    self.status_msg = "Not connected -- cannot create session".to_string();
                    return Task::none();
                }
                let rid = self.next_rid();
                let name = format!("session-{rid}");
                self.status_msg = "Creating session...".to_string();
                self.send_ws(ClientMessage::SessionCreate {
                    request_id: rid,
                    name,
                    program: None,
                    args: vec![],
                    size: TermSize { rows: 24, cols: 80 },
                });
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
                self.ws_sender = None;
                self.last_connect_params = self.connect_params.take();
                self.status_msg = "Connection lost \u{2014} reconnecting in 3s...".to_string();
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
                    self.connect_params = Some(params);
                    self.status_msg = "Reconnecting...".to_string();
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
                self.send_ws(ClientMessage::SetSnapshotMode {
                    enabled: self.force_snapshot_mode,
                });
                Task::none()
            }

            // Clipboard paste
            Message::ClipboardPaste(contents) => {
                if let Some(text) = contents
                    && !text.is_empty()
                {
                    if let Some(session) = &self.active_session {
                        let locked = self.input_locked.get(session).copied().unwrap_or(false);
                        if locked {
                            self.status_msg = "Input locked on this session".to_string();
                            return Task::none();
                        }
                        self.send_ws(ClientMessage::PtyPaste {
                            session: session.clone(),
                            data: text,
                        });
                    } else {
                        self.status_msg =
                            "No active session -- press Ctrl+B then c to create one".to_string();
                    }
                }
                Task::none()
            }

            //  Scroll terminal
            Message::ScrollTerminal(delta) => {
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
                {
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
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get(name)
                {
                    let sgr = grid.modes().sgr_mouse();
                    let bytes = encode_mouse_scroll(col, row, lines, sgr);
                    if !bytes.is_empty() {
                        let locked = self.input_locked.get(name).copied().unwrap_or(false);
                        if !locked {
                            self.send_ws(ClientMessage::PtyInput {
                                session: name.clone(),
                                data: bytes,
                            });
                        }
                    }
                }
                Task::none()
            }

            //  Terminal resize
            Message::TerminalResized { rows, cols } => {
                if let Some(name) = &self.active_session {
                    if let Some(buf) = self.buffers.get_mut(name) {
                        buf.resize(rows, cols);
                    }
                    self.send_ws(ClientMessage::Resize {
                        session: name.clone(),
                        size: TermSize { rows, cols },
                    });
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
                    if !new_name.is_empty() && new_name != session {
                        let rid = self.next_rid();
                        self.send_ws(ClientMessage::SessionRename {
                            request_id: rid,
                            session,
                            new_name,
                        });
                    }
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
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
                {
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
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
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
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
                {
                    if direction > 0 {
                        grid.scroll_up(direction as usize);
                    } else if direction < 0 {
                        grid.scroll_down((-direction) as usize);
                    }
                    // Update selection end to viewport edge
                    if let Some(sel) = grid.selection().copied() {
                        use GridPos;
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
                    if let Some(name) = &self.active_session
                        && let Some(grid) = self.buffers.get_mut(name)
                    {
                        grid.scroll_up(grid.rows);
                    }
                    return Task::none();
                }
                if modifiers.shift() && key == Key::Named(Named::PageDown) {
                    if let Some(name) = &self.active_session
                        && let Some(grid) = self.buffers.get_mut(name)
                    {
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
                        if let Some(name) = &self.active_session
                            && let Some(grid) = self.buffers.get(name)
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
                    && let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
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
                        let rid = self.next_rid();
                        self.send_ws(ClientMessage::SessionClose {
                            request_id: rid,
                            name: session,
                        });
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
                    self.send_ws(ClientMessage::Signal { session, signal });
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
        if let Some(session) = &self.active_session
            && let Some(grid) = self.buffers.get_mut(session)
        {
            grid.scroll_to_bottom();
        }

        let app_cursor = self
            .active_session
            .as_ref()
            .and_then(|s| self.buffers.get(s))
            .map(|b| b.app_cursor())
            .unwrap_or(false);
        let bytes = key_to_bytes(key, modifiers, text_val, app_cursor);
        if let Some(bytes) = bytes {
            if let Some(session) = &self.active_session {
                // Check input lock
                let locked = self.input_locked.get(session).copied().unwrap_or(false);
                if locked {
                    self.status_msg = "Input locked on this session".to_string();
                    return Task::none();
                }
                self.send_ws(ClientMessage::PtyInput {
                    session: session.clone(),
                    data: bytes,
                });
            } else {
                self.status_msg =
                    "No active session -- press Ctrl+B then c to create one".to_string();
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

    fn handle_server_message(&mut self, msg: ServerMessage) -> Task<Message> {
        match msg {
            ServerMessage::AuthResult {
                success,
                reason,
                client_id,
            } => {
                if success {
                    self.client_id = client_id;
                } else {
                    warn!("Auth failed: {:?}", reason);
                    self.status_msg = format!("Auth failed: {}", reason.unwrap_or_default());
                    self.connect_params = None;
                    self.ws_sender = None;
                    self.screen = Screen::Connect;
                }
                Task::none()
            }

            ServerMessage::SessionListResult { sessions, .. } => {
                self.session_list = sessions.clone();
                for info in &sessions {
                    self.buffers.entry(info.name.clone()).or_default();
                }
                if self.active_session.is_none()
                    && let Some(first) = sessions.first()
                {
                    self.active_session = Some(first.name.clone());
                    self.attach_fresh(first.name.clone());
                }
                Task::none()
            }

            ServerMessage::SessionCreated { name, .. } => {
                let size = TermSize::default();
                self.buffers.entry(name.clone()).or_default();
                self.session_list
                    .push(kmux_protocol::messages::SessionInfo {
                        name: name.clone(),
                        program: String::new(),
                        size,
                        attached_clients: vec![],
                        status: SessionStatus::Running,
                    });
                if let Some(prev) = self.active_session.take() {
                    self.send_ws(ClientMessage::Detach { session: prev });
                }
                self.active_session = Some(name.clone());
                self.status_msg = format!("Session '{name}' created");
                self.attach_fresh(name);
                Task::none()
            }

            ServerMessage::SessionClosed { name, .. } => {
                self.buffers.remove(&name);
                self.session_sync.remove(&name);
                self.input_locked.remove(&name);
                self.session_list.retain(|s| s.name != name);
                if self.active_session.as_deref() == Some(&name) {
                    self.active_session = self.session_list.first().map(|s| s.name.clone());
                    if let Some(sess) = self.active_session.clone() {
                        self.attach_fresh(sess);
                    }
                }
                Task::none()
            }

            ServerMessage::TerminalSnapshot {
                session,
                snapshot,
                seqno,
                sent_at_ms,
            } => {
                let start = Instant::now();
                let grid = self.buffers.entry(session.clone()).or_default();
                grid.apply_snapshot(snapshot);
                self.session_sync.insert(
                    session,
                    SessionSync::Synced {
                        expected: SequenceNo(seqno.0 + 1),
                    },
                );
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                self.metrics.record_apply(sent_at_ms, elapsed_ms);
                Task::none()
            }

            ServerMessage::TerminalUpdate {
                session,
                diff,
                seqno,
                sent_at_ms,
            } => {
                match self.session_sync.get(&session) {
                    Some(SessionSync::AwaitingSync) => {
                        debug!("Discarding stale TerminalUpdate for '{session}' (awaiting sync)");
                        self.metrics.record_stale_discard(&session);
                        return Task::none();
                    }
                    Some(SessionSync::Synced { expected }) if seqno != *expected => {
                        warn!(
                            "Seqno gap on '{session}': expected {:?}, got {:?} \u{2014} re-attaching",
                            expected, seqno
                        );
                        self.metrics.record_seqno_gap(&session, expected.0, seqno.0);
                        self.metrics.record_resync(&session, "seqno gap");
                        if let Some(grid) = self.buffers.get_mut(&session) {
                            grid.clear();
                        }
                        self.attach_fresh(session);
                        return Task::none();
                    }
                    _ => {}
                }

                let start = Instant::now();
                let diff = Arc::unwrap_or_clone(diff);
                let op_count = diff.ops.len();
                if let Some(grid) = self.buffers.get_mut(&session) {
                    grid.apply_diff(diff);
                    self.metrics.record_diff_stats(op_count);
                    debug!(
                        session,
                        generation = grid.generation(),
                        "TerminalUpdate applied"
                    );
                }
                self.session_sync.insert(
                    session,
                    SessionSync::Synced {
                        expected: SequenceNo(seqno.0 + 1),
                    },
                );
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                let net_apply_ms = epoch_millis().saturating_sub(sent_at_ms) as f64;
                self.metrics.record_apply(sent_at_ms, elapsed_ms);
                if op_count > 100 {
                    debug!(
                        op_count,
                        apply_ms = format!("{elapsed_ms:.2}"),
                        net_apply_ms = format!("{net_apply_ms:.1}"),
                        "large diff applied"
                    );
                    self.metrics.record_large_diff(net_apply_ms);
                }
                Task::none()
            }

            ServerMessage::CursorUpdate {
                session,
                cursor,
                modes,
                seqno,
                sent_at_ms,
            } => {
                match self.session_sync.get(&session) {
                    Some(SessionSync::AwaitingSync) => {
                        debug!("Discarding stale CursorUpdate for '{session}' (awaiting sync)");
                        self.metrics.record_stale_discard(&session);
                        return Task::none();
                    }
                    Some(SessionSync::Synced { expected }) if seqno != *expected => {
                        warn!(
                            "Seqno gap on '{session}': expected {:?}, got {:?} \u{2014} re-attaching",
                            expected, seqno
                        );
                        self.metrics.record_seqno_gap(&session, expected.0, seqno.0);
                        self.metrics.record_resync(&session, "seqno gap");
                        if let Some(grid) = self.buffers.get_mut(&session) {
                            grid.clear();
                        }
                        self.attach_fresh(session);
                        return Task::none();
                    }
                    _ => {}
                }

                let start = Instant::now();
                if let Some(grid) = self.buffers.get_mut(&session) {
                    grid.apply_cursor_update(cursor, modes);
                }
                self.session_sync.insert(
                    session,
                    SessionSync::Synced {
                        expected: SequenceNo(seqno.0 + 1),
                    },
                );
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                self.metrics.record_apply(sent_at_ms, elapsed_ms);
                Task::none()
            }

            #[allow(deprecated)]
            ServerMessage::PtyOutput { .. } => Task::none(),

            ServerMessage::SyncReset { session } => {
                if let Some(grid) = self.buffers.get_mut(&session) {
                    grid.clear();
                }
                self.metrics.record_resync(&session, "server sync reset");
                self.session_sync.insert(session, SessionSync::AwaitingSync);
                Task::none()
            }

            ServerMessage::Event { event } => {
                info!("Server event: {:?}", event);
                // Handle rename events to update local state
                if let SessionEventMsg::Renamed { old_name, new_name } = event {
                    self.apply_rename(&old_name, &new_name);
                }
                Task::none()
            }

            ServerMessage::Lagged {
                session,
                missed_count,
            } => {
                warn!("Lagged on session '{session}': missed {missed_count} diffs, re-attaching");
                self.metrics.record_lag(&session, missed_count);
                self.metrics.record_resync(&session, "lagged");
                if let Some(grid) = self.buffers.get_mut(&session) {
                    grid.clear();
                }
                self.attach_fresh(session);
                Task::none()
            }

            ServerMessage::Error { message, .. } => {
                warn!("Server error: {message}");
                self.status_msg = format!("Error: {message}");
                Task::none()
            }

            ServerMessage::SessionRenamed { old_name, new_name } => {
                self.apply_rename(&old_name, &new_name);
                Task::none()
            }

            ServerMessage::InputLockGranted { session } => {
                self.input_locked.insert(session.clone(), true);
                self.status_msg = format!("Input lock acquired on '{session}'");
                Task::none()
            }

            ServerMessage::InputLockDenied { session, holder } => {
                self.status_msg = format!(
                    "Input lock denied on '{session}' (held by client {:?})",
                    holder
                );
                Task::none()
            }

            ServerMessage::InputLockReleased { session } => {
                self.input_locked.insert(session.clone(), false);
                self.status_msg = format!("Input lock released on '{session}'");
                Task::none()
            }

            _ => Task::none(),
        }
    }

    /// Apply a session rename across all local state.
    fn apply_rename(&mut self, old_name: &str, new_name: &str) {
        if let Some(buf) = self.buffers.remove(old_name) {
            self.buffers.insert(new_name.to_string(), buf);
        }
        if let Some(sync) = self.session_sync.remove(old_name) {
            self.session_sync.insert(new_name.to_string(), sync);
        }
        if let Some(locked) = self.input_locked.remove(old_name) {
            self.input_locked.insert(new_name.to_string(), locked);
        }
        for info in &mut self.session_list {
            if info.name == old_name {
                info.name = new_name.to_string();
            }
        }
        if self.active_session.as_deref() == Some(old_name) {
            self.active_session = Some(new_name.to_string());
        }
    }

    fn view_connect(&self) -> Element<'_, Message> {
        let title = text("kmux").size(28).color(theme::ACCENT).font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::MONOSPACE
        });
        let subtitle = text("remote terminal v0.1.0").size(12).color(theme::FG_DIM);

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
                let msg_text = text(&self.status_msg).size(12);
                if self.status_msg.starts_with("Connection failed")
                    || self.status_msg.starts_with("Auth failed")
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
        let names: Vec<String> = self.session_list.iter().map(|s| s.name.clone()).collect();
        let active_ref = self.active_session.as_deref();

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
                Some(self.metrics.snapshot(self.force_snapshot_mode)),
                Some(self.metrics.diag_snapshot()),
            )
        } else {
            (None, None)
        };

        let terminal_area: Element<Message> = if let Some(name) = &self.active_session {
            if let Some(buf) = self.buffers.get(name) {
                terminal_view::view(buf, name, metrics, diag)
            } else {
                text("No output yet").color(theme::FG_DIM).into()
            }
        } else {
            text("No active session -- press Ctrl+B then c to create one")
                .color(theme::FG_DIM)
                .into()
        };

        // Input lock status for active session
        let input_locked = self
            .active_session
            .as_ref()
            .and_then(|s| self.input_locked.get(s))
            .copied()
            .unwrap_or(false);

        let status = status_bar::view(
            &self.host_port_display(),
            self.session_list.len(),
            &self.leader_state,
            input_locked,
            self.active_term_size(),
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
}

fn command_palette_input_id() -> text_input::Id {
    text_input::Id::new("command-palette-input")
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

/// Encode mouse scroll events as terminal escape sequences.
///
/// `col` and `row` are 1-based terminal coordinates.
/// `lines` > 0 means scroll up, < 0 means scroll down.
/// Each line generates one escape sequence (matching xterm behavior).
fn encode_mouse_scroll(col: u16, row: u16, lines: i32, sgr: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let count = lines.unsigned_abs() as usize;
    // Button 64 = scroll up, 65 = scroll down (xterm convention).
    let button: u8 = if lines > 0 { 64 } else { 65 };

    for _ in 0..count.min(255) {
        if sgr {
            // SGR format: \x1b[<{button};{col};{row}M
            let seq = format!("\x1b[<{};{};{}M", button, col, row);
            out.extend_from_slice(seq.as_bytes());
        } else {
            // Legacy X10/normal format: \x1b[M{cb}{cx}{cy}
            // cb = button + 32, cx = col + 32, cy = row + 32
            let cb = button + 32;
            let cx = (col as u8).saturating_add(32);
            let cy = (row as u8).saturating_add(32);
            out.extend_from_slice(&[0x1b, b'[', b'M', cb, cx, cy]);
        }
    }
    out
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

    #[test]
    fn sgr_scroll_up() {
        let bytes = encode_mouse_scroll(10, 5, 1, true);
        assert_eq!(bytes, b"\x1b[<64;10;5M");
    }

    #[test]
    fn sgr_scroll_down() {
        let bytes = encode_mouse_scroll(10, 5, -1, true);
        assert_eq!(bytes, b"\x1b[<65;10;5M");
    }

    #[test]
    fn legacy_scroll_up() {
        let bytes = encode_mouse_scroll(10, 5, 1, false);
        // cb = 64+32 = 96, cx = 10+32 = 42, cy = 5+32 = 37
        assert_eq!(bytes, &[0x1b, b'[', b'M', 96, 42, 37]);
    }

    #[test]
    fn legacy_scroll_down() {
        let bytes = encode_mouse_scroll(10, 5, -1, false);
        // cb = 65+32 = 97, cx = 10+32 = 42, cy = 5+32 = 37
        assert_eq!(bytes, &[0x1b, b'[', b'M', 97, 42, 37]);
    }

    #[test]
    fn multiple_lines_generate_multiple_sequences() {
        let bytes = encode_mouse_scroll(1, 1, 3, true);
        assert_eq!(bytes, b"\x1b[<64;1;1M\x1b[<64;1;1M\x1b[<64;1;1M");
    }

    #[test]
    fn zero_lines_produces_empty() {
        let bytes = encode_mouse_scroll(1, 1, 0, true);
        assert!(bytes.is_empty());
    }
}
