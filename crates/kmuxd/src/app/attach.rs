use std::sync::Arc;
use std::sync::atomic::Ordering;

use kmux_protocol::messages::{
    ClientCapabilities, ClientId, GridSnapshot, InputMode, SequenceNo, ServerMessage, TermSize,
    TerminalDiff,
};
use kmux_pty::error::{KmuxError, Result};
use tokio::sync::mpsc;

use super::helpers::parse_pane_id;
use super::{ClientSender, ServerApp};

/// Maximum number of buffered diffs to replay on a delta attach/resume before
/// coalescing to a single snapshot instead (issue #68).
pub(super) const MAX_RESUME_DELTA_DIFFS: usize = 256;

/// Maximum estimated byte size of buffered diffs to replay on a delta
/// attach/resume before coalescing to a single snapshot instead. A grid
/// snapshot is on the order of low tens of KiB, so once the pending delta
/// exceeds this a snapshot is almost always the cheaper catch-up.
pub(super) const MAX_RESUME_DELTA_BYTES: usize = 256 * 1024;

/// Parameters for [`ServerApp::attach`].
pub struct AttachParams {
    pub pane_id: String,
    pub client_id: ClientId,
    pub last_seqno: Option<SequenceNo>,
    pub size: TermSize,
    pub data_tx: mpsc::Sender<ServerMessage>,
    pub ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
    pub capabilities: ClientCapabilities,
}

/// Result of an attach operation describing what replay data to send.
#[derive(Debug)]
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

/// Compute the catch-up payload for an attach/resume from a pane's relay state.
///
/// - `None` last_seqno (fresh attach) → full snapshot.
/// - `Some(seq)` within the scrollback buffer → delta replay of the missed
///   diffs, unless they exceed the coalescing threshold (e.g. after a long
///   pause), in which case a single final-state snapshot (`SyncReset`) is sent.
/// - `Some(seq)` older than the buffer → `SyncReset` with a fresh snapshot.
pub(super) fn compute_replay(
    relay: &super::PaneRelay,
    last_seqno: Option<SequenceNo>,
) -> AttachResult {
    let current_seqno = || {
        SequenceNo(
            relay
                .seqno_counter
                .load(Ordering::Relaxed)
                .saturating_sub(1),
        )
    };
    match last_seqno {
        None => {
            let snapshot = relay.term_state.lock().unwrap().snapshot();
            AttachResult::FullSnapshot(snapshot, current_seqno())
        }
        Some(seq) => {
            let buf = relay.scrollback.lock().unwrap();
            match buf.oldest_seqno() {
                Some(oldest) if seq >= oldest => {
                    // Replay the missed diffs, unless they have piled up past the
                    // coalescing threshold (e.g. after a long pause). In that case
                    // send a single snapshot of the final state — catch-up cost
                    // stays O(screen), not O(time paused).
                    let (count, bytes) = buf.pending_stats(seq);
                    if count > MAX_RESUME_DELTA_DIFFS || bytes > MAX_RESUME_DELTA_BYTES {
                        let snapshot = relay.term_state.lock().unwrap().snapshot();
                        AttachResult::SyncReset(snapshot, current_seqno())
                    } else {
                        AttachResult::Delta(buf.since(seq))
                    }
                }
                _ => {
                    let snapshot = relay.term_state.lock().unwrap().snapshot();
                    AttachResult::SyncReset(snapshot, current_seqno())
                }
            }
        }
    }
}

impl ServerApp {
    /// Register a client's output channel for a pane and return replay data.
    pub async fn attach(&self, params: AttachParams) -> Result<AttachResult> {
        let AttachParams {
            pane_id,
            client_id,
            last_seqno,
            size,
            data_tx,
            ctrl_tx,
            capabilities,
        } = params;
        let (word_id, pane_index) =
            parse_pane_id(&pane_id).ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.clone(),
            })?;

        // Write lock needed so we can update relay.size via apply_effective_size.
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let relay = state
            .panes
            .get_mut(&pane_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;

        let result = compute_replay(relay, last_seqno);

        {
            let mut clients = relay.clients.lock().unwrap();
            // Preserve connection-level flags across a re-attach. Without this,
            // a re-attach (reconnect, tab switch, resume) silently resets
            // snapshot mode. A re-attach implies the client is live again, so
            // `paused` is cleared — resume reconciliation flows through here.
            let force_full_snapshot = clients
                .get(&client_id)
                .map(|s| s.force_full_snapshot)
                .unwrap_or(false);
            clients.insert(
                client_id,
                ClientSender {
                    data_tx,
                    ctrl_tx,
                    force_full_snapshot,
                    paused: false,
                    capabilities,
                    size,
                },
            );
        }
        relay.recompute_live_capabilities();

        // Reconcile effective size after new client joined.
        let seqno = relay
            .seqno_counter
            .load(Ordering::Relaxed)
            .saturating_sub(1);
        if let Some(new_size) = relay.apply_effective_size() {
            relay.broadcast_resize(pane_id.as_str(), new_size, seqno);
        }

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

    /// Pause or resume terminal-output delivery for a client across all panes
    /// (issue #68). While paused the relay skips this client; the pane keeps
    /// running and the client keeps counting toward the effective pane size.
    /// Resume reconciliation happens when the client re-attaches its panes.
    pub async fn set_paused(&self, client_id: ClientId, paused: bool) {
        let sessions = self.sessions.read().await;
        for state in sessions.values() {
            for relay in state.panes.values() {
                let mut map = relay.clients.lock().unwrap();
                if let Some(sender) = map.get_mut(&client_id) {
                    sender.paused = paused;
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
            // Reconcile effective size; no clients left → keep current size.
            let seqno = relay
                .seqno_counter
                .load(Ordering::Relaxed)
                .saturating_sub(1);
            if let Some(new_size) = relay.apply_effective_size() {
                relay.broadcast_resize(pane_id, new_size, seqno);
            }
        }
    }

    /// Remove a client from all panes they were attached to.
    pub async fn detach_client_all(&self, client_id: ClientId) {
        let mut sessions = self.sessions.write().await;
        for state in sessions.values_mut() {
            for (pane_index, relay) in state.panes.iter_mut() {
                let pane_id = format!("{}/{}", state.meta.word_id, pane_index);
                relay.clients.lock().unwrap().remove(&client_id);
                relay.recompute_live_capabilities();
                if relay.input_mode == InputMode::Locked(client_id) {
                    relay.input_mode = InputMode::Open;
                }
                let seqno = relay
                    .seqno_counter
                    .load(Ordering::Relaxed)
                    .saturating_sub(1);
                if let Some(new_size) = relay.apply_effective_size() {
                    relay.broadcast_resize(pane_id.as_str(), new_size, seqno);
                }
            }
        }
    }
}
