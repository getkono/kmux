//! Session lifecycle: create, close, list, restore, rename.

use kmux_protocol::messages::{
    ClientId, ErrorCode, PeerId, RequestId, ServerMessage, SessionEventMsg, WordId,
};

use crate::connection::classify_error;

use super::super::SharedClientState;
use super::Spawn;

/// Handle [`ClientMessage::SessionCreate`].
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

/// Handle [`ClientMessage::SessionClose`].
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

/// Handle [`ClientMessage::SessionList`].
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

/// Handle [`ClientMessage::SessionRestore`].
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

/// Handle [`ClientMessage::SessionRename`].
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
