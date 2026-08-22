//! Tab lifecycle within a session.

use kmux_protocol::messages::{RequestId, ServerMessage, SessionEventMsg, TabIndex, WordId};

use crate::connection::classify_error;

use super::super::SharedClientState;
use super::Spawn;

/// Handle [`ClientMessage::TabCreate`].
pub(super) async fn on_tab_create(
    state: &mut SharedClientState,
    request_id: RequestId,
    word_id: WordId,
    spawn: Spawn,
) {
    let Spawn {
        program,
        args,
        size,
    } = spawn;
    match state
        .app
        .create_tab(&word_id, program, args, size, &state.capabilities)
        .await
    {
        Ok((tab, _pane)) => {
            let tab_index = tab.tab_index;
            state.send(ServerMessage::TabCreated {
                request_id,
                word_id: word_id.clone(),
                tab,
            });
            state
                .app
                .broadcast_session_event(SessionEventMsg::TabCreated { word_id, tab_index });
        }
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::TabClose`].
pub(super) async fn on_tab_close(
    state: &mut SharedClientState,
    request_id: RequestId,
    word_id: WordId,
    tab_index: TabIndex,
) {
    match state.app.close_tab(&word_id, tab_index).await {
        Ok(session_closed) => {
            state.send(ServerMessage::TabClosed {
                request_id,
                word_id: word_id.clone(),
                tab_index,
            });
            if session_closed {
                state
                    .app
                    .broadcast_session_event(SessionEventMsg::SessionClosed { word_id });
            } else {
                state
                    .app
                    .broadcast_session_event(SessionEventMsg::TabClosed { word_id, tab_index });
            }
        }
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::TabRename`].
pub(super) async fn on_tab_rename(
    state: &mut SharedClientState,
    request_id: RequestId,
    word_id: WordId,
    tab_index: TabIndex,
    new_name: String,
) {
    match state.app.rename_tab(&word_id, tab_index, &new_name).await {
        Ok(()) => state
            .app
            .broadcast_session_event(SessionEventMsg::TabRenamed {
                word_id,
                tab_index,
                name: new_name,
            }),
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::TabReorder`].
pub(super) async fn on_tab_reorder(
    state: &mut SharedClientState,
    word_id: WordId,
    tab_index: TabIndex,
    new_position: u32,
) {
    match state
        .app
        .reorder_tab(&word_id, tab_index, new_position)
        .await
    {
        Ok(tab_indices) => state
            .app
            .broadcast_session_event(SessionEventMsg::TabsReordered {
                word_id,
                tab_indices,
            }),
        Err(e) => state.error(None, classify_error(&e), e.to_string()),
    }
}
