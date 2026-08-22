//! Pane lifecycle within a tab: create, close, split.

use kmux_protocol::messages::{
    ClientId, PaneId, RequestId, ServerMessage, SessionEventMsg, SplitDir, TabIndex, WordId,
};
use kmux_protocol::parse_pane_id;

use crate::app::PaneCloseOutcome;
use crate::connection::classify_error;

use super::super::SharedClientState;
use super::Spawn;

/// `PaneCreate` is the legacy "new pane" intent; under the Session → Tab
/// → Pane model it creates a new TAB (with one pane). The reply still
/// names the new pane so existing clients attach to it as before.
pub(super) async fn on_pane_create(
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
        Ok((tab, pane)) => {
            let tab_index = tab.tab_index;
            state.send(ServerMessage::PaneCreated {
                request_id,
                pane_id: pane.pane_id,
                session_word_id: word_id.clone(),
                size,
            });
            state
                .app
                .broadcast_session_event(SessionEventMsg::TabCreated { word_id, tab_index });
        }
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::PaneClose`].
pub(super) async fn on_pane_close(
    state: &mut SharedClientState,
    client_id: ClientId,
    request_id: RequestId,
    pane_id: PaneId,
) {
    if let Some(handle) = state.attached.remove(&pane_id) {
        handle.abort();
    }
    state.app.detach_from_pane(&pane_id, client_id).await;
    match state.app.close_pane(&pane_id).await {
        Ok((exit_code, outcome)) => {
            state.send(ServerMessage::PaneClosed {
                request_id,
                pane_id: pane_id.clone(),
                exit_code,
            });
            let word_id = parse_pane_id(&pane_id)
                .map(|(w, _)| w.to_string())
                .unwrap_or_default();
            match outcome {
                PaneCloseOutcome::TabUpdated {
                    tab_index,
                    layout,
                    focused_pane,
                } => state
                    .app
                    .broadcast_layout(&word_id, tab_index, layout, focused_pane),
                PaneCloseOutcome::TabClosed { tab_index } => state
                    .app
                    .broadcast_session_event(SessionEventMsg::TabClosed { word_id, tab_index }),
                PaneCloseOutcome::SessionClosed => state
                    .app
                    .broadcast_session_event(SessionEventMsg::SessionClosed { word_id }),
                PaneCloseOutcome::Gone => {}
            }
        }
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::PaneSplit`].
pub(super) async fn on_pane_split(
    state: &mut SharedClientState,
    request_id: RequestId,
    word_id: WordId,
    tab_index: TabIndex,
    from_pane: u32,
    dir: SplitDir,
    spawn: Spawn,
) {
    let Spawn {
        program,
        args,
        size,
    } = spawn;
    match state
        .app
        .split_pane(
            &word_id,
            tab_index,
            from_pane,
            dir,
            program,
            args,
            size,
            &state.capabilities,
        )
        .await
    {
        Ok((new_pane, layout, focused)) => {
            state.send(ServerMessage::PaneSplit {
                request_id,
                word_id: word_id.clone(),
                tab_index,
                new_pane,
                layout: layout.clone(),
            });
            state
                .app
                .broadcast_layout(&word_id, tab_index, layout, focused);
        }
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}
