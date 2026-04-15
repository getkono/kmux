use kmux_protocol::messages::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::connect::{self, ConnectResult};
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
                self.connected = true;
                self.status_msg = format!("Connected to {}:{}", self.host, self.port);
                self.last_host = self.host.clone();
                self.last_port = self.port;
                info!("Connected to kmuxd");

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

    pub fn set_ws_sender(&mut self, sender: mpsc::UnboundedSender<ClientMessage>) {
        self.ws_sender = Some(sender);
        self.connected = true;
        self.status_msg = format!("Connected to {}:{}", self.host, self.port);
        self.last_host = self.host.clone();
        self.last_port = self.port;
        info!("Connected to kmuxd (external sender)");
    }

    pub fn request_session_list(&mut self) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::SessionList { request_id: rid });
    }

    pub fn disconnect(&mut self) {
        self.ws_sender = None;
        self.connected = false;
        self.buffers.clear();
        self.active_session = None;
        self.active_pane = None;
        self.session_list.clear();
        self.pane_sync.clear();
        self.input_locked.clear();
        self.status_msg = "Disconnected".to_string();
    }

    pub fn mark_connection_lost(&mut self) {
        self.connected = false;
        self.ws_sender = None;
        self.status_msg = "Connection lost".to_string();
    }

    pub fn set_connection_params(&mut self, host: String, port: u16, token: String) {
        self.host = host;
        self.port = port;
        self.token = token;
    }

    /// Attempt to switch the active transport to QUIC.
    ///
    /// Called when the QUIC upgrade probe in `quic_probe::quic_upgrade_loop`
    /// signals success.  The new QUIC sender must already be authenticated
    /// (i.e., the server has received `Auth { connection_id: Some(...) }` and
    /// sent back `AuthResult { success: true }`).
    ///
    /// The caller is responsible for sending `ClientMessage::ChannelReady` on
    /// the new sender before calling this method.
    pub fn apply_quic_upgrade(&mut self, new_sender: mpsc::UnboundedSender<ClientMessage>) {
        let old_transport = self.current_transport;
        // Drop the old sender, closing the old transport channel.
        let _ = self.ws_sender.replace(new_sender);
        self.current_transport = TransportKind::Quic;
        info!(
            "Transport channel upgraded: {} -> {}",
            old_transport,
            TransportKind::Quic
        );
    }

    /// Switch the active transport to TCP (fallback).
    ///
    /// Called when QUIC drops and a TCP-over-SSH tunnel has been re-established.
    /// `new_sender` must already be authenticated on the TCP transport with the
    /// existing `connection_id`.
    pub fn apply_tcp_fallback(&mut self, new_sender: mpsc::UnboundedSender<ClientMessage>) {
        let old_transport = self.current_transport;
        let _ = self.ws_sender.replace(new_sender);
        self.current_transport = TransportKind::Tcp;
        info!(
            "Transport channel fell back: {} -> {}",
            old_transport,
            TransportKind::Tcp
        );
    }
}
