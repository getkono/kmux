use std::sync::Arc;
use std::time::Instant;

use kmux_protocol::messages::{
    ClientId, ClientMessage, PaneInfo, SequenceNo, ServerMessage, SessionEntry, SessionEventMsg,
    SessionStatus, TermSize, epoch_millis,
};
use tracing::{info, warn};

use super::{PaneSync, SessionManager};

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

impl SessionManager {
    /// Check that the pane's sequence number is in sync. Returns `true` if
    /// processing should continue, `false` if the update was discarded/resynced
    /// (the caller should return immediately on `false`).
    fn check_pane_sync(&mut self, pane_id: &str, seqno: SequenceNo) -> bool {
        match self.pane_sync.get(pane_id) {
            Some(PaneSync::AwaitingSync) => {
                self.metrics.record_stale_discard(pane_id);
                false
            }
            Some(PaneSync::Synced { expected }) if seqno != *expected => {
                self.metrics.record_seqno_gap(pane_id, expected.0, seqno.0);
                self.metrics.record_resync(pane_id, "seqno gap");
                if let Some(grid) = self.buffers.get_mut(pane_id) {
                    grid.clear();
                }
                self.attach_fresh(pane_id.to_string());
                false
            }
            _ => true,
        }
    }

    pub fn handle_server_message(&mut self, msg: ServerMessage) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        match msg {
            ServerMessage::AuthResult {
                success,
                reason,
                client_id,
                server_version,
                connection_id,
            } => {
                if success {
                    self.client_id = client_id;
                    self.server_version = server_version;
                    self.connection_id = connection_id;
                    events.push(SessionEvent::AuthOk);
                } else {
                    warn!("Auth failed: {:?}", reason);
                    let reason_str = reason.unwrap_or_default();
                    let hint = kmux_protocol::messages::version_mismatch_hint(&reason_str);
                    if hint.is_empty() {
                        self.status_msg = format!("Auth failed: {reason_str}");
                    } else {
                        self.status_msg = format!("Auth failed: {reason_str} | {hint}");
                    }
                    self.ws_sender = None;
                    self.connected = false;
                    events.push(SessionEvent::AuthFailed { reason: reason_str });
                }
            }

            ServerMessage::ChannelSwitched { old_transport } => {
                let new_transport = self.current_transport;
                info!(
                    "Transport channel switched: {} -> {}",
                    old_transport, new_transport
                );
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
                if !self.check_pane_sync(&pane_id, seqno) {
                    return events;
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
                if !self.check_pane_sync(&pane_id, seqno) {
                    return events;
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
            }
            | ServerMessage::SessionRenamed { word_id, new_name } => {
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
}

/// Get the word_id of the first session in the list.
fn e_word_id_from_list(list: &[SessionEntry]) -> String {
    list.first()
        .map(|e| e.meta.word_id.clone())
        .unwrap_or_default()
}
