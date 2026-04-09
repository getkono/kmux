use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use kmux_protocol::messages::{
    ClientId, ClientMessage, PaneId, SequenceNo, ServerMessage, SessionEntry, SessionEventMsg,
    TermSize, WordId, epoch_millis,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::connect::{self, ConnectResult};
use crate::grid::CellGrid;
use crate::metrics::RenderMetrics;

/// Per-pane synchronisation state.
#[derive(Default)]
enum PaneSync {
    Synced {
        expected: SequenceNo,
    },
    #[default]
    AwaitingSync,
}

/// High-level events emitted by `handle_server_message` for the UI to react to.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Authentication succeeded.
    AuthOk,
    /// Authentication failed; UI should show connect form.
    AuthFailed { reason: String },
    /// Session list was received.
    SessionListReceived,
    /// A new session was created and is now the active session.
    SessionCreated { word_id: String },
    /// A session was closed.
    SessionClosed { word_id: String },
    /// A session was renamed.
    SessionRenamed { word_id: String, new_name: String },
    /// A new pane was created.
    PaneCreated { pane_id: String },
    /// A pane was closed.
    PaneClosed { pane_id: String },
    /// A structured error from the server.
    ServerError { message: String },
    /// Input lock acquired on a pane.
    InputLockGranted { pane_id: String },
    /// Input lock denied on a pane.
    InputLockDenied { pane_id: String, holder: ClientId },
    /// Input lock released on a pane.
    InputLockReleased { pane_id: String },
}

/// Shared client-side session management logic used by both the TUI and GUI frontends.
pub struct SessionManager {
    // Connection params
    host: String,
    port: u16,
    token: String,
    accept_invalid_certs: bool,

    // Live connection
    ws_sender: Option<mpsc::UnboundedSender<ClientMessage>>,
    pub connected: bool,
    pub status_msg: String,

    // Two-level session state
    pub session_list: Vec<SessionEntry>,
    /// Currently active session (word_id).
    pub active_session: Option<WordId>,
    /// Currently active pane (pane_id = "{word_id}/{pane_index}").
    pub active_pane: Option<PaneId>,
    /// Terminal buffers keyed by pane_id.
    pub buffers: HashMap<PaneId, CellGrid>,
    pane_sync: HashMap<PaneId, PaneSync>,
    pub input_locked: HashMap<PaneId, bool>,
    next_request_id: u64,
    pub client_id: Option<ClientId>,

    // Observability
    pub metrics: RenderMetrics,

    // Last-successful connection info for display / reconnect
    last_host: String,
    last_port: u16,
}

impl SessionManager {
    pub fn new(host: String, port: u16, token: String, accept_invalid_certs: bool) -> Self {
        Self {
            last_host: host.clone(),
            last_port: port,
            host,
            port,
            token,
            accept_invalid_certs,
            ws_sender: None,
            connected: false,
            status_msg: String::new(),
            session_list: Vec::new(),
            active_session: None,
            active_pane: None,
            buffers: HashMap::new(),
            pane_sync: HashMap::new(),
            input_locked: HashMap::new(),
            next_request_id: 0,
            client_id: None,
            metrics: RenderMetrics::new(),
        }
    }

    // ── Connection lifecycle ──────────────────────────────────────────────────

    pub async fn connect(
        &mut self,
        srv_tx: mpsc::UnboundedSender<ServerMessage>,
    ) -> Vec<SessionEvent> {
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
                info!("Connected to kmux-server");

                let rid = self.next_rid();
                self.send_ws(ClientMessage::SessionList { request_id: rid });

                vec![]
            }
            ConnectResult::Failed(e) => {
                self.status_msg = format!("Connection failed: {e}");
                warn!("Connection failed: {e}");
                vec![]
            }
        }
    }

    pub fn set_ws_sender(&mut self, sender: mpsc::UnboundedSender<ClientMessage>) {
        self.ws_sender = Some(sender);
        self.connected = true;
        self.status_msg = format!("Connected to {}:{}", self.host, self.port);
        self.last_host = self.host.clone();
        self.last_port = self.port;
        info!("Connected to kmux-server (external sender)");
    }

    pub fn request_session_list(&mut self) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::SessionList { request_id: rid });
    }

    pub fn disconnect(&mut self) {
        self.ws_sender = None;
        self.connected = false;
        self.buffers.clear();
        self.active_session = None;
        self.active_pane = None;
        self.session_list.clear();
        self.pane_sync.clear();
        self.input_locked.clear();
        self.status_msg = "Disconnected".to_string();
    }

    pub fn mark_connection_lost(&mut self) {
        self.connected = false;
        self.ws_sender = None;
        self.status_msg = "Connection lost".to_string();
    }

    pub fn set_connection_params(&mut self, host: String, port: u16, token: String) {
        self.host = host;
        self.port = port;
        self.token = token;
    }

    // ── Server message handling ───────────────────────────────────────────────

    pub fn handle_server_message(&mut self, msg: ServerMessage) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        match msg {
            ServerMessage::AuthResult {
                success,
                reason,
                client_id,
            } => {
                if success {
                    self.client_id = client_id;
                    events.push(SessionEvent::AuthOk);
                } else {
                    warn!("Auth failed: {:?}", reason);
                    let reason_str = reason.unwrap_or_default();
                    self.status_msg = format!("Auth failed: {reason_str}");
                    self.ws_sender = None;
                    self.connected = false;
                    events.push(SessionEvent::AuthFailed { reason: reason_str });
                }
            }

            ServerMessage::SessionListResult { sessions, .. } => {
                self.session_list = sessions.clone();
                for entry in &sessions {
                    for pane in &entry.panes {
                        self.buffers.entry(pane.pane_id.clone()).or_default();
                    }
                }
                if self.active_session.is_none()
                    && let Some(first_entry) = sessions.first()
                    && let Some(first_pane) = first_entry.panes.first()
                {
                    self.active_session = Some(first_entry.meta.word_id.clone());
                    self.active_pane = Some(first_pane.pane_id.clone());
                    self.attach_fresh(first_pane.pane_id.clone());
                }
                events.push(SessionEvent::SessionListReceived);
            }

            ServerMessage::SessionCreated { entry, .. } => {
                let word_id = entry.meta.word_id.clone();
                for pane in &entry.panes {
                    self.buffers.entry(pane.pane_id.clone()).or_default();
                }
                // Switch to the first pane of the new session
                let first_pane_id = entry.panes.first().map(|p| p.pane_id.clone());
                if let Some(prev_pane) = self.active_pane.take() {
                    self.send_ws(ClientMessage::Detach { pane_id: prev_pane });
                }
                self.active_session = Some(word_id.clone());
                self.active_pane = first_pane_id.clone();
                self.session_list.push(entry);
                self.status_msg = format!("Session '{word_id}' created");
                if let Some(pane_id) = first_pane_id {
                    self.attach_fresh(pane_id);
                }
                events.push(SessionEvent::SessionCreated { word_id });
            }

            ServerMessage::SessionClosed { word_id, .. } => {
                // Remove all pane buffers for this session
                let entry = self
                    .session_list
                    .iter()
                    .find(|e| e.meta.word_id == word_id)
                    .cloned();
                if let Some(entry) = &entry {
                    for pane in &entry.panes {
                        self.buffers.remove(&pane.pane_id);
                        self.pane_sync.remove(&pane.pane_id);
                        self.input_locked.remove(&pane.pane_id);
                    }
                }
                self.session_list.retain(|e| e.meta.word_id != word_id);

                if self.active_session.as_deref() == Some(&word_id) {
                    // Fall back to first remaining session
                    if let Some(next_entry) = self.session_list.first() {
                        let next_word_id = next_entry.meta.word_id.clone();
                        let next_pane_id = next_entry.panes.first().map(|p| p.pane_id.clone());
                        self.active_session = Some(next_word_id);
                        self.active_pane = next_pane_id.clone();
                        if let Some(pane_id) = next_pane_id {
                            self.attach_fresh(pane_id);
                        }
                    } else {
                        self.active_session = None;
                        self.active_pane = None;
                    }
                }
                events.push(SessionEvent::SessionClosed { word_id });
            }

            ServerMessage::PaneCreated {
                pane_id,
                session_word_id,
                ..
            } => {
                self.buffers.entry(pane_id.clone()).or_default();
                // Update the session_list entry
                if let Some(entry) = self
                    .session_list
                    .iter_mut()
                    .find(|e| e.meta.word_id == session_word_id)
                {
                    use kmux_protocol::messages::{PaneInfo, SessionStatus, TermSize};
                    let pane_index = pane_id
                        .rsplit_once('/')
                        .and_then(|(_, idx)| idx.parse().ok())
                        .unwrap_or(0);
                    entry.panes.push(PaneInfo {
                        pane_id: pane_id.clone(),
                        pane_index,
                        program: String::new(),
                        size: TermSize::default(),
                        attached_clients: vec![],
                        status: SessionStatus::Running,
                    });
                }
                self.attach_fresh(pane_id.clone());
                events.push(SessionEvent::PaneCreated { pane_id });
            }

            ServerMessage::PaneClosed { pane_id, .. } => {
                self.buffers.remove(&pane_id);
                self.pane_sync.remove(&pane_id);
                self.input_locked.remove(&pane_id);

                // Remove pane from session_list
                for entry in &mut self.session_list {
                    entry.panes.retain(|p| p.pane_id != pane_id);
                }

                if self.active_pane.as_deref() == Some(&pane_id) {
                    // Try to fall back to another pane in the same session
                    let fallback = self.active_session.as_ref().and_then(|word_id| {
                        self.session_list
                            .iter()
                            .find(|e| e.meta.word_id == *word_id)
                            .and_then(|e| e.panes.first())
                            .map(|p| p.pane_id.clone())
                    });

                    if let Some(pane) = fallback {
                        self.active_pane = Some(pane.clone());
                        self.attach_fresh(pane);
                    } else {
                        // Fall back to first session's first pane
                        let fallback2 = self
                            .session_list
                            .first()
                            .and_then(|e| e.panes.first())
                            .map(|p| (e_word_id_from_list(&self.session_list), p.pane_id.clone()));
                        if let Some((word_id, pane)) = fallback2 {
                            self.active_session = Some(word_id);
                            self.active_pane = Some(pane.clone());
                            self.attach_fresh(pane);
                        } else {
                            self.active_session = None;
                            self.active_pane = None;
                        }
                    }
                }
                events.push(SessionEvent::PaneClosed { pane_id });
            }

            ServerMessage::TerminalSnapshot {
                pane_id,
                snapshot,
                seqno,
                sent_at_ms,
            } => {
                let start = Instant::now();
                let grid = self.buffers.entry(pane_id.clone()).or_default();
                grid.apply_snapshot(snapshot);
                self.pane_sync.insert(
                    pane_id,
                    PaneSync::Synced {
                        expected: SequenceNo(seqno.0 + 1),
                    },
                );
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                self.metrics.record_apply(sent_at_ms, elapsed_ms);
            }

            ServerMessage::TerminalUpdate {
                pane_id,
                diff,
                seqno,
                sent_at_ms,
            } => {
                match self.pane_sync.get(&pane_id) {
                    Some(PaneSync::AwaitingSync) => {
                        self.metrics.record_stale_discard(&pane_id);
                        return events;
                    }
                    Some(PaneSync::Synced { expected }) if seqno != *expected => {
                        self.metrics.record_seqno_gap(&pane_id, expected.0, seqno.0);
                        self.metrics.record_resync(&pane_id, "seqno gap");
                        if let Some(grid) = self.buffers.get_mut(&pane_id) {
                            grid.clear();
                        }
                        self.attach_fresh(pane_id);
                        return events;
                    }
                    _ => {}
                }

                let start = Instant::now();
                let diff = Arc::unwrap_or_clone(diff);
                let op_count = diff.ops.len();
                if let Some(grid) = self.buffers.get_mut(&pane_id) {
                    grid.apply_diff(diff);
                    self.metrics.record_diff_stats(op_count);
                }
                self.pane_sync.insert(
                    pane_id,
                    PaneSync::Synced {
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
                pane_id,
                cursor,
                modes,
                seqno,
                sent_at_ms,
            } => {
                match self.pane_sync.get(&pane_id) {
                    Some(PaneSync::AwaitingSync) => {
                        self.metrics.record_stale_discard(&pane_id);
                        return events;
                    }
                    Some(PaneSync::Synced { expected }) if seqno != *expected => {
                        self.metrics.record_seqno_gap(&pane_id, expected.0, seqno.0);
                        self.metrics.record_resync(&pane_id, "seqno gap");
                        if let Some(grid) = self.buffers.get_mut(&pane_id) {
                            grid.clear();
                        }
                        self.attach_fresh(pane_id);
                        return events;
                    }
                    _ => {}
                }

                let start = Instant::now();
                if let Some(grid) = self.buffers.get_mut(&pane_id) {
                    grid.apply_cursor_update(cursor, modes);
                }
                self.pane_sync.insert(
                    pane_id,
                    PaneSync::Synced {
                        expected: SequenceNo(seqno.0 + 1),
                    },
                );
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                self.metrics.record_apply(sent_at_ms, elapsed_ms);
            }

            ServerMessage::SyncReset { pane_id } => {
                if let Some(grid) = self.buffers.get_mut(&pane_id) {
                    grid.clear();
                }
                self.metrics.record_resync(&pane_id, "server sync reset");
                self.pane_sync.insert(pane_id, PaneSync::AwaitingSync);
            }

            ServerMessage::Event {
                event: SessionEventMsg::SessionRenamed { word_id, new_name },
            } => {
                for entry in &mut self.session_list {
                    if entry.meta.word_id == word_id {
                        entry.meta.name = new_name.clone();
                        break;
                    }
                }
                events.push(SessionEvent::SessionRenamed { word_id, new_name });
            }

            ServerMessage::Event { .. } => {}

            ServerMessage::Lagged {
                pane_id,
                missed_count,
            } => {
                self.metrics.record_lag(&pane_id, missed_count);
                self.metrics.record_resync(&pane_id, "lagged");
                if let Some(grid) = self.buffers.get_mut(&pane_id) {
                    grid.clear();
                }
                self.attach_fresh(pane_id);
            }

            ServerMessage::Error { message, .. } => {
                self.status_msg = format!("Error: {message}");
                events.push(SessionEvent::ServerError { message });
            }

            ServerMessage::SessionRenamed { word_id, new_name } => {
                for entry in &mut self.session_list {
                    if entry.meta.word_id == word_id {
                        entry.meta.name = new_name.clone();
                        break;
                    }
                }
                events.push(SessionEvent::SessionRenamed { word_id, new_name });
            }

            ServerMessage::InputLockGranted { pane_id } => {
                self.input_locked.insert(pane_id.clone(), true);
                self.status_msg = format!("Input lock acquired on '{pane_id}'");
                events.push(SessionEvent::InputLockGranted { pane_id });
            }

            ServerMessage::InputLockDenied { pane_id, holder } => {
                self.status_msg =
                    format!("Input lock denied on '{pane_id}' (held by {:?})", holder);
                events.push(SessionEvent::InputLockDenied { pane_id, holder });
            }

            ServerMessage::InputLockReleased { pane_id } => {
                self.input_locked.insert(pane_id.clone(), false);
                self.status_msg = format!("Input lock released on '{pane_id}'");
                events.push(SessionEvent::InputLockReleased { pane_id });
            }

            _ => {}
        }
        events
    }

    // ── Session operations ────────────────────────────────────────────────────

    /// Switch to a different session by word_id (attaches to its first pane).
    pub fn select_session(&mut self, word_id: String) {
        if let Some(prev_pane) = self.active_pane.take() {
            self.send_ws(ClientMessage::Detach { pane_id: prev_pane });
        }
        let first_pane = self
            .session_list
            .iter()
            .find(|e| e.meta.word_id == word_id)
            .and_then(|e| e.panes.first())
            .map(|p| p.pane_id.clone());
        self.active_session = Some(word_id);
        self.active_pane = first_pane.clone();
        if let Some(pane_id) = first_pane {
            if let Some(buf) = self.buffers.get_mut(&pane_id) {
                buf.clear();
            }
            self.attach_fresh(pane_id);
        }
    }

    /// Switch to a specific pane.
    pub fn select_pane(&mut self, pane_id: String) {
        if let Some(prev_pane) = self.active_pane.take()
            && prev_pane != pane_id
        {
            self.send_ws(ClientMessage::Detach { pane_id: prev_pane });
        }
        if let Some(buf) = self.buffers.get_mut(&pane_id) {
            buf.clear();
        }
        self.active_pane = Some(pane_id.clone());
        self.attach_fresh(pane_id);
    }

    /// Cycle to the next/previous session by offset.
    pub fn cycle_session(&mut self, offset: i32) {
        if self.session_list.is_empty() {
            return;
        }
        let current_idx = self
            .active_session
            .as_ref()
            .and_then(|wid| {
                self.session_list
                    .iter()
                    .position(|e| &e.meta.word_id == wid)
            })
            .unwrap_or(0);
        let len = self.session_list.len() as i32;
        let new_idx = ((current_idx as i32 + offset).rem_euclid(len)) as usize;
        let word_id = self.session_list[new_idx].meta.word_id.clone();
        self.select_session(word_id);
    }

    /// Cycle to the next/previous pane within the active session.
    pub fn cycle_pane(&mut self, offset: i32) {
        let word_id = match &self.active_session {
            Some(w) => w.clone(),
            None => return,
        };
        let panes: Vec<String> = self
            .session_list
            .iter()
            .find(|e| e.meta.word_id == word_id)
            .map(|e| e.panes.iter().map(|p| p.pane_id.clone()).collect())
            .unwrap_or_default();
        if panes.is_empty() {
            return;
        }
        let current_idx = self
            .active_pane
            .as_ref()
            .and_then(|pid| panes.iter().position(|p| p == pid))
            .unwrap_or(0);
        let len = panes.len() as i32;
        let new_idx = ((current_idx as i32 + offset).rem_euclid(len)) as usize;
        self.select_pane(panes[new_idx].clone());
    }

    /// Create a new session. The server assigns the word_id and CWD defaults to
    /// the client's current working directory.
    pub fn create_session(&mut self) {
        if self.ws_sender.is_some() {
            let rid = self.next_rid();
            let cwd = std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(|s| s.to_string()));
            self.send_ws(ClientMessage::SessionCreate {
                request_id: rid,
                name: None,
                cwd,
                program: None,
                args: vec![],
                size: TermSize { rows: 24, cols: 80 },
            });
        }
    }

    /// Create a new pane in the active session.
    pub fn create_pane(&mut self) {
        if let Some(word_id) = self.active_session.clone() {
            let rid = self.next_rid();
            self.send_ws(ClientMessage::PaneCreate {
                request_id: rid,
                word_id,
                program: None,
                args: vec![],
                size: TermSize { rows: 24, cols: 80 },
            });
        }
    }

    /// Close the active pane.
    pub fn close_pane(&mut self) {
        if let Some(pane_id) = self.active_pane.clone() {
            let rid = self.next_rid();
            self.send_ws(ClientMessage::PaneClose {
                request_id: rid,
                pane_id,
            });
        }
    }

    /// Close the entire active session (all its panes).
    pub fn close_session(&mut self, word_id: &str) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::SessionClose {
            request_id: rid,
            word_id: word_id.to_string(),
        });
    }

    /// Rename the active session's display name.
    pub fn rename_session(&mut self, word_id: &str, new_name: &str) {
        if !new_name.is_empty() {
            let rid = self.next_rid();
            self.send_ws(ClientMessage::SessionRename {
                request_id: rid,
                word_id: word_id.to_string(),
                new_name: new_name.to_string(),
            });
        }
    }

    /// Send raw PTY input bytes for the active pane.
    pub fn send_input(&mut self, data: Vec<u8>) -> bool {
        if let Some(pane_id) = self.active_pane.clone() {
            let locked = self.input_locked.get(&pane_id).copied().unwrap_or(false);
            if locked {
                self.status_msg = "Input locked on this pane".to_string();
                return false;
            }
            self.send_ws(ClientMessage::PtyInput { pane_id, data });
        }
        true
    }

    /// Send a paste string for the active pane.
    pub fn send_paste(&mut self, text: String) -> bool {
        if text.is_empty() {
            return true;
        }
        if let Some(pane_id) = self.active_pane.clone() {
            let locked = self.input_locked.get(&pane_id).copied().unwrap_or(false);
            if locked {
                self.status_msg = "Input locked on this pane".to_string();
                return false;
            }
            self.send_ws(ClientMessage::PtyPaste {
                pane_id,
                data: text,
            });
        }
        true
    }

    /// Send a resize event for the given pane and resize the local buffer.
    pub fn send_resize(&mut self, pane_id: &str, rows: u16, cols: u16) {
        if let Some(buf) = self.buffers.get_mut(pane_id) {
            buf.resize(rows, cols);
        }
        self.send_ws(ClientMessage::Resize {
            pane_id: pane_id.to_string(),
            size: TermSize { rows, cols },
        });
    }

    /// Send a Unix signal to the PTY child of the active pane.
    pub fn send_signal(&mut self, pane_id: &str, signal: i32) {
        self.send_ws(ClientMessage::Signal {
            pane_id: pane_id.to_string(),
            signal,
        });
    }

    /// Toggle the input lock on the active pane.
    pub fn toggle_input_lock(&mut self) {
        if let Some(pane_id) = self.active_pane.clone() {
            let locked = self.input_locked.get(&pane_id).copied().unwrap_or(false);
            if locked {
                self.send_ws(ClientMessage::ReleaseInputLock { pane_id });
            } else {
                self.send_ws(ClientMessage::RequestInputLock { pane_id });
            }
        }
    }

    /// Enable or disable full-snapshot mode on the server.
    pub fn set_snapshot_mode(&mut self, enabled: bool) {
        self.send_ws(ClientMessage::SetSnapshotMode { enabled });
    }

    // ── Getters ───────────────────────────────────────────────────────────────

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Active session word_id.
    pub fn active_session(&self) -> Option<&str> {
        self.active_session.as_deref()
    }

    /// Active pane_id.
    pub fn active_pane_id(&self) -> Option<&str> {
        self.active_pane.as_deref()
    }

    pub fn session_list(&self) -> &[SessionEntry] {
        &self.session_list
    }

    pub fn buffer(&self, pane_id: &str) -> Option<&CellGrid> {
        self.buffers.get(pane_id)
    }

    pub fn buffer_mut(&mut self, pane_id: &str) -> Option<&mut CellGrid> {
        self.buffers.get_mut(pane_id)
    }

    pub fn active_grid(&self) -> Option<&CellGrid> {
        self.active_pane.as_ref().and_then(|p| self.buffers.get(p))
    }

    pub fn active_grid_mut(&mut self) -> Option<&mut CellGrid> {
        if let Some(pane_id) = &self.active_pane {
            let pane_id = pane_id.clone();
            self.buffers.get_mut(&pane_id)
        } else {
            None
        }
    }

    pub fn status_msg(&self) -> &str {
        &self.status_msg
    }

    pub fn set_status_msg(&mut self, msg: String) {
        self.status_msg = msg;
    }

    pub fn host_port_display(&self) -> String {
        if self.connected {
            format!("{}:{}", self.host, self.port)
        } else if !self.last_host.is_empty() {
            format!("{}:{}", self.last_host, self.last_port)
        } else {
            String::new()
        }
    }

    pub fn active_term_size(&self) -> Option<(u16, u16)> {
        self.active_grid().map(|b| (b.rows as u16, b.cols as u16))
    }

    pub fn is_input_locked(&self, pane_id: &str) -> bool {
        self.input_locked.get(pane_id).copied().unwrap_or(false)
    }

    pub fn active_input_locked(&self) -> bool {
        self.active_pane
            .as_ref()
            .map(|p| self.is_input_locked(p))
            .unwrap_or(false)
    }

    pub fn client_id(&self) -> Option<ClientId> {
        self.client_id
    }

    /// Compute the display name for a session, disambiguating by parent directory
    /// if multiple sessions share the same name.
    pub fn display_name_for(&self, word_id: &str) -> String {
        let Some(entry) = self.session_list.iter().find(|e| e.meta.word_id == word_id) else {
            return word_id.to_string();
        };
        let name = &entry.meta.name;
        let cwd = &entry.meta.cwd;

        // Count how many sessions share the same display name
        let same_name_count = self
            .session_list
            .iter()
            .filter(|e| &e.meta.name == name)
            .count();

        if same_name_count <= 1 {
            name.clone()
        } else {
            // Show the parent directory to disambiguate
            let parent = Path::new(cwd)
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or(cwd.as_str());
            format!("{name} ({parent})")
        }
    }

    /// Panes for the active session, in index order.
    pub fn active_session_panes(&self) -> &[kmux_protocol::messages::PaneInfo] {
        self.active_session
            .as_ref()
            .and_then(|wid| self.session_list.iter().find(|e| e.meta.word_id == *wid))
            .map(|e| e.panes.as_slice())
            .unwrap_or(&[])
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn send_ws(&self, msg: ClientMessage) {
        if let Some(tx) = &self.ws_sender
            && let Err(e) = tx.send(msg)
        {
            warn!("send_ws failed: {e}");
        }
    }

    fn next_rid(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    fn attach_fresh(&mut self, pane_id: String) {
        self.pane_sync
            .insert(pane_id.clone(), PaneSync::AwaitingSync);
        self.send_ws(ClientMessage::Attach {
            pane_id,
            last_seqno: None,
        });
    }
}

/// Get the word_id of the first session in the list.
fn e_word_id_from_list(list: &[SessionEntry]) -> String {
    list.first()
        .map(|e| e.meta.word_id.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::{
        GridSnapshot, PaneInfo, SessionMeta, SessionStatus, TermModes, TermSize,
    };

    fn make_manager() -> SessionManager {
        SessionManager::new(
            "127.0.0.1".to_string(),
            8443,
            "test-token".to_string(),
            false,
        )
    }

    fn make_connected_manager() -> (SessionManager, mpsc::UnboundedReceiver<ClientMessage>) {
        let mut mgr = make_manager();
        let (tx, rx) = mpsc::unbounded_channel();
        mgr.ws_sender = Some(tx);
        mgr.connected = true;
        (mgr, rx)
    }

    fn make_entry(word_id: &str, cwd: &str) -> SessionEntry {
        use kmux_protocol::messages::SessionMeta;
        SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: word_id.to_string(),
                name: std::path::Path::new(cwd)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(word_id)
                    .to_string(),
                cwd: cwd.to_string(),
            },
            panes: vec![PaneInfo {
                pane_id: format!("{word_id}/0"),
                pane_index: 0,
                program: String::new(),
                size: TermSize::default(),
                attached_clients: vec![],
                status: SessionStatus::Running,
            }],
        }
    }

    #[test]
    fn auth_ok_sets_client_id() {
        let mut mgr = make_manager();
        let events = mgr.handle_server_message(ServerMessage::AuthResult {
            success: true,
            reason: None,
            client_id: Some(ClientId(42)),
        });
        assert!(matches!(events.as_slice(), [SessionEvent::AuthOk]));
        assert_eq!(mgr.client_id, Some(ClientId(42)));
    }

    #[test]
    fn auth_failed_emits_event_and_clears_connection() {
        let mut mgr = make_manager();
        mgr.connected = true;
        let (tx, _rx) = mpsc::unbounded_channel::<ClientMessage>();
        mgr.ws_sender = Some(tx);

        let events = mgr.handle_server_message(ServerMessage::AuthResult {
            success: false,
            reason: Some("bad token".to_string()),
            client_id: None,
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::AuthFailed { reason }] if reason == "bad token"
        ));
        assert!(!mgr.connected);
        assert!(mgr.ws_sender.is_none());
    }

    #[test]
    fn session_list_populates_and_auto_attaches() {
        let (mut mgr, mut rx) = make_connected_manager();

        let sessions = vec![make_entry("eagle", "/home/user/proj")];
        let events = mgr.handle_server_message(ServerMessage::SessionListResult {
            request_id: 0,
            sessions,
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::SessionListReceived]
        ));
        assert_eq!(mgr.session_list.len(), 1);
        assert_eq!(mgr.active_session.as_deref(), Some("eagle"));
        assert_eq!(mgr.active_pane.as_deref(), Some("eagle/0"));
        // Attach message should have been sent
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn session_created_switches_active() {
        let (mut mgr, _rx) = make_connected_manager();
        let entry = make_entry("falcon", "/home/user/other");
        let events = mgr.handle_server_message(ServerMessage::SessionCreated {
            request_id: 0,
            entry,
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::SessionCreated { word_id }] if word_id == "falcon"
        ));
        assert_eq!(mgr.active_session.as_deref(), Some("falcon"));
        assert_eq!(mgr.active_pane.as_deref(), Some("falcon/0"));
        assert!(mgr.buffers.contains_key("falcon/0"));
    }

    #[test]
    fn session_closed_removes_and_falls_back() {
        let (mut mgr, _rx) = make_connected_manager();

        let e1 = make_entry("s1", "/a");
        let e2 = make_entry("s2", "/b");
        mgr.session_list.push(e1);
        mgr.session_list.push(e2);
        mgr.buffers.insert("s1/0".to_string(), CellGrid::default());
        mgr.buffers.insert("s2/0".to_string(), CellGrid::default());
        mgr.active_session = Some("s1".to_string());
        mgr.active_pane = Some("s1/0".to_string());

        let events = mgr.handle_server_message(ServerMessage::SessionClosed {
            request_id: 0,
            word_id: "s1".to_string(),
            exit_code: None,
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::SessionClosed { word_id }] if word_id == "s1"
        ));
        assert!(!mgr.buffers.contains_key("s1/0"));
        assert_eq!(mgr.active_session.as_deref(), Some("s2"));
    }

    #[test]
    fn terminal_snapshot_transitions_to_synced() {
        let (mut mgr, _rx) = make_connected_manager();
        mgr.buffers
            .insert("eagle/0".to_string(), CellGrid::default());
        mgr.pane_sync
            .insert("eagle/0".to_string(), PaneSync::AwaitingSync);

        let snapshot = GridSnapshot {
            rows: 24,
            cols: 80,
            cells: vec![],
            cursor: Default::default(),
            modes: TermModes::EMPTY,
        };
        mgr.handle_server_message(ServerMessage::TerminalSnapshot {
            pane_id: "eagle/0".to_string(),
            snapshot,
            seqno: SequenceNo(5),
            sent_at_ms: 0,
        });

        assert!(matches!(
            mgr.pane_sync.get("eagle/0"),
            Some(PaneSync::Synced {
                expected: SequenceNo(6)
            })
        ));
    }

    #[test]
    fn terminal_update_discarded_when_awaiting_sync() {
        use kmux_protocol::messages::TerminalDiff;
        let (mut mgr, _rx) = make_connected_manager();
        mgr.buffers
            .insert("eagle/0".to_string(), CellGrid::default());
        mgr.pane_sync
            .insert("eagle/0".to_string(), PaneSync::AwaitingSync);

        let diff = Arc::new(TerminalDiff {
            ops: vec![],
            cursor: Default::default(),
            modes: TermModes::EMPTY,
            scrollback_lines: vec![],
        });
        mgr.handle_server_message(ServerMessage::TerminalUpdate {
            pane_id: "eagle/0".to_string(),
            diff,
            seqno: SequenceNo(0),
            sent_at_ms: 0,
        });

        assert_eq!(mgr.metrics.snapshot(false).counters.stale_discards, 1);
    }

    #[test]
    fn cycle_session_wraps_around() {
        let (mut mgr, _rx) = make_connected_manager();
        for (wid, cwd) in [("a", "/a"), ("b", "/b"), ("c", "/c")] {
            let entry = make_entry(wid, cwd);
            mgr.buffers.insert(format!("{wid}/0"), CellGrid::default());
            mgr.session_list.push(entry);
        }
        mgr.active_session = Some("c".to_string());
        mgr.active_pane = Some("c/0".to_string());
        mgr.cycle_session(1);
        assert_eq!(mgr.active_session.as_deref(), Some("a")); // wraps from c to a
    }

    #[test]
    fn display_name_disambiguation() {
        let mut mgr = make_manager();
        // Two sessions with the same basename "src" but different parent dirs
        mgr.session_list.push(SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: "alpha".to_string(),
                name: "src".to_string(),
                cwd: "/proj-a/src".to_string(),
            },
            panes: vec![],
        });
        mgr.session_list.push(SessionEntry {
            meta: SessionMeta {
                index: 1,
                word_id: "beta".to_string(),
                name: "src".to_string(),
                cwd: "/proj-b/src".to_string(),
            },
            panes: vec![],
        });

        assert_eq!(mgr.display_name_for("alpha"), "src (proj-a)");
        assert_eq!(mgr.display_name_for("beta"), "src (proj-b)");
    }

    #[test]
    fn display_name_no_disambiguation_when_unique() {
        let mut mgr = make_manager();
        mgr.session_list.push(SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: "eagle".to_string(),
                name: "myapp".to_string(),
                cwd: "/home/user/myapp".to_string(),
            },
            panes: vec![],
        });
        assert_eq!(mgr.display_name_for("eagle"), "myapp");
    }
}
