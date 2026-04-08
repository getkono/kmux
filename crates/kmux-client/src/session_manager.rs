use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use kmux_protocol::messages::{
    ClientId, ClientMessage, SequenceNo, ServerMessage, SessionEventMsg, SessionInfo,
    SessionStatus, TermSize, epoch_millis,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::connect::{self, ConnectResult};
use crate::grid::CellGrid;
use crate::metrics::RenderMetrics;

/// Per-session synchronisation state.
#[derive(Default)]
enum SessionSync {
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
    SessionCreated { name: String },
    /// A session was closed. If it was active, the manager has switched to another (or None).
    SessionClosed { name: String },
    /// A session was renamed.
    SessionRenamed { old_name: String, new_name: String },
    /// A structured error from the server.
    ServerError { message: String },
    /// Input lock acquired on a session.
    InputLockGranted { session: String },
    /// Input lock denied on a session.
    InputLockDenied { session: String, holder: ClientId },
    /// Input lock released on a session.
    InputLockReleased { session: String },
}

/// Shared client-side session management logic used by both the TUI and GUI frontends.
///
/// Owns all connection state, session state, terminal buffers, and metrics. The frontend
/// is responsible for the event loop and rendering; it calls `SessionManager` methods
/// to drive session operations and handle server messages.
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

    // Session state
    pub buffers: HashMap<String, CellGrid>,
    pub active_session: Option<String>,
    pub session_list: Vec<SessionInfo>,
    session_sync: HashMap<String, SessionSync>,
    pub input_locked: HashMap<String, bool>,
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
            buffers: HashMap::new(),
            active_session: None,
            session_list: Vec::new(),
            session_sync: HashMap::new(),
            input_locked: HashMap::new(),
            next_request_id: 0,
            client_id: None,
            metrics: RenderMetrics::new(),
        }
    }

    // ── Connection lifecycle ──────────────────────────────────────────────────

    /// Establish a QUIC connection using the stored host/port/token.
    /// Returns any events emitted as a result of the attempt (e.g. success status update).
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

    /// Wire up an already-established sender (used by the GUI subscription model
    /// where `connect::connect` is called outside the manager).
    pub fn set_ws_sender(&mut self, sender: mpsc::UnboundedSender<ClientMessage>) {
        self.ws_sender = Some(sender);
        self.connected = true;
        self.status_msg = format!("Connected to {}:{}", self.host, self.port);
        self.last_host = self.host.clone();
        self.last_port = self.port;
        info!("Connected to kmux-server (external sender)");
    }

    /// Send an initial session list request. Call after `set_ws_sender`.
    pub fn request_session_list(&mut self) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::SessionList { request_id: rid });
    }

    /// Tear down the connection and clear all session state.
    pub fn disconnect(&mut self) {
        self.ws_sender = None;
        self.connected = false;
        self.buffers.clear();
        self.active_session = None;
        self.session_list.clear();
        self.session_sync.clear();
        self.input_locked.clear();
        self.status_msg = "Disconnected".to_string();
    }

    /// Mark the connection as lost (channel closed). Does NOT clear session state
    /// so the UI can still display it while reconnecting.
    pub fn mark_connection_lost(&mut self) {
        self.connected = false;
        self.ws_sender = None;
        self.status_msg = "Connection lost".to_string();
    }

    /// Update the connection params (e.g. from the Connect form).
    pub fn set_connection_params(&mut self, host: String, port: u16, token: String) {
        self.host = host;
        self.port = port;
        self.token = token;
    }

    // ── Server message handling ───────────────────────────────────────────────

    /// Process a single `ServerMessage`, mutating internal state and returning
    /// high-level `SessionEvent`s that require UI-layer reactions.
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
                for info in &sessions {
                    self.buffers.entry(info.name.clone()).or_default();
                }
                if self.active_session.is_none()
                    && let Some(first) = sessions.first()
                {
                    self.active_session = Some(first.name.clone());
                    self.attach_fresh(first.name.clone());
                }
                events.push(SessionEvent::SessionListReceived);
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
                self.attach_fresh(name.clone());
                events.push(SessionEvent::SessionCreated { name });
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
                events.push(SessionEvent::SessionClosed { name });
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
                        return events;
                    }
                    Some(SessionSync::Synced { expected }) if seqno != *expected => {
                        self.metrics.record_seqno_gap(&session, expected.0, seqno.0);
                        self.metrics.record_resync(&session, "seqno gap");
                        if let Some(grid) = self.buffers.get_mut(&session) {
                            grid.clear();
                        }
                        self.attach_fresh(session);
                        return events;
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
                        return events;
                    }
                    Some(SessionSync::Synced { expected }) if seqno != *expected => {
                        self.metrics.record_seqno_gap(&session, expected.0, seqno.0);
                        self.metrics.record_resync(&session, "seqno gap");
                        if let Some(grid) = self.buffers.get_mut(&session) {
                            grid.clear();
                        }
                        self.attach_fresh(session);
                        return events;
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
                events.push(SessionEvent::SessionRenamed { old_name, new_name });
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
                events.push(SessionEvent::ServerError { message });
            }

            ServerMessage::SessionRenamed { old_name, new_name } => {
                self.apply_rename(&old_name, &new_name);
                events.push(SessionEvent::SessionRenamed { old_name, new_name });
            }

            ServerMessage::InputLockGranted { session } => {
                self.input_locked.insert(session.clone(), true);
                self.status_msg = format!("Input lock acquired on '{session}'");
                events.push(SessionEvent::InputLockGranted { session });
            }

            ServerMessage::InputLockDenied { session, holder } => {
                self.status_msg =
                    format!("Input lock denied on '{session}' (held by {:?})", holder);
                events.push(SessionEvent::InputLockDenied { session, holder });
            }

            ServerMessage::InputLockReleased { session } => {
                self.input_locked.insert(session.clone(), false);
                self.status_msg = format!("Input lock released on '{session}'");
                events.push(SessionEvent::InputLockReleased { session });
            }

            _ => {}
        }
        events
    }

    // ── Session operations ────────────────────────────────────────────────────

    /// Switch to a different session: detach old, clear buffer, attach new.
    pub fn select_session(&mut self, name: String) {
        if let Some(prev) = self.active_session.take() {
            self.send_ws(ClientMessage::Detach { session: prev });
        }
        if let Some(buf) = self.buffers.get_mut(&name) {
            buf.clear();
        }
        self.active_session = Some(name.clone());
        self.attach_fresh(name);
    }

    /// Cycle to the next/previous session by offset (wraps around).
    pub fn cycle_session(&mut self, offset: i32) {
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

    /// Create a new session with an auto-generated name.
    pub fn create_session(&mut self) {
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

    /// Close the named session.
    pub fn close_session(&mut self, name: &str) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::SessionClose {
            request_id: rid,
            name: name.to_string(),
        });
    }

    /// Rename a session.
    pub fn rename_session(&mut self, old: &str, new_name: &str) {
        if !new_name.is_empty() && new_name != old {
            let rid = self.next_rid();
            self.send_ws(ClientMessage::SessionRename {
                request_id: rid,
                session: old.to_string(),
                new_name: new_name.to_string(),
            });
        }
    }

    /// Send raw PTY input bytes for the active session.
    /// Returns `false` if input is locked (bytes not sent), `true` otherwise.
    pub fn send_input(&mut self, data: Vec<u8>) -> bool {
        if let Some(session) = self.active_session.clone() {
            let locked = self.input_locked.get(&session).copied().unwrap_or(false);
            if locked {
                self.status_msg = "Input locked on this session".to_string();
                return false;
            }
            self.send_ws(ClientMessage::PtyInput { session, data });
        }
        true
    }

    /// Send a paste string for the active session (server handles bracketed paste wrapping).
    /// Returns `false` if input is locked.
    pub fn send_paste(&mut self, text: String) -> bool {
        if text.is_empty() {
            return true;
        }
        if let Some(session) = self.active_session.clone() {
            let locked = self.input_locked.get(&session).copied().unwrap_or(false);
            if locked {
                self.status_msg = "Input locked on this session".to_string();
                return false;
            }
            self.send_ws(ClientMessage::PtyPaste {
                session,
                data: text,
            });
        }
        true
    }

    /// Send a resize event for the given session and resize the local buffer.
    pub fn send_resize(&mut self, session: &str, rows: u16, cols: u16) {
        if let Some(buf) = self.buffers.get_mut(session) {
            buf.resize(rows, cols);
        }
        self.send_ws(ClientMessage::Resize {
            session: session.to_string(),
            size: TermSize { rows, cols },
        });
    }

    /// Send a Unix signal to the PTY child of the named session.
    pub fn send_signal(&mut self, session: &str, signal: i32) {
        self.send_ws(ClientMessage::Signal {
            session: session.to_string(),
            signal,
        });
    }

    /// Toggle the input lock on the active session.
    pub fn toggle_input_lock(&mut self) {
        if let Some(session) = self.active_session.clone() {
            let locked = self.input_locked.get(&session).copied().unwrap_or(false);
            if locked {
                self.send_ws(ClientMessage::ReleaseInputLock { session });
            } else {
                self.send_ws(ClientMessage::RequestInputLock { session });
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

    pub fn active_session(&self) -> Option<&str> {
        self.active_session.as_deref()
    }

    pub fn session_list(&self) -> &[SessionInfo] {
        &self.session_list
    }

    pub fn buffer(&self, name: &str) -> Option<&CellGrid> {
        self.buffers.get(name)
    }

    pub fn buffer_mut(&mut self, name: &str) -> Option<&mut CellGrid> {
        self.buffers.get_mut(name)
    }

    pub fn active_grid(&self) -> Option<&CellGrid> {
        self.active_session
            .as_ref()
            .and_then(|s| self.buffers.get(s))
    }

    pub fn active_grid_mut(&mut self) -> Option<&mut CellGrid> {
        if let Some(name) = &self.active_session {
            let name = name.clone();
            self.buffers.get_mut(&name)
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

    pub fn is_input_locked(&self, session: &str) -> bool {
        self.input_locked.get(session).copied().unwrap_or(false)
    }

    pub fn active_input_locked(&self) -> bool {
        self.active_session
            .as_ref()
            .map(|s| self.is_input_locked(s))
            .unwrap_or(false)
    }

    pub fn client_id(&self) -> Option<ClientId> {
        self.client_id
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::{GridSnapshot, TermModes};

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

        let sessions = vec![SessionInfo {
            name: "foo".to_string(),
            program: String::new(),
            size: TermSize::default(),
            attached_clients: vec![],
            status: SessionStatus::Running,
        }];
        let events = mgr.handle_server_message(ServerMessage::SessionListResult {
            request_id: 0,
            sessions,
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::SessionListReceived]
        ));
        assert_eq!(mgr.session_list.len(), 1);
        assert_eq!(mgr.active_session.as_deref(), Some("foo"));
        // Attach message should have been sent
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn session_created_switches_active() {
        let (mut mgr, _rx) = make_connected_manager();
        let events = mgr.handle_server_message(ServerMessage::SessionCreated {
            request_id: 0,
            name: "bar".to_string(),
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::SessionCreated { name }] if name == "bar"
        ));
        assert_eq!(mgr.active_session.as_deref(), Some("bar"));
        assert!(mgr.buffers.contains_key("bar"));
    }

    #[test]
    fn session_closed_removes_and_falls_back() {
        let (mut mgr, _rx) = make_connected_manager();
        // Populate two sessions
        mgr.buffers.insert("s1".to_string(), CellGrid::default());
        mgr.buffers.insert("s2".to_string(), CellGrid::default());
        mgr.session_list.push(SessionInfo {
            name: "s1".to_string(),
            program: String::new(),
            size: TermSize::default(),
            attached_clients: vec![],
            status: SessionStatus::Running,
        });
        mgr.session_list.push(SessionInfo {
            name: "s2".to_string(),
            program: String::new(),
            size: TermSize::default(),
            attached_clients: vec![],
            status: SessionStatus::Running,
        });
        mgr.active_session = Some("s1".to_string());

        let events = mgr.handle_server_message(ServerMessage::SessionClosed {
            request_id: 0,
            name: "s1".to_string(),
            exit_code: None,
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::SessionClosed { name }] if name == "s1"
        ));
        assert!(!mgr.buffers.contains_key("s1"));
        // Should fall back to s2
        assert_eq!(mgr.active_session.as_deref(), Some("s2"));
    }

    #[test]
    fn terminal_snapshot_transitions_to_synced() {
        let (mut mgr, _rx) = make_connected_manager();
        mgr.buffers.insert("sess".to_string(), CellGrid::default());
        mgr.session_sync
            .insert("sess".to_string(), SessionSync::AwaitingSync);

        let snapshot = GridSnapshot {
            rows: 24,
            cols: 80,
            cells: vec![],
            cursor: Default::default(),
            modes: TermModes::EMPTY,
        };
        mgr.handle_server_message(ServerMessage::TerminalSnapshot {
            session: "sess".to_string(),
            snapshot,
            seqno: SequenceNo(5),
            sent_at_ms: 0,
        });

        // Now synced at seqno 6
        assert!(matches!(
            mgr.session_sync.get("sess"),
            Some(SessionSync::Synced {
                expected: SequenceNo(6)
            })
        ));
    }

    #[test]
    fn terminal_update_discarded_when_awaiting_sync() {
        use kmux_protocol::messages::TerminalDiff;
        let (mut mgr, _rx) = make_connected_manager();
        mgr.buffers.insert("sess".to_string(), CellGrid::default());
        mgr.session_sync
            .insert("sess".to_string(), SessionSync::AwaitingSync);

        let diff = Arc::new(TerminalDiff {
            ops: vec![],
            cursor: Default::default(),
            modes: TermModes::EMPTY,
            scrollback_lines: vec![],
        });
        mgr.handle_server_message(ServerMessage::TerminalUpdate {
            session: "sess".to_string(),
            diff,
            seqno: SequenceNo(0),
            sent_at_ms: 0,
        });

        assert_eq!(mgr.metrics.snapshot(false).counters.stale_discards, 1);
    }

    #[test]
    fn terminal_update_seqno_gap_triggers_resync() {
        use kmux_protocol::messages::TerminalDiff;
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.buffers.insert("sess".to_string(), CellGrid::default());
        mgr.session_sync.insert(
            "sess".to_string(),
            SessionSync::Synced {
                expected: SequenceNo(3),
            },
        );

        let diff = Arc::new(TerminalDiff {
            ops: vec![],
            cursor: Default::default(),
            modes: TermModes::EMPTY,
            scrollback_lines: vec![],
        });
        // Send seqno 99 when expecting 3 → gap
        mgr.handle_server_message(ServerMessage::TerminalUpdate {
            session: "sess".to_string(),
            diff,
            seqno: SequenceNo(99),
            sent_at_ms: 0,
        });

        assert_eq!(mgr.metrics.snapshot(false).counters.seqno_gaps, 1);
        assert_eq!(mgr.metrics.snapshot(false).counters.resyncs, 1);
        // Attach was re-sent
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn cycle_session_wraps_around() {
        let (mut mgr, _rx) = make_connected_manager();
        for name in ["a", "b", "c"] {
            mgr.session_list.push(SessionInfo {
                name: name.to_string(),
                program: String::new(),
                size: TermSize::default(),
                attached_clients: vec![],
                status: SessionStatus::Running,
            });
            mgr.buffers.insert(name.to_string(), CellGrid::default());
        }
        mgr.active_session = Some("c".to_string());
        mgr.cycle_session(1);
        assert_eq!(mgr.active_session.as_deref(), Some("a")); // wraps from c to a
    }

    #[test]
    fn apply_rename_updates_all_maps() {
        let mut mgr = make_manager();
        mgr.buffers.insert("old".to_string(), CellGrid::default());
        mgr.session_sync
            .insert("old".to_string(), SessionSync::AwaitingSync);
        mgr.input_locked.insert("old".to_string(), true);
        mgr.session_list.push(SessionInfo {
            name: "old".to_string(),
            program: String::new(),
            size: TermSize::default(),
            attached_clients: vec![],
            status: SessionStatus::Running,
        });
        mgr.active_session = Some("old".to_string());

        mgr.apply_rename("old", "new");

        assert!(!mgr.buffers.contains_key("old"));
        assert!(mgr.buffers.contains_key("new"));
        assert!(!mgr.session_sync.contains_key("old"));
        assert!(mgr.session_sync.contains_key("new"));
        assert!(!mgr.input_locked.contains_key("old"));
        assert!(mgr.input_locked.contains_key("new"));
        assert_eq!(mgr.session_list[0].name, "new");
        assert_eq!(mgr.active_session.as_deref(), Some("new"));
    }
}
