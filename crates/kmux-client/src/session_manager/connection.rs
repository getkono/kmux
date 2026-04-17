use std::time::Instant;

use kmux_protocol::messages::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::connect::{self, ConnectResult};
use crate::connection_state::{ConnectionState, DisconnectReason};
use crate::transport::TransportKind;

use super::SessionManager;

impl SessionManager {
    pub async fn connect(
        &mut self,
        srv_tx: mpsc::UnboundedSender<ServerMessage>,
    ) -> Vec<super::server_handler::SessionEvent> {
        let host = self.host.clone();
        let port = self.port;
        let token = self.token.clone();
        let accept_invalid = self.accept_invalid_certs;

        self.set_connection_state(ConnectionState::Handshaking);

        match connect::connect(
            host,
            port,
            token,
            accept_invalid,
            srv_tx,
            self.capabilities.clone(),
            self.connection_id,
        )
        .await
        {
            ConnectResult::Connected(sender) => {
                self.ws_sender = Some(sender);
                self.last_host = self.host.clone();
                self.last_port = self.port;
                self.current_transport = TransportKind::Quic;
                self.set_connection_state(ConnectionState::Connected {
                    transport: TransportKind::Quic,
                });
                self.liveness.reset(Instant::now());
                info!("Connected to kmuxd");

                let rid = self.next_rid();
                self.send_ws(ClientMessage::SessionList { request_id: rid });

                vec![]
            }
            ConnectResult::Failed(e) => {
                warn!("Connection failed: {e}");
                self.set_connection_state(ConnectionState::Disconnected {
                    reason: DisconnectReason::BootstrapFailed(e),
                });
                vec![]
            }
        }
    }

    pub fn set_ws_sender(&mut self, sender: mpsc::UnboundedSender<ClientMessage>) {
        self.ws_sender = Some(sender);
        self.last_host = self.host.clone();
        self.last_port = self.port;
        self.set_connection_state(ConnectionState::Connected {
            transport: self.current_transport,
        });
        self.liveness.reset(Instant::now());
        info!("Connected to kmuxd (external sender)");
    }

    pub fn request_session_list(&mut self) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::SessionList { request_id: rid });
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
        info!(
            "Transport channel switched: {} -> {}",
            old_transport, new_kind
        );
    }
}
