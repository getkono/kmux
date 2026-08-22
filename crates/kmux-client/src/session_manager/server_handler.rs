use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use kmux_protocol::messages::{
    AttentionKind, ClientId, ClientMessage, PaneInfo, PaneProgressState, SequenceNo, ServerMessage,
    SessionEventMsg, SessionStatus, epoch_millis,
};
use tracing::{info, warn};

use super::{DirListing, PaneSync, SessionManager};

/// High-level events emitted by `handle_server_message` for the UI to react to.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Authentication succeeded.
    AuthOk,
    /// Authentication failed; UI should show connect form.
    AuthFailed { reason: String },
    /// Session list was received.
    SessionListReceived,
    /// A process-overview snapshot was received (issue #122); the overview view
    /// should repaint from `SessionManager::process_overview`.
    ProcessOverviewReceived,
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
    /// A pane's isolated VT worker crashed (issue #126). The pane's shell is
    /// still alive — the daemon holds the PTY master fd and respawns the worker,
    /// resyncing the pane with a fresh snapshot — so the frontend surfaces this
    /// as a transient "recovering" notice, never as an exit.
    PaneFaulted {
        /// The pane whose worker crashed.
        pane_id: String,
    },
    /// A pane's window title changed (OSC 0/2).
    PaneTitleChanged { pane_id: String, title: String },
    /// A pane rang BEL; frontends surface this as unread tab attention.
    PaneBell { pane_id: String },
    /// A pane's OSC 9;4 progress changed (ConEmu/WT progress bar). The frontend
    /// repaints the pane's progress bar from the cached `PaneInfo`.
    PaneProgressChanged {
        pane_id: String,
        state: PaneProgressState,
        progress: Option<u8>,
    },
    /// A pane's program wrote the clipboard via OSC 52. `data` is the still
    /// base64-encoded payload; the app layer decodes and applies it (subject to
    /// the active-pane policy) at the frontend's clipboard leaf.
    ClipboardCopy {
        pane_id: String,
        selection: String,
        data: String,
    },
    /// A program inside a pane asked for the user's attention via `kmux notify`
    /// (issue #169). The frontend raises a native desktop notification that, on
    /// click, refocuses the window for `word_id` and selects the pane.
    /// `attention_id` is unique per request so a frontend with several windows
    /// on this session posts exactly one notification.
    PaneAttention {
        word_id: String,
        pane_id: String,
        kind: AttentionKind,
        title: String,
        body: String,
        attention_id: u64,
    },
    /// A structured error from the server.
    ServerError { message: String },
    /// Input lock acquired on a pane.
    InputLockGranted { pane_id: String },
    /// Input lock denied on a pane.
    InputLockDenied { pane_id: String, holder: ClientId },
    /// Input lock released on a pane.
    InputLockReleased { pane_id: String },
    /// A directory listing arrived (in response to `request_list_directory`);
    /// the app-layer directory browser should repaint.
    DirectoryListed,
    /// The connected-clients list arrived (issue #146) in response to
    /// [`SessionManager::request_client_list`]; the connected-clients view should
    /// repaint from [`SessionManager::client_list`]. `word_id` is the session it
    /// pertains to.
    ClientListReceived { word_id: String },
    /// A kick this client requested succeeded (issue #146).
    ClientKicked {
        word_id: String,
        client_id: ClientId,
    },
    /// This client was kicked from `word_id` by another client (issue #146);
    /// `by_label` names who. The app should leave the session.
    KickedFromSession { word_id: String, by_label: String },
    /// A federated peer was opened (issue #121): the local daemon now proxies
    /// the remote's sessions. The app should refresh the session list so they
    /// surface, then (re)run auto-select.
    PeerOpened { peer: String },
    /// Opening a federated peer failed (SSH/connect/auth error on the daemon's
    /// upstream link). `peer` is the target's [`PeerId`](kmux_protocol::messages::PeerId)
    /// when the daemon could attribute the failure, so the launcher can mark just
    /// that remote as errored instead of tearing down the whole UI.
    PeerError {
        peer: Option<String>,
        reason: String,
    },
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
                self.in_flight_history_fetches.remove(pane_id);
                self.attach_fresh(pane_id.to_string());
                false
            }
            _ => true,
        }
    }

    /// Drop every per-pane buffer and bookkeeping entry for a closed pane, keeping
    /// the buffer / `pane_sync` / input-lock / fetch maps in lock-step (a pane must
    /// never outlive its sync state).
    fn forget_pane(&mut self, pane_id: &str) {
        self.buffers.remove(pane_id);
        if let Some(handle) = &self.apply {
            handle.forget_pane(pane_id.to_string());
        }
        self.pane_sync.remove(pane_id);
        self.input_locked.remove(pane_id);
        self.in_flight_history_fetches.remove(pane_id);
    }

    /// Record a successfully-applied update: advance the pane's expected seqno and
    /// log apply latency. Shared by every Terminal* / Cursor / Scrollback arm.
    fn mark_synced(&mut self, pane_id: String, seqno: SequenceNo, start: Instant, sent_at_ms: u64) {
        self.pane_sync.insert(
            pane_id,
            PaneSync::Synced {
                expected: SequenceNo(seqno.0 + 1),
            },
        );
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        self.metrics.record_apply(sent_at_ms, elapsed_ms);
    }

    pub fn handle_server_message(&mut self, msg: ServerMessage) -> Vec<SessionEvent> {
        let mut events = Vec::new();

        // Any decoded frame is proof the server is alive. Refreshing the
        // liveness timer here — in one place — means every transport gets
        // timeout detection for free.
        self.liveness.observe_inbound(Instant::now());

        // Attribute the wire cost of this frame to the currently tagged
        // transport. We re-encode for the byte count; MessagePack is cheap
        // enough that this is negligible next to decoding.
        let inbound_bytes = kmux_protocol::encode_server(&msg).map_or(0, |b| b.len());
        let inbound_category = msg.category();
        if inbound_bytes > 0 {
            self.metrics.record_inbound(inbound_bytes, inbound_category);
        }

        match msg {
            ServerMessage::AuthResult {
                success,
                reason,
                client_id,
                server_version,
                connection_id,
                compression,
                machine_id,
                label,
                server_machine_id,
                negotiated_protocol,
                negotiated_capabilities,
            } => {
                if success {
                    self.client_id = client_id;
                    self.server_version = server_version;
                    self.connection_id = connection_id;
                    self.machine_id = machine_id;
                    self.label = label;
                    self.server_machine_id = server_machine_id;
                    self.negotiated_protocol = negotiated_protocol;
                    self.negotiated_capabilities = negotiated_capabilities;
                    // The daemon decides compression; frames self-describe, so
                    // this is informational only (see docs/compression.md).
                    info!(
                        protocol = ?self.negotiated_protocol,
                        capabilities = ?self.negotiated_capabilities,
                        "Authenticated (wire compression: {compression:?})"
                    );
                    events.push(SessionEvent::AuthOk);
                } else {
                    warn!("Auth failed: {:?}", reason);
                    let reason_str = reason.unwrap_or_default();
                    let hint = kmux_protocol::messages::version_mismatch_hint(&reason_str);
                    let msg = if hint.is_empty() {
                        format!("Auth failed: {reason_str}")
                    } else {
                        format!("Auth failed: {reason_str} | {hint}")
                    };
                    self.ws_sender = None;
                    self.set_connection_state(
                        crate::connection_state::ConnectionState::Disconnected {
                            reason: crate::connection_state::DisconnectReason::AuthFailed(msg),
                        },
                    );
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
                        self.ensure_pane(&pane.pane_id);
                    }
                }
                // Resolve a deferred "focus the pane I just created" once its tab
                // is known; else pick an initial session; else re-sync the active
                // session's tab after a refresh.
                if let Some(pending) = self.pending_select_pane.take() {
                    if self.locate_pane(&pending).is_some() {
                        self.select_pane(pending);
                    } else {
                        self.pending_select_pane = Some(pending);
                    }
                } else if self.active_session.is_none() {
                    if let Some(first) = sessions.first().map(|e| e.meta.word_id.clone()) {
                        self.select_session(first);
                    }
                } else if self.visible_panes.is_empty()
                    && let Some(word) = self.active_session.clone()
                {
                    self.select_session(word);
                }
                events.push(SessionEvent::SessionListReceived);
            }

            ServerMessage::ClosedSessionListResult { sessions, .. } => {
                // Already ordered most-recently-active first by the daemon. The
                // launcher polls `closed_session_list()` when it opens (issue #64).
                self.closed_sessions = sessions;
            }

            ServerMessage::ProcessOverviewResult { panes, .. } => {
                self.process_overview = panes;
                events.push(SessionEvent::ProcessOverviewReceived);
            }

            ServerMessage::SessionCreated { entry, .. } => {
                let word_id = entry.meta.word_id.clone();
                for pane in &entry.panes {
                    self.ensure_pane(&pane.pane_id);
                }
                self.session_list.push(entry);
                self.status_msg = format!("Session '{word_id}' created");
                // Switch to the new session (detaches the old visible set and
                // attaches the new session's active tab).
                self.select_session(word_id.clone());
                events.push(SessionEvent::SessionCreated { word_id });
            }

            // A session died. The requester gets the reply; every client gets the
            // broadcast when the session drained via its last tab or pane.
            ServerMessage::SessionClosed { word_id, .. }
            | ServerMessage::Event {
                event: SessionEventMsg::SessionClosed { word_id },
            } => {
                events.extend(self.on_session_gone(word_id));
            }

            ServerMessage::PaneCreated {
                pane_id,
                session_word_id,
                size,
                ..
            } => {
                self.ensure_pane(&pane_id);
                // Record the new pane in the flat list for immediate chrome.
                if let Some(entry) = self
                    .session_list
                    .iter_mut()
                    .find(|e| e.meta.word_id == session_word_id)
                {
                    let pane_index = kmux_protocol::pane_index(&pane_id).unwrap_or(0);
                    if !entry.panes.iter().any(|p| p.pane_id == pane_id) {
                        entry.panes.push(PaneInfo {
                            pane_id: pane_id.clone(),
                            pane_index,
                            program: String::new(),
                            size,
                            attached_clients: vec![],
                            status: SessionStatus::Running,
                            title: String::new(),
                            progress_state: Default::default(),
                            progress: None,
                        });
                    }
                }
                // `PaneCreate` creates a new tab server-side; its layout arrives
                // with the refreshed session list. Defer focusing the new pane
                // until then.
                self.pending_select_pane = Some(pane_id.clone());
                self.request_session_list();
                events.push(SessionEvent::PaneCreated { pane_id });
            }

            // A pane died. The requester gets the reply (with the exit code);
            // every client gets the PTY bus's broadcast of the same close.
            ServerMessage::PaneClosed { pane_id, .. }
            | ServerMessage::Event {
                event: SessionEventMsg::PaneClosed { pane_id },
            } => {
                events.extend(self.on_pane_gone(pane_id));
            }

            // ── Tab / layout reconciliation ─────────────────────────────────
            // `LayoutUpdate` is the authoritative tree (+ shared focus) broadcast
            // after any mutation. Update the cache, and when it targets the tab
            // this client is viewing, reconcile the visible set + focus.
            ServerMessage::LayoutUpdate {
                word_id,
                tab_index,
                layout,
                focused_pane,
            } => {
                if let Some(tab) = self
                    .session_list
                    .iter_mut()
                    .find(|e| e.meta.word_id == word_id)
                    .and_then(|e| e.tabs.iter_mut().find(|t| t.tab_index == tab_index))
                {
                    tab.layout = layout;
                    tab.focused_pane = focused_pane;
                }
                if self.active_session.as_deref() == Some(word_id.as_str())
                    && self.active_tab == Some(tab_index)
                    && let Some((focus_idx, visible)) = self.tab_view(&word_id, tab_index)
                {
                    self.set_visible_set(visible);
                    self.focus_from_tab(&word_id, focus_idx);
                }
            }

            // A tab was created (a different client, or via `TabCreate`). The
            // event carries the tab index but not its full layout, so refresh.
            ServerMessage::TabCreated { word_id, tab, .. } => {
                let tab_index = tab.tab_index;
                if let Some(entry) = self
                    .session_list
                    .iter_mut()
                    .find(|e| e.meta.word_id == word_id)
                    && !entry.tabs.iter().any(|t| t.tab_index == tab.tab_index)
                {
                    entry.tabs.push(tab);
                }
                // If this is our active session, switch to the new tab.
                if self.active_session.as_deref() == Some(word_id.as_str()) {
                    self.select_tab(tab_index);
                }
            }

            // A tab died. The requester gets the reply; every client gets the
            // broadcast, which the daemon sends with no accompanying
            // `LayoutUpdate` — this arm is the only reconciliation there is.
            ServerMessage::TabClosed {
                word_id, tab_index, ..
            }
            | ServerMessage::Event {
                event: SessionEventMsg::TabClosed { word_id, tab_index },
            } => {
                self.on_tab_gone(&word_id, tab_index);
            }

            // The dedicated split reply: a new pane + the tab's new tree. Attach
            // the new pane (without detaching siblings) when it's our active tab.
            ServerMessage::PaneSplit {
                word_id,
                tab_index,
                new_pane,
                layout,
                ..
            } => {
                self.ensure_pane(&new_pane.pane_id);
                let new_idx = new_pane.pane_index;
                if let Some(entry) = self
                    .session_list
                    .iter_mut()
                    .find(|e| e.meta.word_id == word_id)
                {
                    if !entry.panes.iter().any(|p| p.pane_id == new_pane.pane_id) {
                        entry.panes.push(new_pane);
                    }
                    if let Some(tab) = entry.tabs.iter_mut().find(|t| t.tab_index == tab_index) {
                        tab.layout = layout;
                        tab.focused_pane = new_idx;
                    }
                }
                if self.active_session.as_deref() == Some(word_id.as_str())
                    && self.active_tab == Some(tab_index)
                    && let Some((focus_idx, visible)) = self.tab_view(&word_id, tab_index)
                {
                    self.set_visible_set(visible);
                    self.focus_from_tab(&word_id, focus_idx);
                }
            }

            ServerMessage::TerminalSnapshot {
                pane_id,
                snapshot,
                seqno,
                sent_at_ms,
            } => {
                let start = Instant::now();
                let grid = self.ensure_pane(&pane_id);
                // The client owns its freshly-decoded Arc (refcount 1), so this
                // moves the grid out rather than cloning it.
                grid.apply_snapshot(Arc::unwrap_or_clone(snapshot));
                self.mark_synced(pane_id, seqno, start, sent_at_ms);
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
                self.mark_synced(pane_id.clone(), seqno, start, sent_at_ms);
                if op_count > 100 {
                    let net_apply_ms = epoch_millis().saturating_sub(sent_at_ms) as f64;
                    self.metrics.record_large_diff(net_apply_ms);
                }
                self.maybe_fetch_history(&pane_id);
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
                self.mark_synced(pane_id, seqno, start, sent_at_ms);
            }

            ServerMessage::SyncReset { pane_id } => {
                if let Some(grid) = self.buffers.get_mut(&pane_id) {
                    grid.clear();
                }
                self.in_flight_history_fetches.remove(&pane_id);
                self.metrics.record_resync(&pane_id, "server sync reset");
                self.pane_sync.insert(pane_id, PaneSync::AwaitingSync);
            }

            ServerMessage::GridDigest {
                pane_id,
                seqno,
                hash,
            } => {
                // The digest certifies the grid as of `seqno`. Only verify when
                // the pane is synced at EXACTLY that seqno (its next-expected is
                // `seqno + 1`); otherwise the client is mid-stream, resyncing, or
                // the digest is stale, and a comparison would be meaningless. The
                // digest carries no new seqno and never advances sync state — it
                // is a pure side-band check. Skip while a lazy `FetchHistory` is
                // outstanding: the client is legitimately behind on the envelope
                // counts the digest covers, so a mismatch would be a false alarm.
                let synced_here = matches!(
                    self.pane_sync.get(&pane_id),
                    Some(PaneSync::Synced { expected }) if expected.0 == seqno.0 + 1
                );
                if synced_here {
                    // A `Published` pane's content lives on the apply worker, so
                    // the digest is checked there (in-order with the data it
                    // certifies) and a mismatch returns via `WorkerNote`, handled
                    // in `drain_apply_notes`. A `Local` pane checks inline and
                    // returns `Some(mismatch)`.
                    let inline_mismatch = self
                        .buffers
                        .get_mut(&pane_id)
                        .and_then(|grid| grid.request_digest_check(seqno, hash));
                    if inline_mismatch == Some(true) {
                        self.metrics.record_digest_mismatch(&pane_id, seqno.0);
                        self.metrics.record_resync(&pane_id, "grid digest mismatch");
                        if let Some(grid) = self.buffers.get_mut(&pane_id) {
                            grid.clear();
                        }
                        self.in_flight_history_fetches.remove(&pane_id);
                        self.attach_fresh(pane_id);
                    }
                }
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

            // A tab was renamed (by this or another client). Update the cached
            // name; the frontend's tab strip reconciles from it next tick.
            ServerMessage::Event {
                event:
                    SessionEventMsg::TabRenamed {
                        word_id,
                        tab_index,
                        name,
                    },
            } => {
                if let Some(entry) = self
                    .session_list
                    .iter_mut()
                    .find(|e| e.meta.word_id == word_id)
                    && let Some(tab) = entry.tabs.iter_mut().find(|t| t.tab_index == tab_index)
                {
                    tab.name = name;
                }
            }

            ServerMessage::Event {
                event:
                    SessionEventMsg::TabsReordered {
                        word_id,
                        tab_indices,
                    },
            } => {
                if let Some(entry) = self
                    .session_list
                    .iter_mut()
                    .find(|entry| entry.meta.word_id == word_id)
                {
                    entry.tabs.sort_by_key(|tab| {
                        tab_indices
                            .iter()
                            .position(|index| *index == tab.tab_index)
                            .unwrap_or(usize::MAX)
                    });
                }
            }

            ServerMessage::Event {
                event: SessionEventMsg::PaneResized { pane_id, size },
            } => {
                if let Some(grid) = self.buffers.get_mut(&pane_id) {
                    grid.resize(size.rows, size.cols);
                }
            }

            ServerMessage::Event {
                event: SessionEventMsg::PaneTitleChanged { pane_id, title },
            } => {
                for entry in &mut self.session_list {
                    if let Some(pane) = entry.panes.iter_mut().find(|p| p.pane_id == pane_id) {
                        pane.title = title.clone();
                        break;
                    }
                }
                events.push(SessionEvent::PaneTitleChanged { pane_id, title });
            }

            ServerMessage::Event {
                event: SessionEventMsg::PaneBell { pane_id },
            } => {
                self.attention_panes.insert(pane_id.clone());
                events.push(SessionEvent::PaneBell { pane_id });
            }

            ServerMessage::Event {
                event:
                    SessionEventMsg::PaneProgressChanged {
                        pane_id,
                        state,
                        progress,
                    },
            } => {
                // Update the cached snapshot so the frontend's per-pane progress
                // bar repaints from `PaneInfo` on the next render tick.
                for entry in &mut self.session_list {
                    if let Some(pane) = entry.panes.iter_mut().find(|p| p.pane_id == pane_id) {
                        pane.progress_state = state;
                        pane.progress = progress;
                        break;
                    }
                }
                events.push(SessionEvent::PaneProgressChanged {
                    pane_id,
                    state,
                    progress,
                });
            }

            ServerMessage::Event {
                event:
                    SessionEventMsg::PaneClipboardCopy {
                        pane_id,
                        selection,
                        data,
                    },
            } => {
                // Pure relay: the app layer applies the active-pane policy and
                // decodes the base64 payload at the clipboard leaf.
                events.push(SessionEvent::ClipboardCopy {
                    pane_id,
                    selection,
                    data,
                });
            }

            ServerMessage::Event {
                event:
                    SessionEventMsg::PaneAttention {
                        pane_id,
                        kind,
                        title,
                        body,
                        attention_id,
                    },
            } => {
                // The session word the GUI focuses on click; derive it from the
                // pane id (already local — federation rewrote it upstream).
                let word_id = kmux_protocol::pane_word(&pane_id)
                    .unwrap_or(&pane_id)
                    .to_string();
                events.push(SessionEvent::PaneAttention {
                    word_id,
                    pane_id,
                    kind,
                    title,
                    body,
                    attention_id,
                });
            }

            // A session was restored from the graveyard by some client. The
            // broadcast names only the word, so a client that does not already
            // have the entry re-lists; unlike the `SessionCreated` reply it must
            // not *switch* to it — that would yank the view of every other GUI.
            ServerMessage::Event {
                event: SessionEventMsg::SessionCreated { word_id },
            } => {
                let cached = self.knows_session(&word_id);
                self.resync_unless_cached(cached);
            }

            // A tab was created by some client. The broadcast carries the index
            // but no `TabInfo`, so the tree can only come from a fresh list.
            ServerMessage::Event {
                event: SessionEventMsg::TabCreated { word_id, tab_index },
            } => {
                let cached = self
                    .session_list
                    .iter()
                    .find(|e| e.meta.word_id == word_id)
                    .is_none_or(|e| e.tabs.iter().any(|t| t.tab_index == tab_index));
                self.resync_unless_cached(cached);
            }

            // A pane was spawned — by a session/tab create, a split, or a
            // restore. Same shape: an id, no `PaneInfo`, no layout.
            ServerMessage::Event {
                event: SessionEventMsg::PaneSpawned { pane_id },
            } => {
                let cached = kmux_protocol::pane_word(&pane_id)
                    .is_none_or(|word_id| !self.knows_session(word_id))
                    || self.knows_pane(&pane_id);
                self.resync_unless_cached(cached);
            }

            // The pane's child process exited on its own. The pane keeps its
            // slot in the layout tree until someone closes it, so this only
            // records the status.
            ServerMessage::Event {
                event:
                    SessionEventMsg::PaneExited {
                        pane_id,
                        code,
                        signal,
                    },
            } => {
                self.on_pane_exited(&pane_id, code, signal);
            }

            // The pane's isolated VT worker crashed (issue #126). The shell is
            // untouched and the daemon respawns the worker, which resyncs this
            // client with a fresh snapshot through the normal `TerminalSnapshot`
            // path — so no sync state is disturbed here, only the UI is told.
            ServerMessage::Event {
                event: SessionEventMsg::PaneFaulted { pane_id },
            } => {
                warn!(%pane_id, "pane VT worker faulted; the daemon is respawning it");
                self.status_msg = format!("Pane '{pane_id}' is recovering");
                events.push(SessionEvent::PaneFaulted { pane_id });
            }

            // Never sent: `kmuxd` constructs no `LayoutChanged`, and the
            // authoritative `LayoutUpdate` supersedes it. Kept as an arm rather
            // than a `..` catch-all so a new `SessionEventMsg` variant fails to
            // compile here instead of being silently dropped (docs/testing.md R4).
            ServerMessage::Event {
                event: SessionEventMsg::LayoutChanged { .. },
            } => {}

            ServerMessage::Lagged {
                pane_id,
                missed_count,
            } => {
                self.metrics.record_lag(&pane_id, missed_count);
                self.metrics.record_resync(&pane_id, "lagged");
                if let Some(grid) = self.buffers.get_mut(&pane_id) {
                    grid.clear();
                }
                self.in_flight_history_fetches.remove(&pane_id);
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
                self.status_msg = format!("Input lock denied on '{pane_id}' (held by {holder:?})");
                events.push(SessionEvent::InputLockDenied { pane_id, holder });
            }

            ServerMessage::InputLockReleased { pane_id } => {
                self.input_locked.insert(pane_id.clone(), false);
                self.status_msg = format!("Input lock released on '{pane_id}'");
                events.push(SessionEvent::InputLockReleased { pane_id });
            }

            ServerMessage::DirectoryListing {
                request_id,
                path,
                parent,
                entries,
                error,
            } => {
                // Drop stale replies: only the most recent request counts (the
                // user may have navigated again before this listing returned).
                if self.pending_dir_request != Some(request_id) {
                    return events;
                }
                self.pending_dir_request = None;
                self.dir_listing = Some(DirListing {
                    path,
                    parent,
                    entries,
                    error,
                });
                events.push(SessionEvent::DirectoryListed);
            }

            ServerMessage::ScrollbackAppend {
                pane_id,
                first_index,
                lines,
                seqno,
                sent_at_ms,
            } => {
                if !self.check_pane_sync(&pane_id, seqno) {
                    return events;
                }
                let start = Instant::now();
                if let Some(grid) = self.buffers.get_mut(&pane_id) {
                    grid.apply_scrollback_append(first_index, lines);
                }
                self.mark_synced(pane_id.clone(), seqno, start, sent_at_ms);
                self.maybe_fetch_history(&pane_id);
            }

            ServerMessage::HistoryLines {
                request_id,
                pane_id,
                first_index,
                lines,
                history_total,
                ..
            } => {
                if let Some(grid) = self.buffers.get_mut(&pane_id) {
                    grid.apply_history_lines(first_index, lines, history_total);
                }
                // Clear only if this reply matches the in-flight request for
                // this pane; otherwise a stale reply from a prior attach could
                // unblock a fresher request.
                if self
                    .in_flight_history_fetches
                    .get(&pane_id)
                    .is_some_and(|rid| *rid == request_id)
                {
                    self.in_flight_history_fetches.remove(&pane_id);
                }
                self.maybe_fetch_history(&pane_id);
            }

            ServerMessage::Ping { seq } => {
                self.send_ws(ClientMessage::Pong { seq });
            }

            // Response to a client-initiated Ping; the liveness tracker
            // also refreshes on this (in addition to the general
            // `observe_inbound` at the top of this fn) so outstanding
            // ping seqs are cleared.
            ServerMessage::Pong { seq } => {
                if let Some(rtt) = self.liveness.on_pong(seq, Instant::now()) {
                    let rtt_ms = rtt.as_secs_f64() * 1000.0;
                    self.metrics.observe_rtt(rtt_ms);
                    self.record_rtt_to_supervisor(rtt_ms);
                }
            }

            // Federation responses (issue #121). The local daemon sends these
            // after we issue `OpenPeer`/`ClosePeer` to federate a remote server.
            // The handshake challenge is consumed by the connection bootstrap; the
            // session manager never sees it in practice. Ignore for exhaustiveness.
            ServerMessage::AuthChallenge { .. } => {}

            ServerMessage::ClientListResult {
                word_id, clients, ..
            } => {
                self.client_list = clients;
                self.client_list_word = Some(word_id.clone());
                events.push(SessionEvent::ClientListReceived { word_id });
            }

            ServerMessage::ClientKicked {
                word_id, client_id, ..
            } => {
                // The daemon acks the kick and pushes no refreshed list, so the
                // connected-clients view would keep the row until its own ~1 Hz
                // poll came round — a visible lag that reads as a failed kick.
                // The ack is authoritative for the session it names.
                if self.client_list_word.as_deref() == Some(word_id.as_str()) {
                    self.client_list.retain(|c| c.client_id != client_id);
                }
                events.push(SessionEvent::ClientKicked { word_id, client_id });
            }

            ServerMessage::SessionKicked { word_id, by_label } => {
                warn!("kicked from session {word_id} by {by_label}");
                events.push(SessionEvent::KickedFromSession { word_id, by_label });
            }

            ServerMessage::PeerOpened { peer, .. } => {
                events.push(SessionEvent::PeerOpened { peer });
            }
            ServerMessage::PeerError { peer, reason, .. } => {
                events.push(SessionEvent::PeerError { peer, reason });
            }
            // A close ack needs no app-level reconciliation (the peer's sessions
            // simply stop appearing in the next `SessionList`).
            ServerMessage::PeerClosed { .. } => {}

            // Reply to `ClientMessage::Notify` (issue #169). Consumed directly by
            // the `kmux notify` CLI's own read loop, not the streaming session
            // manager — ignore here for exhaustiveness.
            ServerMessage::NotifyAccepted { .. } => {}

            // Replies to `ClientMessage::FetchLogs` (issue #187). Consumed by the
            // `kmux daemon logs --server` CLI read loop, never by the GUI session
            // manager — ignore here for exhaustiveness.
            ServerMessage::LogChunk { .. } | ServerMessage::LogEnd { .. } => {}
        }
        events
    }
}

/// Lifecycle reconciliation shared by a fact's reply form and its broadcast form.
///
/// The daemon reports every session/tab/pane mutation twice. The requesting
/// client gets a dedicated `ServerMessage` reply (`state.send`, one connection),
/// and *every* connected client — the requester included — gets the same fact as
/// a `SessionEventMsg` on the server-wide event channel: `ServerApp::broadcast`
/// (`kmuxd/src/app/mod.rs`) filters by neither session nor attachment, and the
/// PTY lifecycle bus (`kmuxd/src/client_handler/events.rs`) is fanned out the
/// same way. Before this existed the client handled only the reply forms, so a
/// tab, pane or session another GUI touched stayed in this client's cache until
/// an unrelated refresh happened by.
///
/// Each handler below is therefore reached from both arms and must be
/// idempotent. They are written as "reconcile only if the client still holds
/// state for the thing that changed", which makes the second delivery a no-op
/// and keeps the UI event exactly-once.
impl SessionManager {
    /// Whether the client still holds any state for `pane_id` — a decoded grid,
    /// sync bookkeeping, or a cached [`PaneInfo`].
    fn knows_pane(&self, pane_id: &str) -> bool {
        self.buffers.contains_key(pane_id)
            || self.pane_sync.contains_key(pane_id)
            || self
                .session_list
                .iter()
                .any(|e| e.panes.iter().any(|p| p.pane_id == pane_id))
    }

    /// Whether `word_id` is in the cached session list.
    fn knows_session(&self, word_id: &str) -> bool {
        self.session_list.iter().any(|e| e.meta.word_id == word_id)
    }

    /// Reconcile "the pane `pane_id` is gone": drop its buffers and bookkeeping,
    /// prune it from the cached session entry, and move focus off it.
    ///
    /// Returns `None` when the client never knew the pane, which is also what
    /// makes the requester's second delivery silent.
    fn on_pane_gone(&mut self, pane_id: String) -> Option<SessionEvent> {
        if !self.knows_pane(&pane_id) {
            return None;
        }
        self.forget_pane(&pane_id);
        for entry in &mut self.session_list {
            entry.panes.retain(|p| p.pane_id != pane_id);
        }
        if self.active_pane.as_deref() == Some(&pane_id) {
            match self.find_fallback_pane() {
                Some((word_id, pane)) => {
                    self.active_session = Some(word_id);
                    self.active_pane = Some(pane.clone());
                    self.attach_fresh(pane);
                }
                None => {
                    // Nothing left to show. The viewed tab and its attached set
                    // must go with the session, or the client keeps rendering a
                    // tab of a session it no longer considers active — and
                    // `visible_panes` keeps naming the pane just forgotten.
                    self.active_session = None;
                    self.active_pane = None;
                    self.active_tab = None;
                    self.visible_panes.clear();
                }
            }
        }
        Some(SessionEvent::PaneClosed { pane_id })
    }

    /// Reconcile "the session `word_id` is gone": forget every pane it owned,
    /// drop the entry, and fall back to another session when it was the one
    /// being viewed.
    fn on_session_gone(&mut self, word_id: String) -> Option<SessionEvent> {
        if !self.knows_session(&word_id) {
            return None;
        }
        for pane_id in self.session_pane_ids(&word_id) {
            self.forget_pane(&pane_id);
        }
        self.session_list.retain(|e| e.meta.word_id != word_id);

        if self.active_session.as_deref() == Some(&word_id) {
            // The closed session's panes are gone server-side; clear local
            // view state and fall back to the first remaining session.
            self.active_session = None;
            self.active_tab = None;
            self.active_pane = None;
            self.visible_panes.clear();
            if let Some(next) = self.session_list.first().map(|e| e.meta.word_id.clone()) {
                self.select_session(next);
            }
        }
        Some(SessionEvent::SessionClosed { word_id })
    }

    /// Reconcile "the tab `tab_index` of `word_id` is gone": drop it from the
    /// cached entry, forget the panes it alone owned, and move to another tab
    /// when it was the one being viewed.
    fn on_tab_gone(&mut self, word_id: &str, tab_index: u32) {
        let mut closed_panes = Vec::new();
        let mut next_tab = None;
        if let Some(entry) = self
            .session_list
            .iter_mut()
            .find(|e| e.meta.word_id == word_id)
        {
            if let Some(tab) = entry.tabs.iter().find(|t| t.tab_index == tab_index) {
                closed_panes = tab.layout.leaves();
            }
            entry.tabs.retain(|t| t.tab_index != tab_index);
            let live_panes = entry
                .tabs
                .iter()
                .flat_map(|t| t.layout.leaves())
                .collect::<HashSet<_>>();
            closed_panes.retain(|pane| !live_panes.contains(pane));
            entry
                .panes
                .retain(|pane| !closed_panes.contains(&pane.pane_index));
            if entry.active_tab == tab_index {
                entry.active_tab = entry.tabs.first().map_or(0, |t| t.tab_index);
            }
            next_tab = entry.tabs.first().map(|t| t.tab_index);
        }
        for pane_index in closed_panes {
            self.forget_pane(&kmux_protocol::format_pane_id(word_id, pane_index));
        }
        // If the closed tab was the one we were viewing, move to another.
        if self.active_session.as_deref() == Some(word_id) && self.active_tab == Some(tab_index) {
            self.active_tab = None;
            self.visible_panes.clear();
            match next_tab {
                Some(t) => self.select_tab(t),
                None => {
                    self.active_pane = None;
                }
            }
        }
    }

    /// Reconcile a *creation* broadcast.
    ///
    /// Unlike the reply forms, the broadcast forms carry an id and nothing else:
    /// `SessionEventMsg::TabCreated` has no `TabInfo` and `PaneSpawned` no
    /// `PaneInfo`, and the daemon sends no `LayoutUpdate` alongside either. A
    /// fresh session list is therefore the only reconciliation available, so it
    /// is requested exactly when the cache does not already show the fact —
    /// which is what keeps the requesting client, whose reply carried the whole
    /// record, from re-listing on its own change.
    fn resync_unless_cached(&mut self, already_cached: bool) {
        if !already_cached {
            self.request_session_list();
        }
    }

    /// Record that a pane's child process exited (the pane itself survives until
    /// someone closes it, and the daemon leaves it in the layout tree), so
    /// [`SessionManager::is_pane_running`] stops reporting it as live.
    fn on_pane_exited(&mut self, pane_id: &str, code: Option<i32>, signal: Option<i32>) {
        for entry in &mut self.session_list {
            if let Some(pane) = entry.panes.iter_mut().find(|p| p.pane_id == pane_id) {
                pane.status = SessionStatus::Exited { code, signal };
                break;
            }
        }
    }

    /// The pane ids the cached entry for `word_id` owns.
    fn session_pane_ids(&self, word_id: &str) -> Vec<String> {
        self.session_list
            .iter()
            .find(|e| e.meta.word_id == word_id)
            .map(|e| e.panes.iter().map(|p| p.pane_id.clone()).collect())
            .unwrap_or_default()
    }
}

impl SessionManager {
    /// Find the best fallback pane after the active pane closes.
    ///
    /// Priority: another pane in the same session → first pane of any session.
    /// Returns `(word_id, pane_id)` or `None` if no panes remain.
    fn find_fallback_pane(&self) -> Option<(String, String)> {
        // 1. Try another pane in the same session.
        if let Some(word_id) = &self.active_session
            && let Some(pane_id) = self
                .session_list
                .iter()
                .find(|e| e.meta.word_id == *word_id)
                .and_then(|e| e.panes.first())
                .map(|p| p.pane_id.clone())
        {
            return Some((word_id.clone(), pane_id));
        }
        // 2. Fall back to the first pane of any session.
        self.session_list.first().and_then(|e| {
            e.panes
                .first()
                .map(|p| (e.meta.word_id.clone(), p.pane_id.clone()))
        })
    }
}

#[cfg(test)]
mod tests {
    //! Characterization tests: one per arm of [`SessionManager::handle_server_message`].
    //!
    //! `handle_server_message` is a 753-line match over 47 `ServerMessage`
    //! variants, and cargo-mutants generates one body-replacement mutant per
    //! *function* — so the whole dispatcher yields exactly ONE mutant, which any
    //! single test kills. These tests pin what each arm does today (the returned
    //! `Vec<SessionEvent>` *and* the state it leaves behind) so the dispatcher can
    //! be split into per-domain handlers without changing behaviour.
    //!
    //! Every expectation here was recorded empirically, not designed. Each one
    //! that looked wrong was then checked against what `kmuxd` actually sends;
    //! the ones that survived that check became the `fix(client):` commits on
    //! this branch, and the ones that did not carry a comment naming the
    //! sender-side reason the behaviour is right as it stands.

    use kmux_protocol::messages::{
        ClientCapabilities, ClientInfo, ClosedSessionEntry, ConnectionId, CursorState,
        FrontendKind, LayoutNode, SessionEntry, SessionMeta, SplitDir, TabInfo, TermModes,
        TermSize, TransportKind,
    };
    use tokio::sync::mpsc;

    use super::*;
    use crate::grid::CellGrid;

    // ── Fixtures ────────────────────────────────────────────────────────────
    //
    // Deliberately local rather than shared with `session_manager/mod.rs`: this
    // commit is a pure addition, and R1 of docs/testing.md wants a subject's
    // tests in the subject's own file.

    fn make_manager() -> SessionManager {
        let mut mgr = SessionManager::new(
            "127.0.0.1".to_string(),
            8443,
            "test-token".to_string(),
            false,
            ClientCapabilities::default(),
        );
        // Tag a transport so the per-message inbound accounting every arm shares
        // is observable as a value (`inbound_msgs`).
        mgr.tag_transport(TransportKind::Uds);
        mgr
    }

    fn make_connected_manager() -> (SessionManager, mpsc::UnboundedReceiver<ClientMessage>) {
        let mut mgr = make_manager();
        let (tx, rx) = mpsc::unbounded_channel();
        mgr.ws_sender = Some(tx);
        mgr.connected = true;
        (mgr, rx)
    }

    fn pane(word_id: &str, index: u32) -> PaneInfo {
        PaneInfo {
            pane_id: kmux_protocol::format_pane_id(word_id, index),
            pane_index: index,
            program: String::new(),
            size: TermSize::default(),
            attached_clients: vec![],
            status: SessionStatus::Running,
            title: String::new(),
            progress_state: PaneProgressState::default(),
            progress: None,
        }
    }

    fn tab(tab_index: u32, layout: LayoutNode, focused_pane: u32) -> TabInfo {
        TabInfo {
            tab_index,
            name: format!("{}", tab_index + 1),
            layout,
            focused_pane,
        }
    }

    /// A session with one pane and one single-leaf tab.
    fn make_entry(word_id: &str) -> SessionEntry {
        SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: word_id.to_string(),
                name: word_id.to_string(),
                cwd: "/tmp".to_string(),
            },
            panes: vec![pane(word_id, 0)],
            tabs: vec![tab(0, LayoutNode::single(0), 0)],
            active_tab: 0,
            peer: None,
        }
    }

    /// A manager holding `word_id` as its selected session, with the session's
    /// only pane focused. The outbound receiver is returned so attach/detach
    /// traffic an arm generates can be asserted on.
    fn manager_on(word_id: &str) -> (SessionManager, mpsc::UnboundedReceiver<ClientMessage>) {
        let (mut mgr, rx) = make_connected_manager();
        mgr.session_list.push(make_entry(word_id));
        mgr.select_session(word_id.to_string());
        (mgr, rx)
    }

    /// Park `pane_id` in `Synced { expected: seqno }` with a local grid, which is
    /// the precondition every seqno-carrying arm checks.
    fn synced_pane(mgr: &mut SessionManager, pane_id: &str, expected: u64) {
        mgr.buffers.insert(pane_id.to_string(), CellGrid::new(4, 8));
        mgr.pane_sync.insert(
            pane_id.to_string(),
            PaneSync::Synced {
                expected: SequenceNo(expected),
            },
        );
    }

    /// An empty scrollback line (`Arc<[CellState]>`), the wire's line type.
    fn sb_line() -> kmux_protocol::messages::ScrollbackLine {
        Arc::from(Vec::new())
    }

    /// The pane's next-expected seqno, or `None` when it is awaiting a resync.
    fn expected_seqno(mgr: &SessionManager, pane_id: &str) -> Option<u64> {
        match mgr.pane_sync.get(pane_id) {
            Some(PaneSync::Synced { expected }) => Some(expected.0),
            _ => None,
        }
    }

    fn awaiting_sync(mgr: &SessionManager, pane_id: &str) -> bool {
        matches!(mgr.pane_sync.get(pane_id), Some(PaneSync::AwaitingSync))
    }

    /// Total inbound messages accounted to the tagged transport. Every arm
    /// shares the accounting at the top of the dispatcher, so this is the value
    /// that proves a "does nothing" arm still ran.
    fn inbound_msgs(mgr: &SessionManager) -> u64 {
        mgr.metrics
            .network
            .snapshot_by_transport()
            .iter()
            .map(|(_, c)| c.msgs_in)
            .sum()
    }

    fn resyncs(mgr: &SessionManager) -> u64 {
        mgr.metrics.snapshot(false).counters.resyncs
    }

    fn lag_events(mgr: &SessionManager) -> u64 {
        mgr.metrics.snapshot(false).counters.lag_events
    }

    fn stale_discards(mgr: &SessionManager) -> u64 {
        mgr.metrics.snapshot(false).counters.stale_discards
    }

    /// Everything an arm could observably touch, as one comparable value — so an
    /// arm that does nothing is pinned by an `assert_eq!` rather than by the
    /// absence of assertions (R2).
    #[derive(Debug, PartialEq, Eq)]
    struct Observable {
        sessions: Vec<String>,
        closed_sessions: usize,
        process_overview: usize,
        client_list: usize,
        client_list_word: Option<String>,
        status_msg: String,
        active_session: Option<String>,
        active_tab: Option<u32>,
        active_pane: Option<String>,
        visible_panes: Vec<String>,
        buffers: Vec<String>,
        attention_panes: Vec<String>,
        input_locked: Vec<(String, bool)>,
        dir_listing_path: Option<String>,
    }

    fn observe(mgr: &SessionManager) -> Observable {
        let mut buffers: Vec<String> = mgr.buffers.keys().cloned().collect();
        buffers.sort();
        let mut attention_panes: Vec<String> = mgr.attention_panes.iter().cloned().collect();
        attention_panes.sort();
        let mut input_locked: Vec<(String, bool)> = mgr
            .input_locked
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        input_locked.sort();
        Observable {
            sessions: mgr
                .session_list
                .iter()
                .map(|e| e.meta.word_id.clone())
                .collect(),
            closed_sessions: mgr.closed_sessions.len(),
            process_overview: mgr.process_overview.len(),
            client_list: mgr.client_list.len(),
            client_list_word: mgr.client_list_word.clone(),
            status_msg: mgr.status_msg.clone(),
            active_session: mgr.active_session.clone(),
            active_tab: mgr.active_tab,
            active_pane: mgr.active_pane.clone(),
            visible_panes: mgr.visible_panes.clone(),
            buffers,
            attention_panes,
            input_locked,
            dir_listing_path: mgr.dir_listing.as_ref().map(|d| d.path.clone()),
        }
    }

    /// Every outbound message queued so far, in order.
    fn drain(rx: &mut mpsc::UnboundedReceiver<ClientMessage>) -> Vec<ClientMessage> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            out.push(msg);
        }
        out
    }

    // ── ChannelSwitched ─────────────────────────────────────────────────────

    #[test]
    fn channel_switched_only_logs_and_changes_no_client_state() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);
        let before = observe(&mgr);
        let transport_before = mgr.current_transport;

        let events = mgr.handle_server_message(ServerMessage::ChannelSwitched {
            old_transport: "quic".to_string(),
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(observe(&mgr), before, "the arm is log-only");
        assert_eq!(mgr.current_transport, transport_before);
        // SUSPECT: `ChannelSwitched`'s protocol doc says "the client should close
        // the old transport after receiving this", but the arm neither closes it
        // nor tells anyone to — nothing is sent and no state moves. The old
        // channel is only dropped if `apply_transport_upgrade` already replaced
        // the sender, which this message cannot verify.
        assert!(drain(&mut rx).is_empty(), "nothing is sent in reply");
        assert_eq!(inbound_msgs(&mgr), 1, "the frame is still accounted");
    }

    // ── ClosedSessionListResult ─────────────────────────────────────────────

    #[test]
    fn closed_session_list_result_replaces_the_graveyard_and_emits_nothing() {
        let mut mgr = make_manager();
        mgr.closed_sessions.push(ClosedSessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: "stale".to_string(),
                name: "stale".to_string(),
                cwd: "/tmp".to_string(),
            },
            last_active_ms: 1,
            closed_at_ms: 2,
            pane_count: 1,
        });

        let events = mgr.handle_server_message(ServerMessage::ClosedSessionListResult {
            request_id: 7,
            sessions: vec![ClosedSessionEntry {
                meta: SessionMeta {
                    index: 0,
                    word_id: "otter".to_string(),
                    name: "otter".to_string(),
                    cwd: "/home".to_string(),
                },
                last_active_ms: 10,
                closed_at_ms: 20,
                pane_count: 3,
            }],
        });

        // SUSPECT: the graveyard is replaced but no `SessionEvent` is emitted, so
        // a caller that issued `request_closed_sessions` has nothing to react to
        // — unlike `SessionListResult` and `ProcessOverviewResult`, which both
        // emit a "…Received" event. The launcher has to poll.
        assert!(events.is_empty(), "no UI event: {events:?}");
        let closed = mgr.closed_session_list();
        assert_eq!(closed.len(), 1, "the previous list is replaced, not merged");
        assert_eq!(closed[0].meta.word_id, "otter");
        assert_eq!(closed[0].pane_count, 3);
    }

    // ── PaneClosed ──────────────────────────────────────────────────────────

    #[test]
    fn pane_closed_forgets_the_pane_and_emits_pane_closed() {
        let (mut mgr, _rx) = manager_on("eagle");
        synced_pane(&mut mgr, "eagle/0", 5);
        mgr.input_locked.insert("eagle/0".to_string(), true);
        mgr.in_flight_history_fetches
            .insert("eagle/0".to_string(), 3);
        mgr.active_pane = None;

        let events = mgr.handle_server_message(ServerMessage::PaneClosed {
            request_id: 1,
            pane_id: "eagle/0".to_string(),
            exit_code: Some(0),
        });

        assert!(
            matches!(events.as_slice(), [SessionEvent::PaneClosed { pane_id }] if pane_id == "eagle/0"),
            "{events:?}"
        );
        assert!(mgr.buffer("eagle/0").is_none(), "the buffer is dropped");
        assert!(!mgr.pane_sync.contains_key("eagle/0"));
        assert!(!mgr.is_input_locked("eagle/0"));
        assert!(!mgr.in_flight_history_fetches.contains_key("eagle/0"));
        assert!(
            mgr.session_list[0].panes.is_empty(),
            "the pane leaves the cached session entry"
        );
        // Only `panes` is pruned; the tree still names pane index 0. That is
        // correct: the tree is the daemon's to own, and `on_pane_close`
        // (kmuxd/src/client_handler/dispatch/pane.rs) always follows a
        // `PaneClosed` with exactly one of `LayoutUpdate` (the tab survives),
        // `Event{TabClosed}` or `Event{SessionClosed}` — see the follow-up
        // assertion below. Deriving a new tree here would only race the
        // authoritative one.
        assert_eq!(mgr.session_list[0].tabs[0].layout.leaves(), vec![0]);
        assert_eq!(mgr.visible_panes(), ["eagle/0"]);

        // This pane was the session's last, so the daemon's follow-up is
        // `Event{SessionClosed}`, and that is what clears the stale tree.
        mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::SessionClosed {
                word_id: "eagle".to_string(),
            },
        });
        assert!(mgr.session_list().is_empty());
        assert!(mgr.visible_panes().is_empty());
    }

    #[test]
    fn pane_closed_of_the_active_pane_falls_back_to_a_surviving_pane() {
        let (mut mgr, mut rx) = make_connected_manager();
        let mut entry = make_entry("eagle");
        entry.panes.push(pane("eagle", 1));
        entry.tabs[0].layout = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![500, 500],
            children: vec![LayoutNode::single(0), LayoutNode::single(1)],
        };
        mgr.session_list.push(entry);
        mgr.select_session("eagle".to_string());
        assert_eq!(mgr.active_pane_id(), Some("eagle/0"));
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::PaneClosed {
            request_id: 2,
            pane_id: "eagle/0".to_string(),
            exit_code: None,
        });

        assert!(
            matches!(events.as_slice(), [SessionEvent::PaneClosed { pane_id }] if pane_id == "eagle/0"),
            "{events:?}"
        );
        assert_eq!(mgr.active_session(), Some("eagle"));
        assert_eq!(mgr.active_pane_id(), Some("eagle/1"));
        assert!(
            awaiting_sync(&mgr, "eagle/1"),
            "the fallback pane is re-attached fresh"
        );
        assert!(
            drain(&mut rx).iter().any(
                |m| matches!(m, ClientMessage::Attach { pane_id, .. } if pane_id == "eagle/1")
            ),
            "the fallback pane is attached on the wire"
        );
    }

    #[test]
    fn pane_closed_of_the_only_pane_clears_the_active_session_and_pane() {
        let (mut mgr, _rx) = manager_on("eagle");

        let events = mgr.handle_server_message(ServerMessage::PaneClosed {
            request_id: 3,
            pane_id: "eagle/0".to_string(),
            exit_code: Some(1),
        });

        assert!(
            matches!(events.as_slice(), [SessionEvent::PaneClosed { pane_id }] if pane_id == "eagle/0"),
            "{events:?}"
        );
        assert_eq!(mgr.active_session(), None);
        assert_eq!(mgr.active_pane_id(), None);
        // The viewed tab and its attached set go with the session: nothing is
        // left to render, and `visible_panes` must not name the pane the same
        // message just forgot.
        assert_eq!(mgr.active_tab(), None);
        assert!(mgr.visible_panes().is_empty());
        assert!(mgr.render_layout().is_none(), "there is nothing to draw");
    }

    // ── TabCreated ──────────────────────────────────────────────────────────

    #[test]
    fn tab_created_in_the_active_session_appends_the_tab_and_selects_it() {
        let (mut mgr, mut rx) = manager_on("eagle");
        mgr.session_list[0].panes.push(pane("eagle", 1));
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::TabCreated {
            request_id: 4,
            word_id: "eagle".to_string(),
            tab: tab(1, LayoutNode::single(1), 1),
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(
            mgr.session_list[0]
                .tabs
                .iter()
                .map(|t| t.tab_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(mgr.active_tab(), Some(1));
        assert_eq!(mgr.visible_panes(), ["eagle/1"]);
        assert_eq!(mgr.active_pane_id(), Some("eagle/1"));
        let sent = drain(&mut rx);
        assert!(
            sent.iter()
                .any(|m| matches!(m, ClientMessage::Detach { pane_id } if pane_id == "eagle/0")),
            "the old tab's pane is detached: {sent:?}"
        );
        assert!(
            sent.iter().any(
                |m| matches!(m, ClientMessage::Attach { pane_id, .. } if pane_id == "eagle/1")
            ),
            "the new tab's pane is attached: {sent:?}"
        );
    }

    #[test]
    fn tab_created_for_an_unknown_session_is_dropped_entirely() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);
        let before = observe(&mgr);

        let events = mgr.handle_server_message(ServerMessage::TabCreated {
            request_id: 5,
            word_id: "nosuch".to_string(),
            tab: tab(9, LayoutNode::single(0), 0),
        });

        // SUSPECT: an unknown session silently discards the tab — no error, no
        // event, no session-list refresh to reconcile against. The client's cache
        // stays permanently short of a tab the daemon believes exists.
        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(observe(&mgr), before);
        assert!(drain(&mut rx).is_empty());
    }

    // ── PaneSplit ───────────────────────────────────────────────────────────

    #[test]
    fn pane_split_records_the_new_pane_and_focuses_it_in_the_active_tab() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::PaneSplit {
            request_id: 6,
            word_id: "eagle".to_string(),
            tab_index: 0,
            new_pane: pane("eagle", 1),
            layout: LayoutNode::Split {
                dir: SplitDir::Vertical,
                ratios: vec![500, 500],
                children: vec![LayoutNode::single(0), LayoutNode::single(1)],
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(
            mgr.buffer("eagle/1").is_some(),
            "the new pane gets a buffer"
        );
        assert_eq!(
            mgr.session_list[0]
                .panes
                .iter()
                .map(|p| p.pane_id.clone())
                .collect::<Vec<_>>(),
            vec!["eagle/0".to_string(), "eagle/1".to_string()]
        );
        assert_eq!(mgr.session_list[0].tabs[0].focused_pane, 1);
        assert_eq!(mgr.visible_panes(), ["eagle/0", "eagle/1"]);
        assert_eq!(mgr.active_pane_id(), Some("eagle/1"));
        assert!(
            drain(&mut rx).iter().any(
                |m| matches!(m, ClientMessage::Attach { pane_id, .. } if pane_id == "eagle/1")
            ),
            "the sibling stays attached; only the new pane is attached"
        );
    }

    #[test]
    fn pane_split_for_an_unviewed_tab_updates_the_cache_without_retargeting_the_view() {
        let (mut mgr, mut rx) = manager_on("eagle");
        mgr.session_list[0]
            .tabs
            .push(tab(1, LayoutNode::single(1), 1));
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::PaneSplit {
            request_id: 7,
            word_id: "eagle".to_string(),
            tab_index: 1,
            new_pane: pane("eagle", 2),
            layout: LayoutNode::Split {
                dir: SplitDir::Vertical,
                ratios: vec![500, 500],
                children: vec![LayoutNode::single(1), LayoutNode::single(2)],
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(
            mgr.buffer("eagle/2").is_some(),
            "the buffer is created even for an unviewed tab"
        );
        assert_eq!(mgr.session_list[0].tabs[1].focused_pane, 2);
        assert_eq!(mgr.visible_panes(), ["eagle/0"], "the view is untouched");
        assert_eq!(mgr.active_pane_id(), Some("eagle/0"));
        assert!(drain(&mut rx).is_empty(), "no attach for an unviewed tab");
    }

    // ── CursorUpdate ────────────────────────────────────────────────────────

    #[test]
    fn cursor_update_applies_the_cursor_and_advances_the_expected_seqno() {
        let mut mgr = make_manager();
        synced_pane(&mut mgr, "eagle/0", 5);

        let events = mgr.handle_server_message(ServerMessage::CursorUpdate {
            pane_id: "eagle/0".to_string(),
            cursor: CursorState {
                row: 2,
                col: 3,
                ..Default::default()
            },
            modes: TermModes(TermModes::APP_CURSOR),
            seqno: SequenceNo(5),
            sent_at_ms: 1,
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        let grid = mgr.buffer("eagle/0").expect("buffer exists");
        assert_eq!((grid.cursor().row, grid.cursor().col), (2, 3));
        assert!(grid.app_cursor(), "modes are applied alongside the cursor");
        assert_eq!(expected_seqno(&mgr, "eagle/0"), Some(6));
    }

    #[test]
    fn cursor_update_for_a_pane_awaiting_sync_is_discarded_and_counted() {
        let mut mgr = make_manager();
        mgr.buffers
            .insert("eagle/0".to_string(), CellGrid::new(4, 8));
        mgr.pane_sync
            .insert("eagle/0".to_string(), PaneSync::AwaitingSync);

        let events = mgr.handle_server_message(ServerMessage::CursorUpdate {
            pane_id: "eagle/0".to_string(),
            cursor: CursorState {
                row: 2,
                col: 3,
                ..Default::default()
            },
            modes: TermModes::EMPTY,
            seqno: SequenceNo(5),
            sent_at_ms: 1,
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(stale_discards(&mgr), 1, "the discard is counted");
        let grid = mgr.buffer("eagle/0").expect("buffer exists");
        assert_eq!((grid.cursor().row, grid.cursor().col), (0, 0));
        assert!(
            awaiting_sync(&mgr, "eagle/0"),
            "still awaiting the snapshot"
        );
    }

    #[test]
    fn cursor_update_with_a_seqno_gap_resyncs_the_pane() {
        let (mut mgr, mut rx) = make_connected_manager();
        synced_pane(&mut mgr, "eagle/0", 5);

        let events = mgr.handle_server_message(ServerMessage::CursorUpdate {
            pane_id: "eagle/0".to_string(),
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            seqno: SequenceNo(9),
            sent_at_ms: 1,
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(resyncs(&mgr), 1);
        assert!(awaiting_sync(&mgr, "eagle/0"));
        assert!(
            drain(&mut rx)
                .iter()
                .any(|m| matches!(m, ClientMessage::Attach { pane_id, last_seqno: None, .. } if pane_id == "eagle/0")),
            "a gap re-attaches from scratch"
        );
    }

    // ── SyncReset ───────────────────────────────────────────────────────────

    #[test]
    fn sync_reset_clears_the_grid_and_parks_the_pane_awaiting_sync() {
        let (mut mgr, mut rx) = make_connected_manager();
        synced_pane(&mut mgr, "eagle/0", 5);
        mgr.in_flight_history_fetches
            .insert("eagle/0".to_string(), 11);
        mgr.buffers
            .get_mut("eagle/0")
            .expect("buffer exists")
            .apply_cursor_update(
                CursorState {
                    row: 3,
                    col: 3,
                    ..Default::default()
                },
                TermModes::EMPTY,
            );

        let events = mgr.handle_server_message(ServerMessage::SyncReset {
            pane_id: "eagle/0".to_string(),
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(awaiting_sync(&mgr, "eagle/0"));
        assert_eq!(resyncs(&mgr), 1);
        assert!(!mgr.in_flight_history_fetches.contains_key("eagle/0"));
        let grid = mgr.buffer("eagle/0").expect("buffer exists");
        assert_eq!((grid.cursor().row, grid.cursor().col), (0, 0));
        // SUSPECT: unlike `Lagged` and the digest-mismatch path, `SyncReset` does
        // not re-attach — it only parks the pane. The client sits in
        // `AwaitingSync` (discarding everything) until the server volunteers a
        // snapshot; if the server's `SyncReset` was not followed by one, the pane
        // is dark forever with no client-side recovery.
        assert!(drain(&mut rx).is_empty(), "nothing is sent in reply");
    }

    // ── SessionRenamed (event form and message form share one arm) ──────────

    #[test]
    fn session_renamed_event_updates_the_cached_name_and_emits_session_renamed() {
        let (mut mgr, _rx) = manager_on("eagle");

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::SessionRenamed {
                word_id: "eagle".to_string(),
                new_name: "builds".to_string(),
            },
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::SessionRenamed { word_id, new_name }]
                    if word_id == "eagle" && new_name == "builds"
            ),
            "{events:?}"
        );
        assert_eq!(mgr.session_list[0].meta.name, "builds");
    }

    #[test]
    fn session_renamed_message_takes_the_same_path_as_the_event() {
        let (mut mgr, _rx) = manager_on("eagle");

        let events = mgr.handle_server_message(ServerMessage::SessionRenamed {
            word_id: "eagle".to_string(),
            new_name: "builds".to_string(),
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::SessionRenamed { word_id, new_name }]
                    if word_id == "eagle" && new_name == "builds"
            ),
            "{events:?}"
        );
        assert_eq!(mgr.session_list[0].meta.name, "builds");
    }

    #[test]
    fn session_renamed_for_an_unknown_session_still_emits_the_rename() {
        let (mut mgr, _rx) = manager_on("eagle");

        let events = mgr.handle_server_message(ServerMessage::SessionRenamed {
            word_id: "nosuch".to_string(),
            new_name: "builds".to_string(),
        });

        // SUSPECT: a rename of a session the client has never heard of is
        // reported to the UI as a successful rename, indistinguishable from a
        // real one, while no cached name changed.
        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::SessionRenamed { word_id, .. }] if word_id == "nosuch"
            ),
            "{events:?}"
        );
        assert_eq!(mgr.session_list[0].meta.name, "eagle");
    }

    // ── Event::TabRenamed ───────────────────────────────────────────────────

    #[test]
    fn tab_renamed_event_updates_the_cached_tab_name_and_emits_nothing() {
        let (mut mgr, _rx) = manager_on("eagle");
        assert_eq!(mgr.session_list[0].tabs[0].name, "1");

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::TabRenamed {
                word_id: "eagle".to_string(),
                tab_index: 0,
                name: "logs".to_string(),
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(mgr.session_list[0].tabs[0].name, "logs");
        assert_eq!(mgr.active_tab_name().as_deref(), Some("logs"));
    }

    #[test]
    fn tab_renamed_event_for_an_unknown_tab_changes_nothing() {
        let (mut mgr, _rx) = manager_on("eagle");
        let before = observe(&mgr);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::TabRenamed {
                word_id: "eagle".to_string(),
                tab_index: 99,
                name: "logs".to_string(),
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(observe(&mgr), before);
        assert_eq!(mgr.session_list[0].tabs[0].name, "1");
    }

    // ── Event::TabsReordered ────────────────────────────────────────────────

    #[test]
    fn tabs_reordered_event_sorts_the_cached_tabs_into_the_given_order() {
        let (mut mgr, _rx) = manager_on("eagle");
        mgr.session_list[0]
            .tabs
            .push(tab(1, LayoutNode::single(1), 1));
        mgr.session_list[0]
            .tabs
            .push(tab(2, LayoutNode::single(2), 2));

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::TabsReordered {
                word_id: "eagle".to_string(),
                tab_indices: vec![2, 0, 1],
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(
            mgr.session_list[0]
                .tabs
                .iter()
                .map(|t| t.tab_index)
                .collect::<Vec<_>>(),
            vec![2, 0, 1]
        );
        assert_eq!(mgr.active_tab(), Some(0), "the viewed tab is unchanged");
    }

    #[test]
    fn tabs_reordered_event_pushes_an_unlisted_tab_to_the_end() {
        let (mut mgr, _rx) = manager_on("eagle");
        mgr.session_list[0]
            .tabs
            .push(tab(1, LayoutNode::single(1), 1));
        mgr.session_list[0]
            .tabs
            .push(tab(2, LayoutNode::single(2), 2));

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::TabsReordered {
                word_id: "eagle".to_string(),
                tab_indices: vec![2, 0],
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        // SUSPECT: a tab missing from the daemon's order sorts to `usize::MAX`
        // and silently moves to the end rather than being treated as a stale
        // broadcast the client should ignore or refresh from.
        assert_eq!(
            mgr.session_list[0]
                .tabs
                .iter()
                .map(|t| t.tab_index)
                .collect::<Vec<_>>(),
            vec![2, 0, 1]
        );
    }

    // ── Event::PaneResized ──────────────────────────────────────────────────

    #[test]
    fn pane_resized_event_resizes_the_buffer_and_emits_nothing() {
        let mut mgr = make_manager();
        mgr.buffers
            .insert("eagle/0".to_string(), CellGrid::new(4, 8));

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneResized {
                pane_id: "eagle/0".to_string(),
                size: TermSize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        let grid = mgr.buffer("eagle/0").expect("buffer exists");
        assert_eq!((grid.rows, grid.cols), (40, 120));
    }

    #[test]
    fn pane_resized_event_for_an_unbuffered_pane_creates_nothing() {
        let mut mgr = make_manager();

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneResized {
                pane_id: "eagle/0".to_string(),
                size: TermSize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(mgr.buffer("eagle/0").is_none(), "no buffer is conjured");
        assert_eq!(inbound_msgs(&mgr), 1);
    }

    // ── Event::PaneTitleChanged ─────────────────────────────────────────────

    #[test]
    fn pane_title_changed_event_caches_the_title_and_emits_it() {
        let (mut mgr, _rx) = manager_on("eagle");

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneTitleChanged {
                pane_id: "eagle/0".to_string(),
                title: "vim README.md".to_string(),
            },
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::PaneTitleChanged { pane_id, title }]
                    if pane_id == "eagle/0" && title == "vim README.md"
            ),
            "{events:?}"
        );
        assert_eq!(
            mgr.pane_info("eagle/0").map(|p| p.title.as_str()),
            Some("vim README.md")
        );
    }

    #[test]
    fn pane_title_changed_event_for_an_unknown_pane_still_emits_the_title() {
        let (mut mgr, _rx) = manager_on("eagle");

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneTitleChanged {
                pane_id: "nosuch/0".to_string(),
                title: "vim".to_string(),
            },
        });

        // SUSPECT: nothing is cached (there is no such pane) yet the frontend is
        // told a title changed, so a stale broadcast can retitle whatever the UI
        // happens to key the event to.
        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::PaneTitleChanged { pane_id, .. }] if pane_id == "nosuch/0"
            ),
            "{events:?}"
        );
        assert_eq!(mgr.pane_info("eagle/0").map(|p| p.title.as_str()), Some(""));
    }

    // ── Event::PaneBell ─────────────────────────────────────────────────────

    #[test]
    fn pane_bell_event_marks_the_pane_for_attention_and_emits_pane_bell() {
        let (mut mgr, _rx) = manager_on("eagle");
        assert!(!mgr.pane_needs_attention("eagle/0"));

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneBell {
                pane_id: "eagle/0".to_string(),
            },
        });

        assert!(
            matches!(events.as_slice(), [SessionEvent::PaneBell { pane_id }] if pane_id == "eagle/0"),
            "{events:?}"
        );
        assert!(mgr.pane_needs_attention("eagle/0"));
    }

    // ── Event::PaneProgressChanged ──────────────────────────────────────────

    #[test]
    fn pane_progress_changed_event_caches_the_state_and_emits_it() {
        let (mut mgr, _rx) = manager_on("eagle");

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneProgressChanged {
                pane_id: "eagle/0".to_string(),
                state: PaneProgressState::Set,
                progress: Some(42),
            },
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::PaneProgressChanged { pane_id, state, progress }]
                    if pane_id == "eagle/0"
                        && *state == PaneProgressState::Set
                        && *progress == Some(42)
            ),
            "{events:?}"
        );
        let info = mgr.pane_info("eagle/0").expect("pane is cached");
        assert_eq!(info.progress_state, PaneProgressState::Set);
        assert_eq!(info.progress, Some(42));
    }

    // ── Event::PaneClipboardCopy ────────────────────────────────────────────

    #[test]
    fn pane_clipboard_copy_event_relays_the_payload_undecoded_and_uncached() {
        let (mut mgr, _rx) = manager_on("eagle");
        let before = observe(&mgr);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneClipboardCopy {
                pane_id: "eagle/0".to_string(),
                selection: "c".to_string(),
                data: "aGVsbG8=".to_string(),
            },
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::ClipboardCopy { pane_id, selection, data }]
                    if pane_id == "eagle/0" && selection == "c" && data == "aGVsbG8="
            ),
            "the base64 payload is relayed verbatim: {events:?}"
        );
        assert_eq!(observe(&mgr), before, "a pure relay caches nothing");
    }

    // ── Event::PaneAttention ────────────────────────────────────────────────

    #[test]
    fn pane_attention_event_derives_the_word_id_from_the_pane_id() {
        let (mut mgr, _rx) = manager_on("eagle");
        let before = observe(&mgr);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneAttention {
                pane_id: "eagle/0".to_string(),
                kind: AttentionKind::NeedsInput,
                title: "kmux".to_string(),
                body: "needs input".to_string(),
                attention_id: 77,
            },
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::PaneAttention {
                    word_id, pane_id, kind, title, body, attention_id,
                }] if word_id == "eagle"
                    && pane_id == "eagle/0"
                    && matches!(kind, AttentionKind::NeedsInput)
                    && title == "kmux"
                    && body == "needs input"
                    && *attention_id == 77
            ),
            "{events:?}"
        );
        // Unlike `PaneBell`, the arm marks nothing: the unread flag is set by the
        // app layer (`AppCore::handle_session_events` calls `mark_pane_attention`)
        // so the notification and the tab marker stay one decision.
        assert_eq!(observe(&mgr), before);
        assert!(!mgr.pane_needs_attention("eagle/0"));
    }

    #[test]
    fn pane_attention_with_an_unparsable_pane_id_uses_the_whole_id_as_the_word_id() {
        let (mut mgr, _rx) = manager_on("eagle");

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneAttention {
                pane_id: "garbage".to_string(),
                kind: AttentionKind::TurnDone,
                title: "t".to_string(),
                body: "b".to_string(),
                attention_id: 1,
            },
        });

        // SUSPECT: a malformed pane id yields `word_id == pane_id`, so the
        // frontend is asked to focus a session called "garbage" instead of the
        // event being rejected.
        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::PaneAttention { word_id, pane_id, .. }]
                    if word_id == "garbage" && pane_id == "garbage"
            ),
            "{events:?}"
        );
        assert_eq!(mgr.active_session(), Some("eagle"), "nothing is refocused");
    }

    // ── Event: the lifecycle broadcasts ─────────────────────────────────────
    //
    // The daemon reports every mutation twice — a reply to the requester, and a
    // `SessionEventMsg` to every connected client. These pin the broadcast half,
    // which is the only notice a GUI that did *not* make the request ever gets.

    #[test]
    fn a_pane_exited_event_marks_the_pane_not_running_without_forgetting_it() {
        let (mut mgr, _rx) = manager_on("eagle");
        synced_pane(&mut mgr, "eagle/0", 1);
        assert!(mgr.is_pane_running("eagle/0"));

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneExited {
                pane_id: "eagle/0".to_string(),
                code: Some(3),
                signal: None,
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(
            !mgr.is_pane_running("eagle/0"),
            "the exit status reaches the cached PaneInfo"
        );
        assert_eq!(
            mgr.pane_info("eagle/0").map(|p| p.status.clone()),
            Some(SessionStatus::Exited {
                code: Some(3),
                signal: None
            })
        );
        // The daemon leaves an exited pane in the layout tree until someone
        // closes it, so the client must not forget it either.
        assert!(mgr.buffer("eagle/0").is_some(), "the buffer survives");
        assert_eq!(mgr.visible_panes(), ["eagle/0"]);
    }

    #[test]
    fn a_pane_closed_event_forgets_the_pane_exactly_as_the_reply_does() {
        let (mut mgr, _rx) = manager_on("eagle");
        synced_pane(&mut mgr, "eagle/0", 1);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneClosed {
                pane_id: "eagle/0".to_string(),
            },
        });

        assert!(
            matches!(events.as_slice(), [SessionEvent::PaneClosed { pane_id }] if pane_id == "eagle/0"),
            "{events:?}"
        );
        assert!(mgr.buffer("eagle/0").is_none(), "the buffer is dropped");
        assert!(!mgr.pane_sync.contains_key("eagle/0"));
        assert!(mgr.session_list[0].panes.is_empty());
        assert_eq!(mgr.active_pane_id(), None);
    }

    #[test]
    fn the_second_delivery_of_a_pane_close_is_a_silent_no_op() {
        // `ServerApp::broadcast` excludes nobody, so the client that asked for
        // the close receives its reply AND the broadcast. The reconciliation
        // must not fire twice, or the UI is told a pane closed that it already
        // forgot — and `find_fallback_pane` would re-run against a stale focus.
        let (mut mgr, _rx) = manager_on("eagle");
        synced_pane(&mut mgr, "eagle/0", 1);

        let first = mgr.handle_server_message(ServerMessage::PaneClosed {
            request_id: 1,
            pane_id: "eagle/0".to_string(),
            exit_code: Some(0),
        });
        let after_first = observe(&mgr);
        let second = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneClosed {
                pane_id: "eagle/0".to_string(),
            },
        });

        assert_eq!(first.len(), 1, "the first delivery reports the close");
        assert!(second.is_empty(), "the second reports nothing: {second:?}");
        assert_eq!(observe(&mgr), after_first, "and changes nothing");
    }

    #[test]
    fn a_session_closed_event_drops_the_session_exactly_as_the_reply_does() {
        let (mut mgr, _rx) = manager_on("eagle");
        synced_pane(&mut mgr, "eagle/0", 1);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::SessionClosed {
                word_id: "eagle".to_string(),
            },
        });

        assert!(
            matches!(events.as_slice(), [SessionEvent::SessionClosed { word_id }] if word_id == "eagle"),
            "{events:?}"
        );
        assert!(mgr.session_list().is_empty(), "the entry is dropped");
        assert!(mgr.buffer("eagle/0").is_none(), "its panes are forgotten");
        assert_eq!(mgr.active_session(), None);
        assert_eq!(mgr.active_tab(), None);
        assert!(mgr.visible_panes().is_empty());
    }

    #[test]
    fn a_session_closed_event_for_an_unknown_session_reports_nothing() {
        let (mut mgr, _rx) = manager_on("eagle");
        let before = observe(&mgr);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::SessionClosed {
                word_id: "nosuch".to_string(),
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(observe(&mgr), before);
    }

    #[test]
    fn a_tab_closed_event_prunes_the_tab_and_moves_the_view_off_it() {
        let (mut mgr, _rx) = manager_on("eagle");
        mgr.session_list[0].panes.push(pane("eagle", 1));
        mgr.session_list[0]
            .tabs
            .push(tab(1, LayoutNode::single(1), 1));
        mgr.select_tab(1);
        assert_eq!(mgr.active_tab(), Some(1));

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::TabClosed {
                word_id: "eagle".to_string(),
                tab_index: 1,
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(
            mgr.session_list[0]
                .tabs
                .iter()
                .map(|t| t.tab_index)
                .collect::<Vec<_>>(),
            vec![0]
        );
        assert!(
            mgr.buffer("eagle/1").is_none(),
            "the tab's only pane is forgotten"
        );
        assert_eq!(mgr.active_tab(), Some(0), "the view moves to the survivor");
        assert_eq!(mgr.visible_panes(), ["eagle/0"]);
    }

    #[test]
    fn a_tab_created_event_re_lists_because_the_broadcast_carries_no_layout() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::TabCreated {
                word_id: "eagle".to_string(),
                tab_index: 1,
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(
            drain(&mut rx)
                .iter()
                .any(|m| matches!(m, ClientMessage::SessionList { .. })),
            "the tree can only come from a fresh session list"
        );
        assert_eq!(
            mgr.active_tab(),
            Some(0),
            "another client's new tab does not yank this client's view"
        );
    }

    #[test]
    fn a_tab_created_event_for_a_tab_already_cached_sends_nothing() {
        // The requesting client got the whole `TabInfo` in its reply and then
        // receives the broadcast too; it must not re-list on its own change.
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::TabCreated {
                word_id: "eagle".to_string(),
                tab_index: 0,
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(drain(&mut rx).is_empty(), "no redundant refresh");
    }

    #[test]
    fn a_tab_created_event_for_an_untracked_session_sends_nothing() {
        // Broadcasts are server-wide, so events arrive for sessions this client
        // has never listed. Re-listing on each would be a refresh storm.
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::TabCreated {
                word_id: "nosuch".to_string(),
                tab_index: 0,
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_pane_spawned_event_in_a_tracked_session_re_lists() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneSpawned {
                pane_id: "eagle/1".to_string(),
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(
            drain(&mut rx)
                .iter()
                .any(|m| matches!(m, ClientMessage::SessionList { .. })),
            "the new pane's layout can only come from a fresh session list"
        );
    }

    #[test]
    fn a_pane_spawned_event_for_a_cached_pane_sends_nothing() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneSpawned {
                pane_id: "eagle/0".to_string(),
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(drain(&mut rx).is_empty(), "no redundant refresh");
    }

    #[test]
    fn a_session_created_event_re_lists_without_switching_to_it() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::SessionCreated {
                word_id: "otter".to_string(),
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(
            drain(&mut rx)
                .iter()
                .any(|m| matches!(m, ClientMessage::SessionList { .. })),
            "a restored session surfaces via a fresh list"
        );
        assert_eq!(
            mgr.active_session(),
            Some("eagle"),
            "unlike the reply arm, the broadcast never switches the view"
        );
    }

    #[test]
    fn a_pane_faulted_event_reports_recovery_without_disturbing_sync_state() {
        // Issue #126: the shell survives and the daemon respawns the worker,
        // resyncing through the ordinary `TerminalSnapshot` path — so the arm
        // must not clear the grid or park the pane, only tell the UI.
        let (mut mgr, mut rx) = manager_on("eagle");
        synced_pane(&mut mgr, "eagle/0", 7);
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneFaulted {
                pane_id: "eagle/0".to_string(),
            },
        });

        assert!(
            matches!(events.as_slice(), [SessionEvent::PaneFaulted { pane_id }] if pane_id == "eagle/0"),
            "{events:?}"
        );
        assert_eq!(mgr.status_msg(), "Pane 'eagle/0' is recovering");
        assert_eq!(
            expected_seqno(&mgr, "eagle/0"),
            Some(7),
            "sync state is untouched"
        );
        assert!(mgr.buffer("eagle/0").is_some(), "the grid is not cleared");
        assert!(drain(&mut rx).is_empty(), "nothing is sent in reply");
    }

    #[test]
    fn a_layout_changed_event_is_the_only_ignored_broadcast() {
        // `kmuxd` never constructs it; the authoritative `LayoutUpdate`
        // supersedes it. Pinned so the arm is not mistaken for dead weight.
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);
        let before = observe(&mgr);

        let events = mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::LayoutChanged {
                word_id: "eagle".to_string(),
                tab_index: 0,
            },
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(observe(&mgr), before);
        assert!(drain(&mut rx).is_empty());
        assert_eq!(inbound_msgs(&mgr), 1, "the frame is still accounted");
    }

    // ── Lagged ──────────────────────────────────────────────────────────────

    #[test]
    fn lagged_clears_the_grid_counts_the_lag_and_reattaches_the_pane() {
        let (mut mgr, mut rx) = make_connected_manager();
        synced_pane(&mut mgr, "eagle/0", 5);
        mgr.in_flight_history_fetches
            .insert("eagle/0".to_string(), 4);
        mgr.buffers
            .get_mut("eagle/0")
            .expect("buffer exists")
            .apply_cursor_update(
                CursorState {
                    row: 2,
                    col: 2,
                    ..Default::default()
                },
                TermModes::EMPTY,
            );

        let events = mgr.handle_server_message(ServerMessage::Lagged {
            pane_id: "eagle/0".to_string(),
            missed_count: 12,
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(lag_events(&mgr), 1);
        assert_eq!(resyncs(&mgr), 1);
        assert!(awaiting_sync(&mgr, "eagle/0"));
        assert!(!mgr.in_flight_history_fetches.contains_key("eagle/0"));
        let grid = mgr.buffer("eagle/0").expect("buffer exists");
        assert_eq!((grid.cursor().row, grid.cursor().col), (0, 0));
        assert!(
            drain(&mut rx).iter().any(
                |m| matches!(m, ClientMessage::Attach { pane_id, .. } if pane_id == "eagle/0")
            ),
            "the pane is re-attached"
        );
    }

    // ── Error ───────────────────────────────────────────────────────────────

    #[test]
    fn error_sets_the_status_message_and_emits_server_error() {
        let mut mgr = make_manager();

        let events = mgr.handle_server_message(ServerMessage::Error {
            request_id: Some(3),
            code: kmux_protocol::messages::ErrorCode::SessionNotFound,
            message: "session not found: nosuch".to_string(),
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::ServerError { message }] if message == "session not found: nosuch"
            ),
            "{events:?}"
        );
        assert_eq!(mgr.status_msg(), "Error: session not found: nosuch");
        // SUSPECT: neither the `request_id` nor the typed `ErrorCode` survives —
        // the arm discards both, so no caller can correlate an error with the
        // request that caused it or branch on the code.
    }

    // ── InputLock{Granted,Denied,Released} ──────────────────────────────────

    #[test]
    fn input_lock_granted_marks_the_pane_locked_and_reports_it_in_the_status() {
        let (mut mgr, _rx) = manager_on("eagle");

        let events = mgr.handle_server_message(ServerMessage::InputLockGranted {
            pane_id: "eagle/0".to_string(),
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::InputLockGranted { pane_id }] if pane_id == "eagle/0"
            ),
            "{events:?}"
        );
        assert!(mgr.is_input_locked("eagle/0"));
        assert!(mgr.active_input_locked());
        assert_eq!(mgr.status_msg(), "Input lock acquired on 'eagle/0'");
    }

    #[test]
    fn input_lock_denied_reports_the_holder_without_recording_a_lock() {
        let (mut mgr, _rx) = manager_on("eagle");

        let events = mgr.handle_server_message(ServerMessage::InputLockDenied {
            pane_id: "eagle/0".to_string(),
            holder: ClientId(9),
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::InputLockDenied { pane_id, holder }]
                    if pane_id == "eagle/0" && holder.0 == 9
            ),
            "{events:?}"
        );
        assert!(
            !mgr.is_input_locked("eagle/0"),
            "a denial records no local lock"
        );
        assert_eq!(
            mgr.status_msg(),
            "Input lock denied on 'eagle/0' (held by ClientId(9))"
        );
        // SUSPECT: the status line is built with `{holder:?}`, so the user is
        // shown the Rust debug form `ClientId(9)` rather than the holder's label.
    }

    #[test]
    fn input_lock_released_clears_the_lock_flag_and_reports_it_in_the_status() {
        let (mut mgr, _rx) = manager_on("eagle");
        mgr.input_locked.insert("eagle/0".to_string(), true);

        let events = mgr.handle_server_message(ServerMessage::InputLockReleased {
            pane_id: "eagle/0".to_string(),
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::InputLockReleased { pane_id }] if pane_id == "eagle/0"
            ),
            "{events:?}"
        );
        assert!(!mgr.is_input_locked("eagle/0"));
        assert_eq!(
            mgr.input_locked.get("eagle/0"),
            Some(&false),
            "the entry is set to false rather than removed"
        );
        assert_eq!(mgr.status_msg(), "Input lock released on 'eagle/0'");
    }

    // ── ScrollbackAppend ────────────────────────────────────────────────────

    #[test]
    fn scrollback_append_appends_the_lines_and_advances_the_expected_seqno() {
        let mut mgr = make_manager();
        synced_pane(&mut mgr, "eagle/0", 5);

        let events = mgr.handle_server_message(ServerMessage::ScrollbackAppend {
            pane_id: "eagle/0".to_string(),
            first_index: 0,
            lines: vec![sb_line(); 2],
            seqno: SequenceNo(5),
            sent_at_ms: 1,
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(expected_seqno(&mgr, "eagle/0"), Some(6));
        let grid = mgr.buffer("eagle/0").expect("buffer exists");
        assert_eq!(grid.scrollback().len(), 2);
        assert_eq!(grid.scrollback().history_total(), 2);
    }

    #[test]
    fn scrollback_append_for_a_pane_awaiting_sync_is_discarded() {
        let mut mgr = make_manager();
        mgr.buffers
            .insert("eagle/0".to_string(), CellGrid::new(4, 8));
        mgr.pane_sync
            .insert("eagle/0".to_string(), PaneSync::AwaitingSync);

        let events = mgr.handle_server_message(ServerMessage::ScrollbackAppend {
            pane_id: "eagle/0".to_string(),
            first_index: 0,
            lines: vec![sb_line(); 2],
            seqno: SequenceNo(5),
            sent_at_ms: 1,
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(stale_discards(&mgr), 1);
        assert_eq!(
            mgr.buffer("eagle/0")
                .expect("buffer exists")
                .scrollback()
                .len(),
            0
        );
        assert!(awaiting_sync(&mgr, "eagle/0"));
    }

    // ── Ping / Pong ─────────────────────────────────────────────────────────

    #[test]
    fn ping_replies_with_a_pong_carrying_the_same_seq() {
        let (mut mgr, mut rx) = make_connected_manager();

        let events = mgr.handle_server_message(ServerMessage::Ping { seq: 88 });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(
            matches!(drain(&mut rx).as_slice(), [ClientMessage::Pong { seq }] if *seq == 88),
            "exactly one Pong, echoing the seq"
        );
    }

    #[test]
    fn ping_while_disconnected_sends_nothing_and_still_refreshes_liveness() {
        let mut mgr = make_manager();
        let now = Instant::now();

        let events = mgr.handle_server_message(ServerMessage::Ping { seq: 88 });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert!(mgr.ws_sender.is_none(), "there is nowhere to reply");
        assert!(
            mgr.liveness.idle_since(now) < std::time::Duration::from_secs(1),
            "the frame refreshed the liveness clock"
        );
        assert_eq!(inbound_msgs(&mgr), 1);
    }

    #[test]
    fn pong_for_an_outstanding_ping_records_an_rtt_sample() {
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.set_connection_state(crate::connection_state::ConnectionState::Connected {
            transport: TransportKind::Uds,
        });
        // Put a ping on the wire so its seq is outstanding; the reply below is
        // what closes the round trip. Time is a parameter (R3), so the cadence
        // is reached by passing a later instant rather than by sleeping.
        mgr.maybe_send_client_ping(Instant::now() + crate::liveness::PING_INTERVAL);
        let seq = match drain(&mut rx).as_slice() {
            [ClientMessage::Ping { seq }] => *seq,
            other => panic!("expected one Ping, got {other:?}"),
        };

        let events = mgr.handle_server_message(ServerMessage::Pong { seq });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(mgr.liveness.outstanding_count(), 0, "the seq is cleared");
        let rtt = mgr.last_rtt_ms().expect("an RTT sample was recorded");
        assert!(rtt >= 0.0, "rtt is a real measurement: {rtt}");
        assert_eq!(mgr.active_rtt().expect("summary exists").sample_count, 1);
    }

    #[test]
    fn pong_for_an_unknown_seq_records_no_rtt() {
        let (mut mgr, _rx) = make_connected_manager();

        let events = mgr.handle_server_message(ServerMessage::Pong { seq: 4242 });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(
            mgr.last_rtt_ms(),
            None,
            "an unmatched Pong measures nothing"
        );
        assert_eq!(mgr.liveness.outstanding_count(), 0);
    }

    // ── AuthChallenge ───────────────────────────────────────────────────────

    #[test]
    fn auth_challenge_is_ignored_by_the_session_manager() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);
        let before = observe(&mgr);

        let events = mgr.handle_server_message(ServerMessage::AuthChallenge {
            nonce: vec![1, 2, 3],
        });

        // The bootstrap consumes the challenge before the session manager runs;
        // the arm exists only for exhaustiveness.
        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(observe(&mgr), before);
        assert!(drain(&mut rx).is_empty(), "no AuthProof is sent from here");
        assert_eq!(inbound_msgs(&mgr), 1);
    }

    // ── ClientListResult / ClientKicked / SessionKicked ─────────────────────

    /// One connected-clients row, labelled `user{id}@host`.
    fn sample_client(id: u64) -> ClientInfo {
        ClientInfo {
            client_id: ClientId(id),
            connection_id: ConnectionId(id + 1),
            label: format!("user{id}@host"),
            machine_id: "abcd".to_string(),
            hostname: "host".to_string(),
            username: format!("user{id}"),
            transport: "uds".to_string(),
            attached_panes: vec![],
            uptime_secs: 5,
            is_self: false,
            frontend: FrontendKind::Cli,
            build: String::new(),
            build_profile: String::new(),
        }
    }

    #[test]
    fn client_list_result_caches_the_clients_and_names_the_session() {
        let mut mgr = make_manager();

        let events = mgr.handle_server_message(ServerMessage::ClientListResult {
            request_id: 12,
            word_id: "eagle".to_string(),
            clients: vec![ClientInfo {
                attached_panes: vec![0],
                is_self: true,
                label: "user@host".to_string(),
                ..sample_client(1)
            }],
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::ClientListReceived { word_id }] if word_id == "eagle"
            ),
            "{events:?}"
        );
        assert_eq!(mgr.client_list.len(), 1);
        assert_eq!(mgr.client_list[0].label, "user@host");
        assert_eq!(mgr.client_list_word.as_deref(), Some("eagle"));
    }

    /// A manager whose cached client list for `word_id` holds `ids`.
    fn manager_with_client_list(word_id: &str, ids: &[u64]) -> SessionManager {
        let mut mgr = make_manager();
        mgr.client_list_word = Some(word_id.to_string());
        mgr.client_list = ids.iter().map(|id| sample_client(*id)).collect();
        mgr
    }

    #[test]
    fn client_kicked_prunes_the_kicked_connection_from_the_cached_list() {
        let mut mgr = manager_with_client_list("eagle", &[1, 2]);

        let events = mgr.handle_server_message(ServerMessage::ClientKicked {
            request_id: 13,
            word_id: "eagle".to_string(),
            client_id: ClientId(1),
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::ClientKicked { word_id, client_id }]
                    if word_id == "eagle" && client_id.0 == 1
            ),
            "{events:?}"
        );
        assert_eq!(
            mgr.client_list
                .iter()
                .map(|c| c.client_id)
                .collect::<Vec<_>>(),
            vec![ClientId(2)],
            "the row goes as soon as the kick is acked, not on the next poll"
        );
    }

    #[test]
    fn client_kicked_for_another_session_leaves_the_cached_list_alone() {
        // The cache is scoped to one `client_list_word`; a `ClientId` is only
        // meaningful next to the session the list was fetched for.
        let mut mgr = manager_with_client_list("eagle", &[1, 2]);

        mgr.handle_server_message(ServerMessage::ClientKicked {
            request_id: 14,
            word_id: "otter".to_string(),
            client_id: ClientId(1),
        });

        assert_eq!(mgr.client_list.len(), 2);
    }

    #[test]
    fn session_kicked_emits_the_eviction_without_leaving_the_session() {
        let (mut mgr, _rx) = manager_on("eagle");

        let events = mgr.handle_server_message(ServerMessage::SessionKicked {
            word_id: "eagle".to_string(),
            by_label: "someone@else".to_string(),
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::KickedFromSession { word_id, by_label }]
                    if word_id == "eagle" && by_label == "someone@else"
            ),
            "{events:?}"
        );
        // SUSPECT: the client stays fully attached — session, tab, pane and
        // buffers are all untouched. `SessionEvent::KickedFromSession`'s doc says
        // "The app should leave the session", but nothing consumes it: the event
        // falls into `AppCore::handle_session_events`' `_ => {}`, so a kicked GUI
        // keeps rendering a session the daemon has already detached it from.
        assert_eq!(mgr.active_session(), Some("eagle"));
        assert_eq!(mgr.active_pane_id(), Some("eagle/0"));
        assert_eq!(mgr.session_list().len(), 1);
    }

    // ── Peer{Opened,Error,Closed} ───────────────────────────────────────────

    #[test]
    fn peer_opened_emits_the_peer_without_refreshing_the_session_list() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);

        let events = mgr.handle_server_message(ServerMessage::PeerOpened {
            request_id: 14,
            peer: "work".to_string(),
        });

        assert!(
            matches!(events.as_slice(), [SessionEvent::PeerOpened { peer }] if peer == "work"),
            "{events:?}"
        );
        // The refresh is deliberately the app layer's (`AppCore` re-arms
        // auto-select and re-requests the list), so this arm sends nothing.
        assert!(
            drain(&mut rx).is_empty(),
            "no session-list refresh is issued"
        );
        assert_eq!(mgr.session_list().len(), 1);
    }

    #[test]
    fn peer_error_emits_the_reason_and_the_attributed_peer() {
        let mut mgr = make_manager();

        let events = mgr.handle_server_message(ServerMessage::PeerError {
            request_id: 15,
            peer: Some("work".to_string()),
            reason: "ssh: connection refused".to_string(),
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::PeerError { peer, reason }]
                    if peer.as_deref() == Some("work") && reason == "ssh: connection refused"
            ),
            "{events:?}"
        );
        // Unlike `Error`, a federation failure never touches `status_msg`; the app
        // layer decides between a per-remote error row and a full disconnect.
        assert_eq!(mgr.status_msg(), "");
    }

    #[test]
    fn peer_error_without_attribution_emits_a_peerless_error() {
        let mut mgr = make_manager();

        let events = mgr.handle_server_message(ServerMessage::PeerError {
            request_id: 16,
            peer: None,
            reason: "no peer".to_string(),
        });

        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::PeerError { peer, reason }]
                    if peer.is_none() && reason == "no peer"
            ),
            "{events:?}"
        );
        assert_eq!(mgr.status_msg(), "");
    }

    #[test]
    fn peer_closed_is_ignored_and_leaves_the_peers_sessions_listed() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);
        let before = observe(&mgr);

        let events = mgr.handle_server_message(ServerMessage::PeerClosed {
            request_id: 17,
            peer: "work".to_string(),
        });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(observe(&mgr), before);
        assert!(drain(&mut rx).is_empty());
        assert_eq!(inbound_msgs(&mgr), 1);
    }

    // ── NotifyAccepted / LogChunk / LogEnd ──────────────────────────────────

    #[test]
    fn notify_accepted_is_ignored_by_the_streaming_session_manager() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);
        let before = observe(&mgr);

        let events = mgr.handle_server_message(ServerMessage::NotifyAccepted { request_id: 18 });

        assert!(events.is_empty(), "no UI event: {events:?}");
        assert_eq!(observe(&mgr), before);
        assert_eq!(inbound_msgs(&mgr), 1);
    }

    #[test]
    fn log_chunk_and_log_end_are_ignored_by_the_streaming_session_manager() {
        let (mut mgr, mut rx) = manager_on("eagle");
        drain(&mut rx);
        let before = observe(&mgr);

        let chunk = mgr.handle_server_message(ServerMessage::LogChunk {
            request_id: 19,
            data: b"a log line\n".to_vec(),
        });
        let end = mgr.handle_server_message(ServerMessage::LogEnd { request_id: 19 });

        assert!(chunk.is_empty(), "no UI event: {chunk:?}");
        assert!(end.is_empty(), "no UI event: {end:?}");
        assert_eq!(observe(&mgr), before);
        assert_eq!(inbound_msgs(&mgr), 2, "both frames are accounted");
    }
}
