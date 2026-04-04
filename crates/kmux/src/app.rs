use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyEvent, MouseEvent, MouseEventKind};
use futures::StreamExt;
use kmux_client::connect::{self, ConnectResult};
use kmux_client::grid::CellGrid;
use kmux_client::input::{encode_mouse_scroll, key_to_bytes};
use kmux_client::metrics::RenderMetrics;
use kmux_protocol::messages::{
    ClientId, ClientMessage, SequenceNo, ServerMessage, SessionEventMsg, SessionInfo,
    SessionStatus, TermSize, epoch_millis,
};
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::key_convert;
use crate::mode::{self, Action, ConnectField, Mode};
use crate::ui;

/// Per-session synchronisation state.
#[derive(Default)]
enum SessionSync {
    Synced {
        expected: SequenceNo,
    },
    #[default]
    AwaitingSync,
}

pub struct App {
    // Connection
    host: String,
    port: u16,
    token: String,
    accept_invalid_certs: bool,
    ws_sender: Option<mpsc::UnboundedSender<ClientMessage>>,
    pub connected: bool,
    pub status_msg: String,

    // Sessions
    pub buffers: HashMap<String, CellGrid>,
    pub active_session: Option<String>,
    pub session_list: Vec<SessionInfo>,
    session_sync: HashMap<String, SessionSync>,
    next_request_id: u64,

    // UI mode
    pub mode: Mode,
    pub hud_visible: bool,
    pub force_snapshot_mode: bool,
    pub input_locked: HashMap<String, bool>,
    pub client_id: Option<ClientId>,
    pub metrics: RenderMetrics,

    // Connect form state
    pub connect_host: String,
    pub connect_port: String,
    pub connect_token: String,

    // Reconnection
    last_host: String,
    last_port: u16,
    last_token: String,
    disconnect_at: Option<Instant>,

    // Dirty flag for rendering
    needs_render: bool,
}

impl App {
    pub fn new(host: String, port: u16, token: String, accept_invalid_certs: bool) -> Self {
        let connect_host = host.clone();
        let connect_port = port.to_string();
        let connect_token = token.clone();

        Self {
            host: host.clone(),
            port,
            token: token.clone(),
            accept_invalid_certs,
            ws_sender: None,
            connected: false,
            status_msg: String::new(),
            buffers: HashMap::new(),
            active_session: None,
            session_list: Vec::new(),
            session_sync: HashMap::new(),
            next_request_id: 0,
            mode: if token.is_empty() {
                Mode::Connect {
                    field: ConnectField::Host,
                }
            } else {
                Mode::Normal
            },
            hud_visible: false,
            force_snapshot_mode: false,
            input_locked: HashMap::new(),
            client_id: None,
            metrics: RenderMetrics::new(),
            connect_host,
            connect_port,
            connect_token,
            last_host: host,
            last_port: port,
            last_token: token,
            disconnect_at: None,
            needs_render: true,
        }
    }

    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> anyhow::Result<()> {
        let mut event_stream = EventStream::new();
        let (srv_tx, mut srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let mut reconnect_timer: Option<tokio::time::Instant> = None;

        // Auto-connect if token is available
        if !self.token.is_empty() {
            self.status_msg = "Connecting...".to_string();
            self.start_connection(srv_tx.clone()).await;
        }

        let render_interval = Duration::from_millis(33); // ~30 FPS
        let mut render_tick = tokio::time::interval(render_interval);
        render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Render if needed
            if self.needs_render {
                terminal.draw(|f| ui::render(f, self))?;
                self.needs_render = false;
            }

            tokio::select! {
                event = event_stream.next() => {
                    match event {
                        Some(Ok(Event::Key(key_event))) => {
                            if self.handle_key(key_event, &srv_tx).await {
                                return Ok(());
                            }
                            self.needs_render = true;
                        }
                        Some(Ok(Event::Mouse(mouse_event))) => {
                            self.handle_mouse(mouse_event);
                            self.needs_render = true;
                        }
                        Some(Ok(Event::Resize(cols, rows))) => {
                            self.handle_resize(rows, cols);
                            self.needs_render = true;
                        }
                        Some(Err(_)) | None => break,
                        _ => {}
                    }
                }
                msg = srv_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            // Drain all available messages
                            let mut batch = vec![msg];
                            while let Ok(m) = srv_rx.try_recv() {
                                batch.push(m);
                            }
                            self.metrics.record_batch(batch.len());
                            for m in batch {
                                self.handle_server_message(m);
                            }
                            self.needs_render = true;
                        }
                        None => {
                            // Channel closed = disconnected
                            if self.connected {
                                self.connected = false;
                                self.ws_sender = None;
                                self.status_msg = "Connection lost".to_string();
                                self.disconnect_at = Some(Instant::now());
                                reconnect_timer = Some(tokio::time::Instant::now() + Duration::from_secs(3));
                                self.needs_render = true;
                            }
                        }
                    }
                }
                _ = async {
                    if let Some(when) = reconnect_timer {
                        tokio::time::sleep_until(when).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    reconnect_timer = None;
                    self.status_msg = "Reconnecting...".to_string();
                    // Create new channel for reconnection
                    let (new_tx, new_rx) = mpsc::unbounded_channel();
                    srv_rx = new_rx;
                    self.start_connection(new_tx).await;
                    self.needs_render = true;
                }
                _ = render_tick.tick() => {
                    // Periodic render for animations (cursor blink, HUD updates)
                    self.needs_render = true;
                }
            }
        }

        Ok(())
    }

    async fn start_connection(&mut self, srv_tx: mpsc::UnboundedSender<ServerMessage>) {
        let host = self.host.clone();
        let port = self.port;
        let token = self.token.clone();
        let accept_invalid = self.accept_invalid_certs;

        match connect::connect(host, port, token, accept_invalid, srv_tx).await {
            ConnectResult::Connected(sender) => {
                self.ws_sender = Some(sender);
                self.connected = true;
                self.status_msg = format!("Connected to {}:{}", self.host, self.port);
                self.last_host = self.host.clone();
                self.last_port = self.port;
                self.last_token = self.token.clone();
                info!("Connected to kmux-server");

                // Request session list
                let rid = self.next_rid();
                self.send_ws(ClientMessage::SessionList { request_id: rid });

                // Switch to normal mode
                if matches!(self.mode, Mode::Connect { .. }) {
                    self.mode = Mode::Normal;
                }
            }
            ConnectResult::Failed(e) => {
                self.status_msg = format!("Connection failed: {e}");
                warn!("Connection failed: {e}");
            }
        }
    }

    fn next_rid(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    fn send_ws(&self, msg: ClientMessage) {
        if let Some(tx) = &self.ws_sender
            && let Err(e) = tx.send(msg)
        {
            warn!("send_ws failed: {e}");
        }
    }

    pub fn host_port_display(&self) -> String {
        if self.connected {
            format!("{}:{}", self.host, self.port)
        } else {
            String::new()
        }
    }

    pub fn active_term_size(&self) -> Option<(u16, u16)> {
        self.active_session
            .as_ref()
            .and_then(|s| self.buffers.get(s))
            .map(|b| (b.rows as u16, b.cols as u16))
    }

    /// Handle a key event. Returns true if the app should exit.
    async fn handle_key(
        &mut self,
        key_event: KeyEvent,
        _srv_tx: &mpsc::UnboundedSender<ServerMessage>,
    ) -> bool {
        let (key, mods) = key_convert::convert(&key_event);
        let (new_mode, action) = mode::resolve(&self.mode, &key, mods);

        if let Some(m) = new_mode {
            self.mode = m;
        }

        match action {
            Action::ForwardKey => {
                // Snap to bottom on keypress
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
                {
                    grid.scroll_to_bottom();
                }

                let app_cursor = self
                    .active_session
                    .as_ref()
                    .and_then(|s| self.buffers.get(s))
                    .map(|b| b.app_cursor())
                    .unwrap_or(false);

                let text = key_convert::text_from_event(&key_event);
                let bytes = key_to_bytes(&key, mods, text.as_deref(), app_cursor);
                if let Some(bytes) = bytes
                    && let Some(session) = &self.active_session
                {
                    let locked = self.input_locked.get(session).copied().unwrap_or(false);
                    if locked {
                        self.status_msg = "Input locked on this session".to_string();
                    } else {
                        self.send_ws(ClientMessage::PtyInput {
                            session: session.clone(),
                            data: bytes,
                        });
                    }
                }
            }
            Action::CreateSession => {
                if self.ws_sender.is_some() {
                    let rid = self.next_rid();
                    let name = format!("session-{rid}");
                    self.send_ws(ClientMessage::SessionCreate {
                        request_id: rid,
                        name,
                        program: None,
                        args: vec![],
                        size: TermSize { rows: 24, cols: 80 },
                    });
                }
            }
            Action::CloseSession => {
                if let Some(session) = self.active_session.clone() {
                    self.mode = Mode::ConfirmClose { session };
                }
            }
            Action::ConfirmCloseYes => {
                if let Mode::ConfirmClose { session } =
                    std::mem::replace(&mut self.mode, Mode::Normal)
                {
                    let rid = self.next_rid();
                    self.send_ws(ClientMessage::SessionClose {
                        request_id: rid,
                        name: session,
                    });
                }
            }
            Action::NextSession => self.cycle_session(1),
            Action::PrevSession => self.cycle_session(-1),
            Action::JumpToSession(idx) => {
                if idx < self.session_list.len() {
                    let name = self.session_list[idx].name.clone();
                    self.select_session(name);
                }
            }
            Action::RenameSession => {
                if let Some(session) = self.active_session.clone() {
                    self.mode = Mode::Rename {
                        buffer: session.clone(),
                        session,
                    };
                }
            }
            Action::RenameChar(ch) => {
                if let Mode::Rename { buffer, .. } = &mut self.mode {
                    buffer.push(ch);
                }
            }
            Action::RenameBackspace => {
                if let Mode::Rename { buffer, .. } = &mut self.mode {
                    buffer.pop();
                }
            }
            Action::RenameSubmit => {
                if let Mode::Rename { buffer, session } =
                    std::mem::replace(&mut self.mode, Mode::Normal)
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
            }
            Action::Disconnect => {
                self.ws_sender = None;
                self.connected = false;
                self.buffers.clear();
                self.active_session = None;
                self.session_list.clear();
                self.session_sync.clear();
                self.input_locked.clear();
                self.mode = Mode::Connect {
                    field: ConnectField::Host,
                };
                self.status_msg = "Disconnected".to_string();
            }
            Action::SendSignal(signal) => {
                if let Some(session) = &self.active_session {
                    self.send_ws(ClientMessage::Signal {
                        session: session.clone(),
                        signal,
                    });
                }
            }
            Action::ScrollUp(n) => {
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
                {
                    grid.scroll_up(n);
                }
            }
            Action::ScrollDown(n) => {
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
                {
                    grid.scroll_down(n);
                }
            }
            Action::ScrollPageUp => {
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
                {
                    let rows = grid.rows;
                    grid.scroll_up(rows);
                }
            }
            Action::ScrollPageDown => {
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get_mut(name)
                {
                    let rows = grid.rows;
                    grid.scroll_down(rows);
                }
            }
            Action::ToggleHud => {
                self.hud_visible = !self.hud_visible;
            }
            Action::ToggleSnapshotMode => {
                self.force_snapshot_mode = !self.force_snapshot_mode;
                self.send_ws(ClientMessage::SetSnapshotMode {
                    enabled: self.force_snapshot_mode,
                });
            }
            Action::ToggleInputLock => {
                if let Some(session) = self.active_session.clone() {
                    let locked = self.input_locked.get(&session).copied().unwrap_or(false);
                    if locked {
                        self.send_ws(ClientMessage::ReleaseInputLock { session });
                    } else {
                        self.send_ws(ClientMessage::RequestInputLock { session });
                    }
                }
            }
            Action::CopySelection => {
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get(name)
                    && let Some(text) = grid.selected_text()
                {
                    let _ = cli_clipboard::set_contents(text);
                }
            }
            Action::Paste => {
                if let Ok(text) = cli_clipboard::get_contents()
                    && !text.is_empty()
                    && let Some(session) = &self.active_session
                {
                    let locked = self.input_locked.get(session).copied().unwrap_or(false);
                    if !locked {
                        self.send_ws(ClientMessage::PtyPaste {
                            session: session.clone(),
                            data: text,
                        });
                    }
                }
            }
            Action::ConnectSubmit => {
                self.host = self.connect_host.clone();
                self.port = self.connect_port.parse().unwrap_or(8443);
                self.token = self.connect_token.clone();
                self.status_msg = "Connecting...".to_string();
                let (new_tx, _new_rx) = mpsc::unbounded_channel();
                // We can't easily replace srv_rx here; instead we connect directly
                self.start_connection(new_tx).await;
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
            Action::None => {}
        }

        false
    }

    fn handle_mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::ScrollUp => {
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get(name)
                {
                    if grid.modes().mouse_report() {
                        let col = event.column + 1;
                        let row = event.row + 1; // Adjust for session bar
                        let sgr = grid.modes().sgr_mouse();
                        let bytes = encode_mouse_scroll(col, row, 3, sgr);
                        if !bytes.is_empty() {
                            let locked = self.input_locked.get(name).copied().unwrap_or(false);
                            if !locked {
                                self.send_ws(ClientMessage::PtyInput {
                                    session: name.clone(),
                                    data: bytes,
                                });
                            }
                        }
                    } else if let Some(grid) = self.buffers.get_mut(name) {
                        grid.scroll_up(3);
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(name) = &self.active_session
                    && let Some(grid) = self.buffers.get(name)
                {
                    if grid.modes().mouse_report() {
                        let col = event.column + 1;
                        let row = event.row + 1;
                        let sgr = grid.modes().sgr_mouse();
                        let bytes = encode_mouse_scroll(col, row, -3, sgr);
                        if !bytes.is_empty() {
                            let locked = self.input_locked.get(name).copied().unwrap_or(false);
                            if !locked {
                                self.send_ws(ClientMessage::PtyInput {
                                    session: name.clone(),
                                    data: bytes,
                                });
                            }
                        }
                    } else if let Some(grid) = self.buffers.get_mut(name) {
                        grid.scroll_down(3);
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_resize(&mut self, rows: u16, cols: u16) {
        // Account for session bar (1 row) + status bar (1 row) + hint bar (1 row)
        let term_rows = rows.saturating_sub(3);
        let term_cols = cols;

        if let Some(name) = &self.active_session {
            if let Some(buf) = self.buffers.get_mut(name) {
                buf.resize(term_rows, term_cols);
            }
            self.send_ws(ClientMessage::Resize {
                session: name.clone(),
                size: TermSize {
                    rows: term_rows,
                    cols: term_cols,
                },
            });
        }
    }

    fn select_session(&mut self, name: String) {
        if let Some(prev) = self.active_session.take() {
            self.send_ws(ClientMessage::Detach { session: prev });
        }
        if let Some(buf) = self.buffers.get_mut(&name) {
            buf.clear();
        }
        self.active_session = Some(name.clone());
        self.attach_fresh(name);
    }

    fn cycle_session(&mut self, offset: i32) {
        if self.session_list.is_empty() {
            return;
        }
        let current_idx = self
            .active_session
            .as_ref()
            .and_then(|name| self.session_list.iter().position(|s| &s.name == name))
            .unwrap_or(0);
        let len = self.session_list.len() as i32;
        let new_idx = ((current_idx as i32 + offset).rem_euclid(len)) as usize;
        let name = self.session_list[new_idx].name.clone();
        self.select_session(name);
    }

    fn attach_fresh(&mut self, session: String) {
        self.session_sync
            .insert(session.clone(), SessionSync::AwaitingSync);
        self.send_ws(ClientMessage::Attach {
            session,
            last_seqno: None,
        });
    }

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

    fn handle_server_message(&mut self, msg: ServerMessage) {
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
                    self.ws_sender = None;
                    self.connected = false;
                    self.mode = Mode::Connect {
                        field: ConnectField::Host,
                    };
                }
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
            }
            ServerMessage::SessionCreated { name, .. } => {
                let size = TermSize::default();
                self.buffers.entry(name.clone()).or_default();
                self.session_list.push(SessionInfo {
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
            }
            ServerMessage::TerminalUpdate {
                session,
                diff,
                seqno,
                sent_at_ms,
            } => {
                match self.session_sync.get(&session) {
                    Some(SessionSync::AwaitingSync) => {
                        self.metrics.record_stale_discard(&session);
                        return;
                    }
                    Some(SessionSync::Synced { expected }) if seqno != *expected => {
                        self.metrics.record_seqno_gap(&session, expected.0, seqno.0);
                        self.metrics.record_resync(&session, "seqno gap");
                        if let Some(grid) = self.buffers.get_mut(&session) {
                            grid.clear();
                        }
                        self.attach_fresh(session);
                        return;
                    }
                    _ => {}
                }

                let start = Instant::now();
                let diff = Arc::unwrap_or_clone(diff);
                let op_count = diff.ops.len();
                if let Some(grid) = self.buffers.get_mut(&session) {
                    grid.apply_diff(diff);
                    self.metrics.record_diff_stats(op_count);
                }
                self.session_sync.insert(
                    session,
                    SessionSync::Synced {
                        expected: SequenceNo(seqno.0 + 1),
                    },
                );
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                self.metrics.record_apply(sent_at_ms, elapsed_ms);
                if op_count > 100 {
                    let net_apply_ms = epoch_millis().saturating_sub(sent_at_ms) as f64;
                    self.metrics.record_large_diff(net_apply_ms);
                }
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
                        self.metrics.record_stale_discard(&session);
                        return;
                    }
                    Some(SessionSync::Synced { expected }) if seqno != *expected => {
                        self.metrics.record_seqno_gap(&session, expected.0, seqno.0);
                        self.metrics.record_resync(&session, "seqno gap");
                        if let Some(grid) = self.buffers.get_mut(&session) {
                            grid.clear();
                        }
                        self.attach_fresh(session);
                        return;
                    }
                    _ => {}
                }

                if let Some(grid) = self.buffers.get_mut(&session) {
                    grid.apply_cursor_update(cursor, modes);
                }
                self.session_sync.insert(
                    session,
                    SessionSync::Synced {
                        expected: SequenceNo(seqno.0 + 1),
                    },
                );
                let start = Instant::now();
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                self.metrics.record_apply(sent_at_ms, elapsed_ms);
            }
            #[allow(deprecated)]
            ServerMessage::PtyOutput { .. } => {}
            ServerMessage::SyncReset { session } => {
                if let Some(grid) = self.buffers.get_mut(&session) {
                    grid.clear();
                }
                self.metrics.record_resync(&session, "server sync reset");
                self.session_sync.insert(session, SessionSync::AwaitingSync);
            }
            ServerMessage::Event {
                event: SessionEventMsg::Renamed { old_name, new_name },
            } => {
                self.apply_rename(&old_name, &new_name);
            }
            ServerMessage::Event { .. } => {}
            ServerMessage::Lagged {
                session,
                missed_count,
            } => {
                self.metrics.record_lag(&session, missed_count);
                self.metrics.record_resync(&session, "lagged");
                if let Some(grid) = self.buffers.get_mut(&session) {
                    grid.clear();
                }
                self.attach_fresh(session);
            }
            ServerMessage::Error { message, .. } => {
                self.status_msg = format!("Error: {message}");
            }
            ServerMessage::SessionRenamed { old_name, new_name } => {
                self.apply_rename(&old_name, &new_name);
            }
            ServerMessage::InputLockGranted { session } => {
                self.input_locked.insert(session.clone(), true);
                self.status_msg = format!("Input lock acquired on '{session}'");
            }
            ServerMessage::InputLockDenied { session, holder } => {
                self.status_msg =
                    format!("Input lock denied on '{session}' (held by {:?})", holder);
            }
            ServerMessage::InputLockReleased { session } => {
                self.input_locked.insert(session.clone(), false);
                self.status_msg = format!("Input lock released on '{session}'");
            }
            _ => {}
        }
    }
}
