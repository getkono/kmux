use std::collections::HashMap;

use iced::futures::SinkExt as _;
use iced::widget::{button, column, container, row, text, text_input};
use iced::{Element, Event, Length, Subscription, Task, Theme};
use smux_protocol::messages::{ClientMessage, ServerMessage, SessionInfo, SessionStatus, TermSize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::terminal_view::CellGrid;
use crate::{connect, session_bar, terminal_view, theme};

/// Connection parameters used as a subscription ID (triggers reconnect on change).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ConnectParams {
    host: String,
    port: u16,
    token: String,
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
    #[allow(dead_code)]
    CloseSession(String),

    // Raw keyboard event from subscription
    RawKeyEvent {
        key: iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
        text: Option<String>,
    },

    // Terminal canvas resize detected
    TerminalResized {
        rows: u16,
        cols: u16,
    },
}

/// Top-level application state.
#[derive(Default)]
pub struct SmuxApp {
    screen: Screen,
    connect_params: Option<ConnectParams>,
    ws_sender: Option<mpsc::UnboundedSender<ClientMessage>>,

    /// Terminal cell grids, keyed by session name.
    buffers: HashMap<String, CellGrid>,
    active_session: Option<String>,
    session_list: Vec<SessionInfo>,
    next_request_id: u64,

    // Connect form
    host: String,
    port: String,
    token: String,
    status_msg: String,
}

impl SmuxApp {
    pub fn new() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: "8443".to_string(),
            ..Default::default()
        }
    }

    fn next_rid(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
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

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            // ── Connect form ─────────────────────────────────────────
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
                });
                self.status_msg = "Connecting…".to_string();
                Task::none()
            }

            Message::DisconnectPressed => {
                self.connect_params = None;
                self.ws_sender = None;
                self.screen = Screen::Connect;
                self.buffers.clear();
                self.active_session = None;
                self.session_list.clear();
                self.status_msg = "Disconnected".to_string();
                Task::none()
            }

            // ── Async events ─────────────────────────────────────────
            Message::Connected(sender) => {
                self.ws_sender = Some(sender);
                self.screen = Screen::Terminal;
                if let Some(p) = &self.connect_params {
                    self.status_msg = format!("Connected to {}:{}", p.host, p.port);
                }
                info!("Connected to smux-server");
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
                let tasks: Vec<Task<Message>> = msgs
                    .into_iter()
                    .map(|msg| self.handle_server_message(msg))
                    .collect();
                Task::batch(tasks)
            }

            // ── Session management ───────────────────────────────────
            Message::SelectSession(name) => {
                if let Some(prev) = self.active_session.take() {
                    self.send_ws(ClientMessage::Detach { session: prev });
                }
                if let Some(buf) = self.buffers.get_mut(&name) {
                    buf.clear();
                }
                self.active_session = Some(name.clone());
                self.send_ws(ClientMessage::Attach {
                    session: name,
                    last_seqno: None,
                });
                Task::none()
            }

            Message::CreateSessionPressed => {
                if self.ws_sender.is_none() {
                    warn!("CreateSessionPressed: no active connection, ignoring");
                    self.status_msg = "Not connected — cannot create session".to_string();
                    return Task::none();
                }
                let rid = self.next_rid();
                let name = format!("session-{rid}");
                self.status_msg = "Creating session…".to_string();
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
                let rid = self.next_rid();
                self.send_ws(ClientMessage::SessionClose {
                    request_id: rid,
                    name,
                });
                Task::none()
            }

            // ── Keyboard input ───────────────────────────────────────
            Message::RawKeyEvent {
                key,
                modifiers,
                text,
            } => {
                let app_cursor = self
                    .active_session
                    .as_ref()
                    .and_then(|s| self.buffers.get(s))
                    .map(|b| b.app_cursor())
                    .unwrap_or(false);
                let bytes = key_to_bytes(key, modifiers, text, app_cursor);
                match &bytes {
                    Some(b) => debug!(byte_count = b.len(), "RawKeyEvent: mapped to bytes"),
                    None => debug!("RawKeyEvent: key_to_bytes returned None"),
                }
                if let Some(bytes) = bytes {
                    if let Some(session) = &self.active_session {
                        debug!(
                            session,
                            byte_count = bytes.len(),
                            "RawKeyEvent: forwarding to PTY"
                        );
                        self.send_ws(ClientMessage::PtyInput {
                            session: session.clone(),
                            data: bytes,
                        });
                    } else {
                        debug!("RawKeyEvent: dropped (no active session)");
                        self.status_msg = "No active session — press [+] to create one".to_string();
                    }
                }
                Task::none()
            }

            // ── Terminal resize ──────────────────────────────────────
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
        }
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
                    true,
                    srv_tx,
                )
                .await;

                match result {
                    connect::ConnectResult::Connected(sender) => {
                        let _ = output.send(Message::Connected(sender)).await;
                        while let Some(msg) = srv_rx.recv().await {
                            // Drain all pending messages into a single batch so iced
                            // processes them in one update/view/draw cycle.
                            let mut batch = vec![msg];
                            while let Ok(msg) = srv_rx.try_recv() {
                                batch.push(msg);
                            }
                            if output.send(Message::ServerMsgBatch(batch)).await.is_err() {
                                break;
                            }
                        }
                    }
                    connect::ConnectResult::Failed(e) => {
                        let _ = output.send(Message::ConnectionFailed(e)).await;
                    }
                }

                std::future::pending::<()>().await;
                unreachable!()
            }),
        );

        Subscription::batch([conn_sub, kbd_sub])
    }

    pub fn theme(&self) -> Theme {
        theme::default()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn handle_server_message(&mut self, msg: ServerMessage) -> Task<Message> {
        match msg {
            ServerMessage::AuthResult {
                success, reason, ..
            } => {
                if !success {
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
                    self.send_ws(ClientMessage::Attach {
                        session: first.name.clone(),
                        last_seqno: None,
                    });
                }
                Task::none()
            }

            ServerMessage::SessionCreated { name, .. } => {
                let size = TermSize::default();
                self.buffers.entry(name.clone()).or_default();
                self.session_list
                    .push(smux_protocol::messages::SessionInfo {
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
                self.send_ws(ClientMessage::Attach {
                    session: name,
                    last_seqno: None,
                });
                Task::none()
            }

            ServerMessage::SessionClosed { name, .. } => {
                self.buffers.remove(&name);
                self.session_list.retain(|s| s.name != name);
                if self.active_session.as_deref() == Some(&name) {
                    self.active_session = self.session_list.first().map(|s| s.name.clone());
                    if let Some(sess) = self.active_session.clone() {
                        self.send_ws(ClientMessage::Attach {
                            session: sess,
                            last_seqno: None,
                        });
                    }
                }
                Task::none()
            }

            // Server-side VT diff: apply snapshot or incremental update
            ServerMessage::TerminalSnapshot {
                session, snapshot, ..
            } => {
                let grid = self.buffers.entry(session).or_default();
                grid.apply_snapshot(snapshot);
                Task::none()
            }

            ServerMessage::TerminalUpdate { session, diff, .. } => {
                if let Some(grid) = self.buffers.get_mut(&session) {
                    grid.apply_diff(diff);
                }
                Task::none()
            }

            // Legacy PtyOutput — keep for backwards compat during transition
            ServerMessage::PtyOutput { .. } => Task::none(),

            ServerMessage::SyncReset { session } => {
                if let Some(grid) = self.buffers.get_mut(&session) {
                    grid.clear();
                }
                Task::none()
            }

            ServerMessage::Event { event } => {
                info!("Server event: {:?}", event);
                Task::none()
            }

            ServerMessage::Lagged {
                session,
                missed_count,
            } => {
                warn!("Lagged on session '{session}': missed {missed_count} diffs, re-attaching");
                self.send_ws(ClientMessage::Attach {
                    session,
                    last_seqno: None,
                });
                Task::none()
            }

            ServerMessage::Error { message, .. } => {
                warn!("Server error: {message}");
                self.status_msg = format!("Error: {message}");
                Task::none()
            }

            _ => Task::none(),
        }
    }

    fn view_connect(&self) -> Element<'_, Message> {
        let form = column![
            text("smux — remote terminal").size(24),
            text("Host"),
            text_input("127.0.0.1", &self.host).on_input(Message::HostChanged),
            text("Port"),
            text_input("8443", &self.port).on_input(Message::PortChanged),
            text("Auth Token"),
            text_input("paste token here", &self.token)
                .on_input(Message::TokenChanged)
                .secure(true),
            button("Connect").on_press(Message::ConnectPressed),
            text(&self.status_msg),
        ]
        .spacing(8)
        .padding(24)
        .max_width(400);

        container(form)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    }

    fn view_terminal(&self) -> Element<'_, Message> {
        let names: Vec<String> = self.session_list.iter().map(|s| s.name.clone()).collect();
        let active_ref = self.active_session.as_deref();

        let bar = session_bar::view(
            &names,
            active_ref,
            Message::SelectSession,
            Message::CreateSessionPressed,
        );

        let terminal_area: Element<Message> = if let Some(name) = &self.active_session {
            if let Some(buf) = self.buffers.get(name) {
                terminal_view::view(buf, name)
            } else {
                text("No output yet").into()
            }
        } else {
            text("No active session — press [+] to create one").into()
        };

        let status = text(&self.status_msg).size(12);
        let disconnect = button("Disconnect").on_press(Message::DisconnectPressed);

        column![bar, terminal_area, row![status, disconnect].spacing(8),]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
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
                _ => return None,
            };
            Some(bytes.to_vec())
        }
        _ => None,
    }
}
