//! The handshake, and the channel-swap notice that follows it.
//!
//! The pre-auth gate lives here: until a connection has proved its identity the
//! only two messages that mean anything are `Auth` and `AuthProof`, and this is
//! the one place in the dispatcher that can decide to hang up.

use kmux_protocol::messages::{
    CAPABILITY_FRAME_ZSTD, ClientCapabilities, ClientMessage, Compression, ConnectionId, ErrorCode,
    FrontendKind, ProtocolRange, ServerMessage, negotiate_capabilities,
};
use tracing::{info, warn};

use crate::app::ClientIdentity;
use crate::auth::validate_token;

use super::super::{PendingAuth, SharedClientState};
use super::Flow;

/// Build a failed `AuthResult` carrying a human-readable `reason` (issue #146).
fn auth_failure(reason: String) -> ServerMessage {
    ServerMessage::AuthResult {
        success: false,
        reason: Some(reason),
        client_id: None,
        server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        connection_id: None,
        compression: None,
        machine_id: None,
        label: None,
        server_machine_id: None,
        negotiated_protocol: None,
        negotiated_capabilities: Vec::new(),
    }
}

/// The pre-auth gate: what a connection may say before it has proved who it is.
///
/// Exactly two messages mean anything here, and they are a two-step handshake —
/// `Auth` earns a nonce, `AuthProof` signs it. Everything else is refused
/// without closing, so a client that sent its first message too early can
/// recover by sending `Auth`.
pub(super) async fn handle_unauthenticated(
    state: &mut SharedClientState,
    msg: ClientMessage,
) -> Flow {
    match msg {
        ClientMessage::Auth {
            token,
            protocol_range,
            protocol_capabilities,
            capabilities,
            connection_id,
            public_key,
            hostname,
            username,
            client_kind,
            client_git_sha,
            client_git_dirty,
            client_build_profile,
        } => on_auth(
            state,
            AuthRequest {
                token,
                protocol_range,
                protocol_capabilities,
                capabilities,
                connection_id,
                public_key,
                hostname,
                username,
                client_kind,
                client_git_sha,
                client_git_dirty,
                client_build_profile,
            },
        ),
        ClientMessage::AuthProof { signature } => on_auth_proof(state, signature).await,
        _ => {
            state.error(None, ErrorCode::NotAuthenticated, "send Auth first");
            Flow::Continue
        }
    }
}

/// The twelve fields of [`ClientMessage::Auth`], carried as one value.
///
/// A struct rather than twelve parameters: they are one message, they are
/// destructured together, and every one of them ends up in `PendingAuth`
/// unchanged. Twelve positional arguments would be twelve chances to transpose
/// two `String`s the compiler cannot tell apart.
pub(super) struct AuthRequest {
    pub token: String,
    pub protocol_range: ProtocolRange,
    pub protocol_capabilities: Vec<String>,
    pub capabilities: ClientCapabilities,
    pub connection_id: Option<ConnectionId>,
    pub public_key: Vec<u8>,
    pub hostname: String,
    pub username: String,
    pub client_kind: FrontendKind,
    pub client_git_sha: String,
    pub client_git_dirty: bool,
    pub client_build_profile: String,
}

/// Step 1: validate token + protocol, then issue a signing challenge.
///
/// A mismatch on either closes the connection — there is nothing a peer that
/// cannot speak the protocol, or does not hold the token, can usefully say next.
pub(super) fn on_auth(state: &mut SharedClientState, req: AuthRequest) -> Flow {
    let Some(negotiated_protocol) = kmux_protocol::compat::negotiate_protocol(req.protocol_range)
    else {
        state.send(auth_failure(format!(
            "protocol version mismatch: client={}, server={}",
            req.protocol_range,
            kmux_protocol::messages::PROTOCOL_RANGE
        )));
        warn!(
            "Protocol version mismatch: client={}, server={}",
            req.protocol_range,
            kmux_protocol::messages::PROTOCOL_RANGE
        );
        return Flow::Close;
    };
    let negotiated_capabilities = negotiate_capabilities(&req.protocol_capabilities);
    if !validate_token(&req.token, &state.app.auth_token) {
        state.send(auth_failure("invalid token".to_string()));
        warn!("authentication failed");
        return Flow::Close;
    }
    // Token accepted: challenge the client to prove it holds the private key
    // behind `public_key` (issue #146).
    let nonce = kmux_sys::identity::random_nonce().to_vec();
    state.pending_auth = Some(PendingAuth {
        nonce: nonce.clone(),
        public_key: req.public_key,
        hostname: req.hostname,
        username: req.username,
        capabilities: req.capabilities,
        negotiated_protocol,
        negotiated_capabilities,
        connection_id: req.connection_id,
        client_kind: req.client_kind,
        client_git_sha: req.client_git_sha,
        client_git_dirty: req.client_git_dirty,
        client_build_profile: req.client_build_profile,
    });
    state.send(ServerMessage::AuthChallenge { nonce });
    Flow::Continue
}

/// Step 2: verify the signature over the nonce, then register the connection.
///
/// A proof with no challenge behind it is a protocol error, not an attack, so it
/// is refused without closing. A proof that fails to verify closes: the peer
/// claimed a public key it cannot back.
pub(super) async fn on_auth_proof(state: &mut SharedClientState, signature: Vec<u8>) -> Flow {
    let Some(pending) = state.pending_auth.take() else {
        state.error(
            None,
            ErrorCode::NotAuthenticated,
            "send Auth before AuthProof",
        );
        return Flow::Continue;
    };
    if !kmux_sys::identity::verify(&pending.public_key, &pending.nonce, &signature) {
        state.send(auth_failure("identity verification failed".to_string()));
        warn!("identity verification failed");
        return Flow::Close;
    }
    let machine_id = kmux_sys::identity::fingerprint(&pending.public_key);
    let reg = state
        .app
        .register_client(
            state.transport,
            std::sync::Arc::clone(&state.metrics),
            pending.connection_id,
            ClientIdentity {
                machine_id: machine_id.clone(),
                hostname: pending.hostname,
                username: pending.username,
                client_kind: pending.client_kind,
                client_git_sha: pending.client_git_sha,
                client_git_dirty: pending.client_git_dirty,
                client_build_profile: pending.client_build_profile,
            },
        )
        .await;
    state.client_id = Some(reg.client_id);
    state.connection_id = Some(reg.connection_id);
    state.capabilities = pending.capabilities;
    state.authenticated = true;
    state.pending_swap_from = reg.previous_transport;
    state.machine_id = Some(machine_id.clone());
    state.label = Some(reg.label.clone());
    state.conn_span.record("conn_id", reg.connection_id.0);
    state.conn_span.record("client_id", reg.client_id.0);
    // The daemon decides compression from client locality + config (issue #59).
    // Self-describing frames make this purely a sender policy: flip the shared
    // toggle the writer/attacher tasks read.
    let compress = pending
        .negotiated_capabilities
        .iter()
        .any(|capability| capability == CAPABILITY_FRAME_ZSTD)
        && state.app.compression.enabled_for(state.transport);
    state.comp_out.set_enabled(compress);
    let server_machine_id = state.app.server_machine_id.clone();
    state.send(ServerMessage::AuthResult {
        success: true,
        reason: None,
        client_id: Some(reg.client_id),
        server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        connection_id: Some(reg.connection_id),
        compression: compress.then_some(Compression::Zstd),
        machine_id: Some(machine_id),
        label: Some(reg.label),
        server_machine_id: (!server_machine_id.is_empty()).then_some(server_machine_id),
        negotiated_protocol: Some(pending.negotiated_protocol),
        negotiated_capabilities: pending.negotiated_capabilities,
    });
    info!(
        conn_id = reg.connection_id.0,
        client_id = reg.client_id.0,
        label = state.label.as_deref().unwrap_or(""),
        compress,
        "client authenticated"
    );
    Flow::Continue
}

/// Handle [`ClientMessage::ChannelReady`].
pub(super) fn on_channel_ready(state: &mut SharedClientState) {
    // The previous transport was captured in `state.pending_swap_from`
    // by the Auth handler at the moment register_client swapped it
    // out. Consuming it here ensures the `ChannelSwitched` reply
    // names the genuine prior transport, even if the registry's
    // recorded transport has since changed (e.g. a third channel
    // arrived). `take` clears the field so a stray duplicate
    // ChannelReady doesn't re-emit a stale switch event.
    if let Some(old) = state.pending_swap_from.take() {
        state.send(ServerMessage::ChannelSwitched {
            old_transport: old.to_string(),
        });
    }
}
