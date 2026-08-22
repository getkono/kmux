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
        Ok(exit_code) => {
            state.send(ServerMessage::SessionClosed {
                request_id,
                word_id: word_id.clone(),
                exit_code,
            });
            // Everyone else has to hear about it too. `TabClose` already
            // broadcasts, and a session closing is the larger event: without
            // this, another GUI keeps the session in its list -- as an entry
            // whose panes drain one by one and then sits there empty -- until
            // something unrelated makes it re-list.
            state
                .app
                .broadcast_session_event(SessionEventMsg::SessionClosed { word_id });
        }
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
        Ok(()) => {
            state.send(ServerMessage::SessionRenamed {
                word_id: word_id.clone(),
                new_name: new_name.clone(),
            });
            // A name is shared state: every client showing this session in a
            // picker or a tab bar is displaying the old one until it is told.
            // `TabRename` already broadcasts; this did not, so a rename was
            // visible only to whoever performed it.
            state
                .app
                .broadcast_session_event(SessionEventMsg::SessionRenamed { word_id, new_name });
        }
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

    /// A session closing is not the requester's private news. Every other GUI
    /// showing it keeps a stale entry — one whose panes drain away and then sits
    /// there empty — until something unrelated makes it re-list.
    #[tokio::test]
    async fn closing_a_session_tells_every_client_not_only_the_requester() {
        let (app, word, mut state, mut ctrl_rx) = app_with_one_session().await;
        let mut events = app.subscribe_vt_events();

        let keep = handle_message(
            &mut state,
            ClientMessage::SessionClose {
                request_id: 30,
                word_id: word.clone(),
            },
            &NoopAttacher,
        )
        .await;
        assert!(keep);

        // The requester still gets its correlated reply.
        match only(drain(&mut ctrl_rx)) {
            ServerMessage::SessionClosed {
                request_id,
                word_id: replied,
                ..
            } => {
                assert_eq!(request_id, 30);
                assert_eq!(replied, word);
            }
            other => panic!("expected SessionClosed, got {other:?}"),
        }

        // And everyone else hears it on the server-wide channel.
        let broadcast = broadcast_event(&mut events, |e| match e {
            SessionEventMsg::SessionClosed { word_id } => Some(word_id),
            _ => None,
        })
        .expect("the close was broadcast to every client");
        assert_eq!(broadcast, word);
    }

    /// A name is shared state: a rename by one GUI has to reach the others, or
    /// their pickers and tab bars keep showing the old one.
    #[tokio::test]
    async fn renaming_a_session_tells_every_client_not_only_the_renamer() {
        let (app, word, mut state, mut ctrl_rx) = app_with_one_session().await;
        let mut events = app.subscribe_vt_events();

        let keep = handle_message(
            &mut state,
            ClientMessage::SessionRename {
                request_id: 31,
                word_id: word.clone(),
                new_name: "builds".to_string(),
            },
            &NoopAttacher,
        )
        .await;
        assert!(keep);

        match only(drain(&mut ctrl_rx)) {
            ServerMessage::SessionRenamed { word_id, new_name } => {
                assert_eq!(word_id, word);
                assert_eq!(new_name, "builds");
            }
            other => panic!("expected SessionRenamed, got {other:?}"),
        }

        let broadcast = broadcast_event(&mut events, |e| match e {
            SessionEventMsg::SessionRenamed { word_id, new_name } => Some((word_id, new_name)),
            _ => None,
        })
        .expect("the rename was broadcast to every client");
        assert_eq!(broadcast, (word.clone(), "builds".to_string()));

        let _ = app.close_session(&word).await;
    }
}
