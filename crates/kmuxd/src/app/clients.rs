//! Client-management endpoints for locally-hosted sessions (issue #146): list the
//! connections attached to a session, and kick one connection out of it. The
//! dispatch layer routes federated sessions to the owning peer instead (see
//! [`super::ServerApp::is_federated_session`]).

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use kmux_protocol::format_pane_id;
use kmux_protocol::messages::{ClientId, ClientInfo, ConnectionId, InputMode, ServerMessage};

use super::ServerApp;

/// Outcome of a [`ServerApp::kick_client_from_session`] request.
pub enum KickOutcome {
    /// The connection was detached from the session.
    Kicked,
    /// No session with that word id is hosted locally.
    SessionNotFound,
    /// The target connection is not attached to any pane of the session.
    ClientNotFound,
}

impl ServerApp {
    /// Snapshot every live client connection with its build identity, for the
    /// `connections` control RPC (backs `kmux client status`). Unlike
    /// [`list_session_clients`](Self::list_session_clients) this is not scoped to
    /// a session — it lists the whole connection registry.
    pub async fn snapshot_connections(&self) -> kmux_protocol::control_rpc::ConnectionsResponse {
        use kmux_protocol::control_rpc::{ConnectionSummary, ConnectionsResponse};
        let conns = self.connections.read().await;
        let mut connections: Vec<ConnectionSummary> = conns
            .iter()
            .map(|(id, c)| ConnectionSummary {
                connection_id: *id,
                label: c.label.clone(),
                machine_id: c.machine_id.clone(),
                frontend: c.client_kind.to_string(),
                build: if c.client_git_dirty {
                    format!("{}-dirty", c.client_git_sha)
                } else {
                    c.client_git_sha.clone()
                },
                build_profile: c.client_build_profile.clone(),
                transport: c.transport.to_string(),
                uptime_secs: c.metrics.created_at.elapsed().as_secs(),
            })
            .collect();
        connections.sort_unstable_by_key(|c| c.connection_id);
        ConnectionsResponse { connections }
    }

    /// List the client connections attached to any pane of the locally-hosted
    /// session `word_id`, joined with each connection's identity (machine id,
    /// label, hostname). `requester` is marked `is_self`. Returns `None` when the
    /// session is not hosted locally (the caller forwards to the owning peer).
    ///
    /// Holds the sessions and connections locks together (sessions first, like
    /// [`ServerApp::snapshot_sessions_with_connections`]) to avoid tearing.
    pub async fn list_session_clients(
        &self,
        word_id: &str,
        requester: ClientId,
    ) -> Option<Vec<ClientInfo>> {
        let sessions = self.sessions.read().await;
        let state = sessions.get(word_id)?;

        // Collect the panes each attached client is viewing in this session.
        let mut panes_by_client: HashMap<ClientId, Vec<u32>> = HashMap::new();
        for (pane_index, relay) in &state.panes {
            for client_id in relay.clients.lock().unwrap().keys() {
                panes_by_client
                    .entry(*client_id)
                    .or_default()
                    .push(*pane_index);
            }
        }

        let conns = self.connections.read().await;
        let mut out: Vec<ClientInfo> = panes_by_client
            .into_iter()
            .filter_map(|(client_id, mut panes)| {
                panes.sort_unstable();
                panes.dedup();
                // Find this client's live connection record for its identity.
                let (conn_id_u64, c) = conns.iter().find(|(_, c)| c.client_id == client_id)?;
                Some(ClientInfo {
                    client_id,
                    connection_id: ConnectionId(*conn_id_u64),
                    label: c.label.clone(),
                    machine_id: c.machine_id.clone(),
                    hostname: c.hostname.clone(),
                    username: c.username.clone(),
                    transport: c.transport.to_string(),
                    attached_panes: panes,
                    uptime_secs: c.metrics.created_at.elapsed().as_secs(),
                    is_self: client_id == requester,
                    frontend: c.client_kind,
                    build: if c.client_git_dirty {
                        format!("{}-dirty", c.client_git_sha)
                    } else {
                        c.client_git_sha.clone()
                    },
                    build_profile: c.client_build_profile.clone(),
                })
            })
            .collect();
        // Stable display order.
        out.sort_unstable_by_key(|c| c.client_id.0);
        Some(out)
    }

    /// Detach `target` from every pane of the locally-hosted session `word_id`,
    /// notify it with [`ServerMessage::SessionKicked`], and leave its connection
    /// alive. `by_label` is the requester's label, surfaced to the kicked client.
    pub async fn kick_client_from_session(
        &self,
        word_id: &str,
        target: ClientId,
        by_label: &str,
    ) -> KickOutcome {
        let mut sessions = self.sessions.write().await;
        let Some(state) = sessions.get_mut(word_id) else {
            return KickOutcome::SessionNotFound;
        };

        // Confirm attachment and grab the target's control channel before detaching.
        let mut ctrl_tx = None;
        for relay in state.panes.values() {
            if let Some(sender) = relay.clients.lock().unwrap().get(&target) {
                ctrl_tx = Some(sender.ctrl_tx.clone());
                break;
            }
        }
        let Some(ctrl_tx) = ctrl_tx else {
            return KickOutcome::ClientNotFound;
        };

        // Detach from every pane of this session, releasing any input lock and
        // reconciling the smallest-wins size so the kicked client no longer counts.
        for (pane_index, relay) in state.panes.iter_mut() {
            let pane_id = format_pane_id(&state.meta.word_id, *pane_index);
            relay.clients.lock().unwrap().remove(&target);
            relay.recompute_live_capabilities();
            if relay.input_mode == InputMode::Locked(target) {
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
        drop(sessions);

        let _ = ctrl_tx.send(ServerMessage::SessionKicked {
            word_id: word_id.to_string(),
            by_label: by_label.to_string(),
        });
        KickOutcome::Kicked
    }
}
