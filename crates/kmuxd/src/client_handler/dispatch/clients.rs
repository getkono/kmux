//! Who else is attached to a session, and kicking one of them off (issue #146).

use kmux_protocol::messages::{ClientId, ErrorCode, RequestId, ServerMessage, WordId};

use crate::app::KickOutcome;

use super::super::SharedClientState;

/// Handle [`ClientMessage::ClientList`](kmux_protocol::messages::ClientMessage::ClientList).
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

/// Handle [`ClientMessage::KickClient`](kmux_protocol::messages::ClientMessage::KickClient).
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

#[cfg(test)]
mod tests {
    use super::super::testing::*;

    #[tokio::test]
    async fn client_list_for_an_unknown_session_errors_with_the_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::ClientList {
            request_id: 18,
            word_id: MISSING_WORD.to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(18));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn kick_client_in_an_unknown_session_errors_with_the_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::KickClient {
            request_id: 19,
            word_id: MISSING_WORD.to_string(),
            client_id: ClientId(42),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(19));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    /// The other `KickClient` failure: the session is real, the client id is
    /// not attached to it. Its message names both, so a caller looking at a log
    /// line can tell which of the two was wrong.
    #[tokio::test]
    async fn kicking_a_client_that_is_not_attached_names_the_client_and_the_session() {
        let (app, word, mut state, mut ctrl_rx) = app_with_one_session().await;

        let keep = handle_message(
            &mut state,
            ClientMessage::KickClient {
                request_id: 21,
                word_id: word.clone(),
                client_id: ClientId(42),
            },
            &NoopAttacher,
        )
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(drain(&mut ctrl_rx));
        assert_eq!(request_id, Some(21));
        assert_eq!(code, ErrorCode::ClientNotFound);
        assert_eq!(message, format!("client 42 not attached to session {word}"));

        let _ = app.close_session(&word).await;
    }
}
