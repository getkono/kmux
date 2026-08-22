//! Per-client view state: which panes this connection is attached to, and how
//! much of each pane's stream it wants.

use kmux_protocol::messages::{
    ClientId, ClientMessage, ErrorCode, PaneId, RequestId, SequenceNo, ServerMessage, TermSize,
    epoch_millis,
};
use tokio::sync::mpsc;
use tracing::debug;

use crate::app::{AttachParams, AttachResult};
use crate::connection::classify_error;

use super::super::{CLIENT_CHANNEL_CAPACITY, PaneAttacher, SharedClientState};

/// Handle [`ClientMessage::Attach`].
pub(super) async fn on_attach<A: PaneAttacher>(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
    last_seqno: Option<SequenceNo>,
    size: TermSize,
    attacher: &A,
) {
    // If already attached, detach first (routes to the peer subsystem
    // for federated panes, the local relay otherwise).
    if let Some(old) = state.attached.remove(&pane_id) {
        old.abort();
        state.app.detach_pane_any(&pane_id, client_id).await;
    }

    let (client_tx, client_rx) = mpsc::channel::<ServerMessage>(CLIENT_CHANNEL_CAPACITY);

    if state.app.is_federated_pane(&pane_id) {
        // Federated pane: register as a viewer and forward the `Attach`
        // upstream. The remote's snapshot and diffs arrive asynchronously
        // through the peer feed loop and are pumped to this client via
        // `client_rx`, so the synchronous replay is empty — `Delta(vec![])`
        // emits no initial frames (see `build_attach_replay`).
        state.app.federated_attach(
            &pane_id,
            client_id,
            client_tx,
            state.ctrl_tx.clone(),
            last_seqno,
            size,
        );
        match attacher
            .start_pane_stream(pane_id.clone(), AttachResult::Delta(vec![]), client_rx)
            .await
        {
            Ok(handle) => {
                state.attached.insert(pane_id, handle);
            }
            Err(e) => state.error(None, ErrorCode::InternalError, e),
        }
    } else {
        match state
            .app
            .attach(AttachParams {
                pane_id: pane_id.clone(),
                client_id,
                last_seqno,
                size,
                data_tx: client_tx,
                ctrl_tx: state.ctrl_tx.clone(),
                capabilities: state.capabilities.clone(),
            })
            .await
        {
            Ok(result) => {
                match attacher
                    .start_pane_stream(pane_id.clone(), result, client_rx)
                    .await
                {
                    Ok(handle) => {
                        state.attached.insert(pane_id, handle);
                    }
                    Err(e) => {
                        state.error(None, ErrorCode::InternalError, e);
                    }
                }
            }
            Err(e) => state.error(None, classify_error(&e), e.to_string()),
        }
    }
}

/// Handle [`ClientMessage::Detach`].
pub(super) async fn on_detach(state: &mut SharedClientState, client_id: ClientId, pane_id: PaneId) {
    if let Some(handle) = state.attached.remove(&pane_id) {
        handle.abort();
        state.app.detach_pane_any(&pane_id, client_id).await;
        debug!("detached from pane '{pane_id}'");
    }
}

/// Handle [`ClientMessage::SetSnapshotMode`].
pub(super) async fn on_set_snapshot_mode(
    state: &mut SharedClientState,
    client_id: ClientId,
    enabled: bool,
) {
    state.app.set_snapshot_mode(client_id, enabled).await;
    debug!("client {client_id:?} snapshot mode = {enabled}");
}

/// Handle [`ClientMessage::SetPaused`].
pub(super) async fn on_set_paused(
    state: &mut SharedClientState,
    client_id: ClientId,
    paused: bool,
    auto: bool,
) {
    state.app.set_paused(client_id, paused, auto).await;
    debug!("client {client_id:?} paused = {paused} (auto = {auto})");
}

/// Handle [`ClientMessage::SetPaneNoAutoPause`].
pub(super) async fn on_set_pane_no_auto_pause(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
    exempt: bool,
) {
    state
        .app
        .set_pane_no_auto_pause(client_id, &pane_id, exempt)
        .await;
    debug!("client {client_id:?} pane {pane_id} no_auto_pause = {exempt}");
}

/// Handle [`ClientMessage::FetchHistory`].
pub(super) async fn on_fetch_history(
    state: &mut SharedClientState,
    request_id: RequestId,
    pane_id: PaneId,
    start_index: u64,
    count: u32,
) {
    // For a federated pane, forward the request upstream; the remote's
    // `HistoryLines` reply is pane-scoped, so the feed loop translates it
    // back to this viewer (matched by `request_id`).
    if state.app.is_federated_pane(&pane_id) {
        state
            .app
            .forward_peer_message(&pane_id, move |remote| ClientMessage::FetchHistory {
                request_id,
                pane_id: remote,
                start_index,
                count,
            });
    } else {
        match state.app.fetch_history(&pane_id, start_index, count).await {
            Ok((first_index, lines, history_total)) => {
                state.send(ServerMessage::HistoryLines {
                    request_id,
                    pane_id,
                    first_index,
                    lines,
                    history_total,
                    sent_at_ms: epoch_millis(),
                });
            }
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        }
    }
}
