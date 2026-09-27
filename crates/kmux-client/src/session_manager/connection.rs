use std::time::Instant;

use kmux_protocol::messages::{ClientId, ClientMessage, PeerId, PeerTarget, ServerMessage, WordId};
use tokio::sync::mpsc;
use tracing::info;

use crate::connection_state::{ConnectionState, DisconnectReason};
use crate::pipeline::{
    self, BootstrapError, BootstrapObserver, BootstrapOutcome, ResolvedTarget, SshContext,
};
use crate::transport::TransportKind;

use super::SessionManager;

impl SessionManager {
    /// Run the bootstrap pipeline for `target` and, on success, wire the
    /// resulting data-plane sender into this session manager.
    ///
    /// The same code path is used by `--dry-run` / `--test` (with a
    /// `ConsoleObserver`) so a successful dry-run proves the real flow
    /// works. Callers handle the returned `SshContext` — spawning the
    /// tunnel-death monitor and the `TransportSupervisor` — because
    /// those live in the frontend, not the client library.
    pub async fn connect(
        &mut self,
        srv_tx: mpsc::UnboundedSender<ServerMessage>,
        target: ResolvedTarget,
        observer: &dyn BootstrapObserver,
    ) -> Result<Option<SshContext>, BootstrapError> {
        self.set_connection_state(ConnectionState::Handshaking);

        match pipeline::run_bootstrap(
            target,
            self.capabilities.clone(),
            self.connection_id,
            srv_tx,
            observer,
        )
        .await
        {
            Ok(outcome) => Ok(self.apply_outcome(outcome)),
            Err(e) => {
                self.set_connection_state(ConnectionState::Disconnected {
                    reason: DisconnectReason::BootstrapFailed(e.to_string()),
                });
                Err(e)
            }
        }
    }

    /// Consume a successful [`BootstrapOutcome`] and update all
    /// connection-derived state. Returns the SSH context (if any) so the
    /// caller can spawn the tunnel-death monitor + supervisor.
    pub fn apply_outcome(&mut self, outcome: BootstrapOutcome) -> Option<SshContext> {
        self.ensure_worker();
        self.ws_sender = Some(outcome.client_tx);
        self.host = outcome.host.clone();
        self.port = outcome.port;
        self.last_host = outcome.host;
        self.last_port = outcome.port;
        self.token = outcome.token;
        self.accept_invalid_certs = outcome.accept_invalid_certs;
        self.current_transport = outcome.transport;
        self.connection_id = Some(outcome.connection_id);
        if outcome.server_version.is_some() {
            self.server_version = outcome.server_version;
        }
        self.set_connection_state(ConnectionState::Connected {
            transport: outcome.transport,
        });
        self.liveness.reset(Instant::now());
        self.tag_transport(outcome.transport);
        info!(
            transport = %outcome.transport,
            connection_id = outcome.connection_id.0,
            "Connected to kmuxd",
        );

        self.request_session_list();
        self.previous_daemon_pid = self.daemon_pid;
        self.daemon_pid = outcome.daemon_pid;
        self.resume_visible_panes(self.link_reached_same_daemon());

        outcome.ssh_context
    }

    /// Whether the last link reached the same daemon run as the one before it
    /// (issue #208): only then do its panes resume from their seqnos, and is
    /// input held during the outage still meant for the same shells.
    pub fn link_reached_same_daemon(&self) -> bool {
        self.daemon_pid.is_some() && self.daemon_pid == self.previous_daemon_pid
    }

    pub fn set_ws_sender(&mut self, sender: mpsc::UnboundedSender<ClientMessage>) {
        self.ensure_worker();
        self.ws_sender = Some(sender);
        self.last_host = self.host.clone();
        self.last_port = self.port;
        self.set_connection_state(ConnectionState::Connected {
            transport: self.current_transport,
        });
        self.liveness.reset(Instant::now());
        self.tag_transport(self.current_transport);
        info!("Connected to kmuxd (external sender)");
    }

    pub fn request_session_list(&mut self) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::SessionList { request_id: rid });
    }

    /// Request a fresh process-overview snapshot (issue #122). The app layer
    /// drives this on a ~1 Hz cadence while the overview view is open; the reply
    /// is a `ProcessOverviewResult` handled into `process_overview`.
    pub fn request_process_overview(&mut self) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::ProcessOverview { request_id: rid });
    }

    /// Request the connections attached to `word_id` (issue #146). The app layer
    /// drives this while the connected-clients view is open; the reply is a
    /// `ClientListResult` handled into [`super::SessionManager::client_list`].
    pub fn request_client_list(&mut self, word_id: WordId) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::ClientList {
            request_id: rid,
            word_id,
        });
    }

    /// Kick one client connection out of `word_id` (issue #146). The reply is a
    /// `ClientKicked` (handled as a [`SessionEvent`](super::SessionEvent)) or an
    /// `Error`.
    pub fn kick_client(&mut self, word_id: WordId, client_id: ClientId) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::KickClient {
            request_id: rid,
            word_id,
            client_id,
        });
    }

    /// Ask the (local) daemon to federate `target` (issue #121): it opens one
    /// upstream connection to the remote `kmuxd` and surfaces that peer's
    /// sessions in our `SessionList`. The reply is a `PeerOpened`/`PeerError`
    /// (handled as a [`SessionEvent`](super::SessionEvent)). Idempotent on the
    /// daemon, so it is safe to re-issue after a reconnect to re-federate.
    pub fn open_peer(&mut self, target: PeerTarget) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::OpenPeer {
            request_id: rid,
            target,
        });
    }

    /// Ask the daemon to drop a federated peer's upstream link and stop
    /// surfacing its sessions. Best-effort; the ack needs no reconciliation.
    pub fn close_peer(&mut self, peer: PeerId) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::ClosePeer {
            request_id: rid,
            peer,
        });
    }

    pub fn disconnect(&mut self) {
        self.ws_sender = None;
        self.reset_apply_worker();
        self.buffers.clear();
        self.active_session = None;
        self.active_pane = None;
        self.session_list.clear();
        self.pane_sync.clear();
        self.input_locked.clear();
        // A reconnect gets a fresh daemon that knows nothing of our pause state
        // (issue #68); clear the mirror so the next reconcile re-sends it.
        self.pause_applied = (false, false);
        self.set_connection_state(ConnectionState::Disconnected {
            reason: DisconnectReason::UserInitiated,
        });
    }

    /// Transition to `Disconnected` with an explicit reason. The old
    /// variant (zero-arg) is preserved as a default-reason helper.
    pub fn mark_connection_lost(&mut self) {
        self.mark_connection_lost_with(DisconnectReason::ServerClosed);
    }

    pub fn mark_connection_lost_with(&mut self, reason: DisconnectReason) {
        self.ws_sender = None;
        // The mirror is stale once the link drops; a reconnect re-sends pause
        // state via the next reconcile (issue #68).
        self.pause_applied = (false, false);
        self.set_connection_state(ConnectionState::Disconnected { reason });
    }

    /// Prepare for a fresh bootstrap: drop the dead sender and flip the state
    /// to `Handshaking` so the TUI badge updates immediately. `connection_id`
    /// is intentionally preserved so the server can transfer pane streams to
    /// the new channel.
    pub fn prepare_reconnect(&mut self) {
        self.ws_sender = None;
        self.set_connection_state(ConnectionState::Handshaking);
    }

    /// Like [`Self::prepare_reconnect`], for automatic reconnect attempt
    /// `attempt` (1-based, issue #208): the state reads `Reconnecting`, so the
    /// badge shows the retry rather than a first handshake.
    pub fn begin_reconnect_attempt(&mut self, attempt: u32) {
        self.ws_sender = None;
        // The next link needs the pause state again (issue #68).
        self.pause_applied = (false, false);
        self.set_connection_state(ConnectionState::Reconnecting { attempt });
    }

    pub fn set_connection_params(&mut self, host: String, port: u16, token: String) {
        self.host = host;
        self.port = port;
        self.token = token;
    }

    /// Make `new_sender`, already authenticated on `new_kind` with this
    /// connection's `ConnectionId`, the active transport.
    ///
    /// Driven by the `TransportSupervisor`'s `UpgradeSignal`: the caller sends
    /// `ChannelReady` on `new_sender` first, and dropping the old sender closes
    /// the old channel.
    pub fn apply_transport_upgrade(
        &mut self,
        new_sender: mpsc::UnboundedSender<ClientMessage>,
        new_kind: TransportKind,
    ) {
        let old_transport = self.current_transport;
        let _ = self.ws_sender.replace(new_sender);
        self.current_transport = new_kind;
        self.set_connection_state(ConnectionState::Connected {
            transport: new_kind,
        });
        self.liveness.reset(Instant::now());
        self.tag_transport(new_kind);
        info!(
            "Transport channel switched: {} -> {}",
            old_transport, new_kind
        );
    }
}
