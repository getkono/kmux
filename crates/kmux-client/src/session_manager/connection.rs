use std::time::Instant;

use kmux_protocol::messages::{ClientMessage, PeerId, PeerTarget, ServerMessage};
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

        outcome.ssh_context
    }

    pub fn set_ws_sender(&mut self, sender: mpsc::UnboundedSender<ClientMessage>) {
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
        self.buffers.clear();
        self.active_session = None;
        self.active_pane = None;
        self.session_list.clear();
        self.pane_sync.clear();
        self.input_locked.clear();
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

    pub fn set_connection_params(&mut self, host: String, port: u16, token: String) {
        self.host = host;
        self.port = port;
        self.token = token;
    }

    /// Attempt to switch the active transport to QUIC.
    ///
    /// Called when the `TransportSupervisor` signals a successful QUIC probe.
    /// The new QUIC sender must already be authenticated (i.e., the server has
    /// received `Auth { connection_id: Some(...) }` and sent back
    /// `AuthResult { success: true }`).
    ///
    /// The caller is responsible for sending `ClientMessage::ChannelReady` on
    /// the new sender before calling this method.
    pub fn apply_quic_upgrade(&mut self, new_sender: mpsc::UnboundedSender<ClientMessage>) {
        let old_transport = self.current_transport;
        // Drop the old sender, closing the old transport channel.
        let _ = self.ws_sender.replace(new_sender);
        self.current_transport = TransportKind::Quic;
        self.set_connection_state(ConnectionState::Connected {
            transport: TransportKind::Quic,
        });
        self.liveness.reset(Instant::now());
        self.tag_transport(TransportKind::Quic);
        info!(
            "Transport channel upgraded: {} -> {}",
            old_transport,
            TransportKind::Quic
        );
    }

    /// Switch the active transport to TCP+TLS (fallback).
    ///
    /// Called when QUIC drops and a TCP+TLS-over-SSH tunnel has been re-established.
    /// `new_sender` must already be authenticated on the TCP+TLS transport with the
    /// existing `connection_id`.
    pub fn apply_tcp_fallback(&mut self, new_sender: mpsc::UnboundedSender<ClientMessage>) {
        let old_transport = self.current_transport;
        let _ = self.ws_sender.replace(new_sender);
        self.current_transport = TransportKind::TcpTls;
        self.set_connection_state(ConnectionState::Connected {
            transport: TransportKind::TcpTls,
        });
        self.liveness.reset(Instant::now());
        self.tag_transport(TransportKind::TcpTls);
        info!(
            "Transport channel fell back: {} -> {}",
            old_transport,
            TransportKind::TcpTls
        );
    }

    /// Apply any transport upgrade, choosing the correct method based on `kind`.
    ///
    /// Used by the `TransportSupervisor` so it can signal upgrades without
    /// knowing which specific method to call.
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
