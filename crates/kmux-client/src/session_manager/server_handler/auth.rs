//! The handshake and the transport swap that can follow it.

use super::*;

/// The eleven fields of `ServerMessage::AuthResult`, carried as one value.
///
/// A struct rather than eleven parameters: they arrive together, they are read
/// together, and eight of them are `Option<String>` — eight chances to transpose
/// two arguments the compiler cannot tell apart.
pub(super) struct AuthOutcome {
    pub success: bool,
    pub reason: Option<String>,
    pub client_id: Option<ClientId>,
    pub server_version: Option<String>,
    pub connection_id: Option<ConnectionId>,
    pub compression: Option<Compression>,
    pub machine_id: Option<String>,
    pub label: Option<String>,
    pub server_machine_id: Option<String>,
    pub negotiated_protocol: Option<ProtocolVersion>,
    pub negotiated_capabilities: Vec<String>,
}

impl SessionManager {
    /// Handle a `AuthResult` frame.
    pub(super) fn on_auth_result(&mut self, outcome: AuthOutcome) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        let AuthOutcome {
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
        } = outcome;
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
            self.set_connection_state(crate::connection_state::ConnectionState::Disconnected {
                reason: crate::connection_state::DisconnectReason::AuthFailed(msg),
            });
            events.push(SessionEvent::AuthFailed { reason: reason_str });
        }
        events
    }

    /// Federation responses (issue #121). The local daemon sends these
    /// after we issue `OpenPeer`/`ClosePeer` to federate a remote server.
    /// The handshake challenge is consumed by the connection bootstrap; the
    /// session manager never sees it in practice. Ignore for exhaustiveness.
    pub(super) fn on_auth_challenge() -> Vec<SessionEvent> {
        Vec::new()
    }

    /// Handle a `ChannelSwitched` frame.
    pub(super) fn on_channel_switched(&mut self, old_transport: &str) -> Vec<SessionEvent> {
        let new_transport = self.current_transport;
        info!(
            "Transport channel switched: {} -> {}",
            old_transport, new_transport
        );
        Vec::new()
    }
}
