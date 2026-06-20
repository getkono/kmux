use std::sync::Arc;
use std::time::Instant;

use kmux_protocol::messages::{
    ClientId, ClientMessage, PaneInfo, SequenceNo, ServerMessage, SessionEventMsg, SessionStatus,
    epoch_millis,
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
    /// A pane's window title changed (OSC 0/2).
    PaneTitleChanged { pane_id: String, title: String },
    /// A pane's program wrote the clipboard via OSC 52. `data` is the still
    /// base64-encoded payload; the app layer decodes and applies it (subject to
    /// the active-pane policy) at the frontend's clipboard leaf.
    ClipboardCopy {
        pane_id: String,
        selection: String,
        data: String,
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

    pub fn handle_server_message(&mut self, msg: ServerMessage) -> Vec<SessionEvent> {
        let mut events = Vec::new();

        // Any decoded frame is proof the server is alive. Refreshing the
        // liveness timer here — in one place — means every transport gets
        // timeout detection for free.
        self.liveness.observe_inbound(Instant::now());

        // Attribute the wire cost of this frame to the currently tagged
        // transport. We re-encode for the byte count; postcard is cheap
        // enough that this is negligible next to decoding.
        let inbound_bytes = kmux_protocol::encode_server(&msg)
            .map(|b| b.len())
            .unwrap_or(0);
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
            } => {
                if success {
                    self.client_id = client_id;
                    self.server_version = server_version;
                    self.connection_id = connection_id;
                    // The daemon decides compression; frames self-describe, so
                    // this is informational only (see docs/compression.md).
                    info!("Authenticated (wire compression: {compression:?})");
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
                        self.buffers.entry(pane.pane_id.clone()).or_default();
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

            ServerMessage::ProcessOverviewResult { panes, .. } => {
                self.process_overview = panes;
                events.push(SessionEvent::ProcessOverviewReceived);
            }

            ServerMessage::SessionCreated { entry, .. } => {
                let word_id = entry.meta.word_id.clone();
                for pane in &entry.panes {
                    self.buffers.entry(pane.pane_id.clone()).or_default();
                }
                self.session_list.push(entry);
                self.status_msg = format!("Session '{word_id}' created");
                // Switch to the new session (detaches the old visible set and
                // attaches the new session's active tab).
                self.select_session(word_id.clone());
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
                        self.in_flight_history_fetches.remove(&pane.pane_id);
                    }
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
                events.push(SessionEvent::SessionClosed { word_id });
            }

            ServerMessage::PaneCreated {
                pane_id,
                session_word_id,
                size,
                ..
            } => {
                self.buffers.entry(pane_id.clone()).or_default();
                // Record the new pane in the flat list for immediate chrome.
                if let Some(entry) = self
                    .session_list
                    .iter_mut()
                    .find(|e| e.meta.word_id == session_word_id)
                {
                    let pane_index = pane_id
                        .rsplit_once('/')
                        .and_then(|(_, idx)| idx.parse().ok())
                        .unwrap_or(0);
                    if !entry.panes.iter().any(|p| p.pane_id == pane_id) {
                        entry.panes.push(PaneInfo {
                            pane_id: pane_id.clone(),
                            pane_index,
                            program: String::new(),
                            size,
                            attached_clients: vec![],
                            status: SessionStatus::Running,
                            title: String::new(),
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

            ServerMessage::PaneClosed { pane_id, .. } => {
                self.buffers.remove(&pane_id);
                self.pane_sync.remove(&pane_id);
                self.input_locked.remove(&pane_id);
                self.in_flight_history_fetches.remove(&pane_id);

                // Remove pane from session_list
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
                            self.active_session = None;
                            self.active_pane = None;
                        }
                    }
                }
                events.push(SessionEvent::PaneClosed { pane_id });
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

            ServerMessage::TabClosed {
                word_id, tab_index, ..
            } => {
                if let Some(entry) = self
                    .session_list
                    .iter_mut()
                    .find(|e| e.meta.word_id == word_id)
                {
                    entry.tabs.retain(|t| t.tab_index != tab_index);
                }
                // If the closed tab was the one we were viewing, move to another.
                if self.active_session.as_deref() == Some(word_id.as_str())
                    && self.active_tab == Some(tab_index)
                {
                    self.active_tab = None;
                    self.visible_panes.clear();
                    let next = self
                        .session_list
                        .iter()
                        .find(|e| e.meta.word_id == word_id)
                        .and_then(|e| e.tabs.first())
                        .map(|t| t.tab_index);
                    match next {
                        Some(t) => self.select_tab(t),
                        None => {
                            self.active_pane = None;
                        }
                    }
                }
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
                self.buffers.entry(new_pane.pane_id.clone()).or_default();
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
                    pane_id.clone(),
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
                self.in_flight_history_fetches.remove(&pane_id);
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
                self.status_msg =
                    format!("Input lock denied on '{pane_id}' (held by {:?})", holder);
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
                self.pane_sync.insert(
                    pane_id.clone(),
                    PaneSync::Synced {
                        expected: SequenceNo(seqno.0 + 1),
                    },
                );
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                self.metrics.record_apply(sent_at_ms, elapsed_ms);
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
            ServerMessage::PeerOpened { peer, .. } => {
                events.push(SessionEvent::PeerOpened { peer });
            }
            ServerMessage::PeerError { peer, reason, .. } => {
                events.push(SessionEvent::PeerError { peer, reason });
            }
            // A close ack needs no app-level reconciliation (the peer's sessions
            // simply stop appearing in the next `SessionList`).
            ServerMessage::PeerClosed { .. } => {}
        }
        events
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
