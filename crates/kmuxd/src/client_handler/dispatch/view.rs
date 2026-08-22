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

#[cfg(test)]
mod tests {
    use super::super::testing::*;

    #[tokio::test]
    async fn attach_to_an_unknown_pane_errors_and_starts_no_stream() {
        let (mut state, mut ctrl_rx) = authenticated_client().await;
        let keep = handle_message(
            &mut state,
            ClientMessage::Attach {
                pane_id: MISSING_PANE.to_string(),
                last_seqno: None,
                size: TermSize::default(),
            },
            &NoopAttacher,
        )
        .await;
        assert!(keep);
        assert!(
            state.attached.is_empty(),
            "a failed attach registers no forwarding task"
        );
        let (request_id, code, message) = only_error(drain(&mut ctrl_rx));
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn detach_from_a_pane_this_client_never_attached_answers_nothing() {
        let (keep, msgs) = dispatch_one(ClientMessage::Detach {
            pane_id: MISSING_PANE.to_string(),
        })
        .await;
        assert!(keep);
        assert!(msgs.is_empty(), "nothing was attached: {msgs:?}");
    }

    #[tokio::test]
    async fn set_snapshot_mode_is_applied_without_a_reply() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetSnapshotMode { enabled: true }).await;
        assert!(keep);
        assert!(
            msgs.is_empty(),
            "snapshot mode is a silent connection setting: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn set_paused_is_applied_without_a_reply() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetPaused {
            paused: true,
            auto: false,
        })
        .await;
        assert!(keep);
        assert!(
            msgs.is_empty(),
            "pausing is a silent connection setting: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn set_pane_no_auto_pause_for_an_unknown_pane_answers_nothing() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetPaneNoAutoPause {
            pane_id: MISSING_PANE.to_string(),
            exempt: true,
        })
        .await;
        assert!(keep);
        // The exemption is a per-client preference, recorded without validating
        // that the pane exists.
        assert!(msgs.is_empty(), "no reply is defined: {msgs:?}");
    }

    #[tokio::test]
    async fn fetch_history_for_an_unknown_pane_errors_with_the_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::FetchHistory {
            request_id: 14,
            pane_id: MISSING_PANE.to_string(),
            start_index: 0,
            count: 10,
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(14));
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }
}
