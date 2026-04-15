use std::sync::Arc;
use std::sync::atomic::Ordering;

use kmux_protocol::messages::{
    ClientCapabilities, ClientId, GridSnapshot, InputMode, SequenceNo, ServerMessage, TerminalDiff,
};
use kmux_pty::error::{KmuxError, Result};
use tokio::sync::mpsc;

use super::helpers::parse_pane_id;
use super::{ClientSender, ServerApp};

/// Result of an attach operation describing what replay data to send.
pub enum AttachResult {
    /// Fresh attach or first-time connect: full grid snapshot from TermState.
    FullSnapshot(GridSnapshot, SequenceNo),
    /// Delta replay: only diffs with seqno > last_seqno.
    Delta(Vec<(SequenceNo, Arc<TerminalDiff>)>),
    /// Requested seqno was too old; full snapshot sent, client must reset state.
    SyncReset(GridSnapshot, SequenceNo),
}

/// Outcome of an input lock request.
pub enum InputLockOutcome {
    Granted,
    Denied(ClientId),
}

impl ServerApp {
    /// Register a client's output channel for a pane and return replay data.
    pub async fn attach(
        &self,
        pane_id: &str,
        client_id: ClientId,
        last_seqno: Option<SequenceNo>,
        data_tx: mpsc::Sender<ServerMessage>,
        ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
        capabilities: ClientCapabilities,
    ) -> Result<AttachResult> {
        let (word_id, pane_index) =
            parse_pane_id(pane_id).ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;

        let sessions = self.sessions.read().await;
        let state = sessions
            .get(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let relay = state
            .panes
            .get(&pane_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;

        let result = match last_seqno {
            None => {
                let snapshot = relay.term_state.lock().unwrap().snapshot();
                let current_seqno = SequenceNo(
                    relay
                        .seqno_counter
                        .load(Ordering::Relaxed)
                        .saturating_sub(1),
                );
                AttachResult::FullSnapshot(snapshot, current_seqno)
            }
            Some(seq) => {
                let buf = relay.scrollback.lock().unwrap();
                match buf.oldest_seqno() {
                    Some(oldest) if seq >= oldest => AttachResult::Delta(buf.since(seq)),
                    _ => {
                        let snapshot = relay.term_state.lock().unwrap().snapshot();
                        let current_seqno = SequenceNo(
                            relay
                                .seqno_counter
                                .load(Ordering::Relaxed)
                                .saturating_sub(1),
                        );
                        AttachResult::SyncReset(snapshot, current_seqno)
                    }
                }
            }
        };

        relay.clients.lock().unwrap().insert(
            client_id,
            ClientSender {
                data_tx,
                ctrl_tx,
                force_full_snapshot: false,
                capabilities,
            },
        );
        relay.recompute_live_capabilities();

        Ok(result)
    }

    /// Set the full-snapshot mode flag for a client across all attached panes.
    pub async fn set_snapshot_mode(&self, client_id: ClientId, enabled: bool) {
        let sessions = self.sessions.read().await;
        for state in sessions.values() {
            for relay in state.panes.values() {
                let mut map = relay.clients.lock().unwrap();
                if let Some(sender) = map.get_mut(&client_id) {
                    sender.force_full_snapshot = enabled;
                }
            }
        }
    }

    /// Remove a client from a specific pane and release any lock they hold.
    pub async fn detach_from_pane(&self, pane_id: &str, client_id: ClientId) {
        let Some((word_id, pane_index)) = parse_pane_id(pane_id) else {
            return;
        };
        let mut sessions = self.sessions.write().await;
        if let Some(state) = sessions.get_mut(word_id)
            && let Some(relay) = state.panes.get_mut(&pane_index)
        {
            relay.clients.lock().unwrap().remove(&client_id);
            relay.recompute_live_capabilities();
            if relay.input_mode == InputMode::Locked(client_id) {
                relay.input_mode = InputMode::Open;
            }
        }
    }

    /// Remove a client from all panes they were attached to.
    pub async fn detach_client_all(&self, client_id: ClientId) {
        let mut sessions = self.sessions.write().await;
        for state in sessions.values_mut() {
            for relay in state.panes.values_mut() {
                relay.clients.lock().unwrap().remove(&client_id);
                relay.recompute_live_capabilities();
                if relay.input_mode == InputMode::Locked(client_id) {
                    relay.input_mode = InputMode::Open;
                }
            }
        }
    }
}
