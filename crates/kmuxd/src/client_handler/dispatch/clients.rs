//! Who else is attached to a session, and kicking one of them off (issue #146).

use kmux_protocol::messages::{ClientId, ErrorCode, RequestId, ServerMessage, WordId};

use crate::app::KickOutcome;

use super::super::SharedClientState;

/// Handle [`ClientMessage::ClientList`].
pub(super) async fn on_client_list(
    state: &mut SharedClientState,
    client_id: ClientId,
    request_id: RequestId,
    word_id: WordId,
) {
    // Federated session ⇒ forward to the owning peer; otherwise build the
    // list from this daemon's own connections (issue #146).
    if state.app.is_federated_session(&word_id) {
        match state.app.list_federated_session_clients(&word_id).await {
            Ok(clients) => state.send(ServerMessage::ClientListResult {
                request_id,
                word_id,
                clients,
            }),
            Err(reason) => {
                state.error(Some(request_id), ErrorCode::SessionNotFound, reason);
            }
        }
    } else {
        match state.app.list_session_clients(&word_id, client_id).await {
            Some(clients) => state.send(ServerMessage::ClientListResult {
                request_id,
                word_id,
                clients,
            }),
            None => state.error(
                Some(request_id),
                ErrorCode::SessionNotFound,
                format!("session not found: {word_id}"),
            ),
        }
    }
}

/// Handle [`ClientMessage::KickClient`].
pub(super) async fn on_kick_client(
    state: &mut SharedClientState,
    _client_id: ClientId,
    request_id: RequestId,
    word_id: WordId,
    target: ClientId,
) {
    if state.app.is_federated_session(&word_id) {
        match state.app.kick_federated_client(&word_id, target).await {
            Ok(()) => state.send(ServerMessage::ClientKicked {
                request_id,
                word_id,
                client_id: target,
            }),
            Err(reason) => state.error(Some(request_id), ErrorCode::ClientNotFound, reason),
        }
    } else {
        let by_label = state.label.clone().unwrap_or_default();
        match state
            .app
            .kick_client_from_session(&word_id, target, &by_label)
            .await
        {
            KickOutcome::Kicked => state.send(ServerMessage::ClientKicked {
                request_id,
                word_id,
                client_id: target,
            }),
            KickOutcome::SessionNotFound => state.error(
                Some(request_id),
                ErrorCode::SessionNotFound,
                format!("session not found: {word_id}"),
            ),
            KickOutcome::ClientNotFound => state.error(
                Some(request_id),
                ErrorCode::ClientNotFound,
                format!("client {} not attached to session {word_id}", target.0),
            ),
        }
    }
}
