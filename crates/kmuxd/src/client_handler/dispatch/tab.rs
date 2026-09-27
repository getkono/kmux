//! Tab lifecycle within a session.

use kmux_protocol::messages::{RequestId, ServerMessage, SessionEventMsg, TabIndex, WordId};

use crate::connection::classify_error;

use super::super::SharedClientState;
use super::Spawn;

/// Handle [`ClientMessage::TabCreate`](kmux_protocol::messages::ClientMessage::TabCreate).
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

/// Handle [`ClientMessage::TabClose`](kmux_protocol::messages::ClientMessage::TabClose).
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

/// Handle [`ClientMessage::TabRename`](kmux_protocol::messages::ClientMessage::TabRename).
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

/// Handle [`ClientMessage::TabReorder`](kmux_protocol::messages::ClientMessage::TabReorder).
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

#[cfg(test)]
mod tests {
    use super::super::testing::*;

    #[tokio::test]
    async fn tab_create_for_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::TabCreate {
            request_id: 5,
            word_id: MISSING_WORD.to_string(),
            program: None,
            args: vec![],
            size: TermSize::default(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(5));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn tab_close_of_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::TabClose {
            request_id: 6,
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
        })
        .await;
        assert!(keep);
        // A `TabClosed` reply also suppresses the session-event broadcast that
        // follows it, so the old answer was a success the rest of the fleet
        // never heard about.
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(6));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn tab_rename_for_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::TabRename {
            request_id: 7,
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            new_name: "renamed".to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(7));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn tab_reorder_for_an_unknown_session_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::TabReorder {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            new_position: 1,
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        // `TabReorder` carries no request id, so the error cannot correlate.
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }
}
