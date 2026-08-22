//! Session lifecycle: create, close, list, restore, rename.

use kmux_protocol::messages::{
    ClientId, ErrorCode, PeerId, RequestId, ServerMessage, SessionEventMsg, WordId,
};

use crate::connection::classify_error;

use super::super::SharedClientState;
use super::Spawn;

/// Handle [`ClientMessage::SessionCreate`](kmux_protocol::messages::ClientMessage::SessionCreate).
pub(super) async fn on_session_create(
    state: &mut SharedClientState,
    request_id: RequestId,
    name: Option<String>,
    cwd: Option<String>,
    spawn: Spawn,
    peer: Option<PeerId>,
) {
    let Spawn {
        program,
        args,
        size,
    } = spawn;
    match peer {
        // Create on a federated peer (issue #121 launcher): the hub forwards
        // the create upstream and registers the result under a local word,
        // then replies SessionCreated exactly as for a local create.
        Some(peer) => match state
            .app
            .create_remote_session(&peer, name, cwd, program, args, size)
            .await
        {
            Ok(entry) => state.send(ServerMessage::SessionCreated { request_id, entry }),
            Err(e) => state.error(Some(request_id), ErrorCode::InternalError, e),
        },
        None => match state
            .app
            .create_session(name, cwd, program, args, size, &state.capabilities)
            .await
        {
            Ok(entry) => state.send(ServerMessage::SessionCreated { request_id, entry }),
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },
    }
}

/// Handle [`ClientMessage::SessionClose`](kmux_protocol::messages::ClientMessage::SessionClose).
pub(super) async fn on_session_close(
    state: &mut SharedClientState,
    client_id: ClientId,
    request_id: RequestId,
    word_id: WordId,
) {
    let pane_ids: Vec<String> = state
        .attached
        .keys()
        .filter(|k| k.starts_with(&format!("{word_id}/")))
        .cloned()
        .collect();
    for pane_id in &pane_ids {
        if let Some(handle) = state.attached.remove(pane_id) {
            handle.abort();
        }
        state.app.detach_from_pane(pane_id, client_id).await;
    }
    match state.app.close_session(&word_id).await {
        Ok(exit_code) => state.send(ServerMessage::SessionClosed {
            request_id,
            word_id,
            exit_code,
        }),
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::SessionList`](kmux_protocol::messages::ClientMessage::SessionList).
pub(super) async fn on_session_list(state: &mut SharedClientState, request_id: RequestId) {
    // Merge locally-hosted sessions with every open peer's proxied
    // sessions (local IDs, peer-decorated names). Federation off ⇒ the
    // federated list is empty and this is the original behaviour.
    let mut sessions = state.app.list_sessions().await;
    sessions.extend(state.app.list_federated_sessions());
    state.send(ServerMessage::SessionListResult {
        request_id,
        sessions,
    });
}

/// Closed-session restore (issue #64). The graveyard is local-only, so
/// these are not federated.
pub(super) fn on_session_list_closed(state: &mut SharedClientState, request_id: RequestId) {
    state.send(ServerMessage::ClosedSessionListResult {
        request_id,
        sessions: state.app.closed_session_entries(),
    });
}

/// Handle [`ClientMessage::SessionRestore`](kmux_protocol::messages::ClientMessage::SessionRestore).
pub(super) async fn on_session_restore(
    state: &mut SharedClientState,
    request_id: RequestId,
    word_id: WordId,
) {
    match state.app.restore_session(&word_id).await {
        Ok(entry) => {
            let restored = entry.meta.word_id.clone();
            state.send(ServerMessage::SessionCreated { request_id, entry });
            state
                .app
                .broadcast_session_event(SessionEventMsg::SessionCreated { word_id: restored });
        }
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::SessionRename`](kmux_protocol::messages::ClientMessage::SessionRename).
pub(super) async fn on_session_rename(
    state: &mut SharedClientState,
    request_id: RequestId,
    word_id: WordId,
    new_name: String,
) {
    match state.app.rename_session(&word_id, &new_name).await {
        Ok(()) => state.send(ServerMessage::SessionRenamed { word_id, new_name }),
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;

    #[tokio::test]
    async fn session_create_on_an_unknown_peer_errors_naming_the_peer() {
        // Only the federated branch is exercised: the local branch spawns a real
        // PTY, which a unit test must not do.
        let (keep, msgs) = dispatch_one(ClientMessage::SessionCreate {
            request_id: 1,
            name: None,
            cwd: None,
            program: None,
            args: vec![],
            size: TermSize::default(),
            peer: Some("nosuchpeer".to_string()),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(1));
        assert_eq!(code, ErrorCode::InternalError);
        assert_eq!(message, "peer nosuchpeer is not connected");
    }

    #[tokio::test]
    async fn session_close_of_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::SessionClose {
            request_id: 2,
            word_id: MISSING_WORD.to_string(),
        })
        .await;
        assert!(keep);
        // A `SessionClosed` reply here would be indistinguishable from a real
        // close, which is what the client treats as confirmation.
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(2));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn session_list_on_an_empty_server_returns_an_empty_list() {
        let (keep, msgs) = dispatch_one(ClientMessage::SessionList { request_id: 9 }).await;
        assert!(keep);
        match only(msgs) {
            ServerMessage::SessionListResult {
                request_id,
                sessions,
            } => {
                assert_eq!(request_id, 9);
                assert!(sessions.is_empty(), "no sessions exist: {sessions:?}");
            }
            other => panic!("expected SessionListResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_list_closed_on_an_empty_server_returns_an_empty_graveyard() {
        let (keep, msgs) = dispatch_one(ClientMessage::SessionListClosed { request_id: 10 }).await;
        assert!(keep);
        match only(msgs) {
            ServerMessage::ClosedSessionListResult {
                request_id,
                sessions,
            } => {
                assert_eq!(request_id, 10);
                assert!(sessions.is_empty(), "the graveyard is empty: {sessions:?}");
            }
            other => panic!("expected ClosedSessionListResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_restore_of_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::SessionRestore {
            request_id: 11,
            word_id: MISSING_WORD.to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(11));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn session_rename_of_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::SessionRename {
            request_id: 13,
            word_id: MISSING_WORD.to_string(),
            new_name: "renamed".to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(13));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }
}
