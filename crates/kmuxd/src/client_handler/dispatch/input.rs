//! Everything that reaches a PTY: keys, paste, resize, signals, and the
//! exclusive input lock that arbitrates between clients competing for one pane.

use kmux_protocol::messages::{ClientId, KeyEvent, PaneId, ServerMessage, TermSize};

use crate::app::{InputLockOutcome, Requester};
use crate::connection::classify_error;

use super::super::SharedClientState;

/// Handle [`ClientMessage::PtyInput`](kmux_protocol::messages::ClientMessage::PtyInput).
pub(super) async fn on_pty_input(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
    data: Vec<u8>,
) {
    if let Err(e) = state.app.write_input(&pane_id, client_id, data).await {
        state.error(None, classify_error(&e), e.to_string());
    }
}

/// Handle [`ClientMessage::PtyPaste`](kmux_protocol::messages::ClientMessage::PtyPaste).
pub(super) async fn on_pty_paste(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
    data: String,
) {
    if let Err(e) = state.app.write_paste(&pane_id, client_id, data).await {
        state.error(None, classify_error(&e), e.to_string());
    }
}

/// Handle [`ClientMessage::PtyKeyBatch`](kmux_protocol::messages::ClientMessage::PtyKeyBatch).
pub(super) async fn on_pty_key_batch(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
    events: Vec<KeyEvent>,
) {
    if let Err(e) = state
        .app
        .write_key_batch(&pane_id, client_id, &events)
        .await
    {
        state.error(None, classify_error(&e), e.to_string());
    }
}

/// Handle [`ClientMessage::Resize`](kmux_protocol::messages::ClientMessage::Resize).
pub(super) async fn on_resize(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
    size: TermSize,
) {
    // Federated panes reconcile smallest-wins across local viewers inside
    // the peer subsystem (which forwards at most one upstream Resize),
    // rather than forwarding this client's size verbatim.
    if state.app.is_federated_pane(&pane_id) {
        state.app.federated_resize(&pane_id, client_id, size);
    } else if let Err(e) = state.app.resize(&pane_id, client_id, size).await {
        state.error(None, classify_error(&e), e.to_string());
    }
}

/// Handle [`ClientMessage::Signal`](kmux_protocol::messages::ClientMessage::Signal).
pub(super) async fn on_signal(state: &mut SharedClientState, pane_id: PaneId, signal: i32) {
    if let Err(e) = state.app.send_signal(&pane_id, signal).await {
        state.error(None, classify_error(&e), e.to_string());
    }
}

/// Handle [`ClientMessage::RequestInputLock`](kmux_protocol::messages::ClientMessage::RequestInputLock).
pub(super) async fn on_request_input_lock(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
) {
    if state.app.is_federated_pane(&pane_id) {
        let from = Requester::client(client_id, state.ctrl_tx.clone());
        if let Err(refusal) = state.app.federated_request_input_lock(&from, &pane_id) {
            state.send(refusal.into_error());
        }
        return;
    }
    match state.app.request_input_lock(&pane_id, client_id).await {
        Ok(InputLockOutcome::Granted) => {
            state.send(ServerMessage::InputLockGranted { pane_id });
        }
        Ok(InputLockOutcome::Denied(holder)) => {
            state.send(ServerMessage::InputLockDenied { pane_id, holder });
        }
        Err(e) => state.error(None, classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::ReleaseInputLock`](kmux_protocol::messages::ClientMessage::ReleaseInputLock).
pub(super) async fn on_release_input_lock(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
) {
    if state.app.is_federated_pane(&pane_id) {
        let from = Requester::client(client_id, state.ctrl_tx.clone());
        if let Err(refusal) = state.app.federated_release_input_lock(&from, &pane_id) {
            state.send(refusal.into_error());
        }
        return;
    }
    match state.app.release_input_lock(&pane_id, client_id).await {
        Ok(true) => state.send(ServerMessage::InputLockReleased { pane_id }),
        Ok(false) => {}
        Err(e) => state.error(None, classify_error(&e), e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;

    #[tokio::test]
    async fn pty_input_to_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::PtyInput {
            pane_id: MISSING_PANE.to_string(),
            data: b"x".to_vec(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn pty_paste_to_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::PtyPaste {
            pane_id: MISSING_PANE.to_string(),
            data: "x".to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn pty_key_batch_to_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::PtyKeyBatch {
            pane_id: MISSING_PANE.to_string(),
            events: vec![one_key()],
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn an_empty_pty_key_batch_to_an_unknown_pane_still_errors() {
        let (keep, msgs) = dispatch_one(ClientMessage::PtyKeyBatch {
            pane_id: MISSING_PANE.to_string(),
            events: vec![],
        })
        .await;
        assert!(keep);
        // Whether the pane exists cannot depend on how many keys were sent.
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn resize_of_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::Resize {
            pane_id: MISSING_PANE.to_string(),
            size: TermSize::default(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn signal_to_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::Signal {
            pane_id: MISSING_PANE.to_string(),
            signal: 15,
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn request_input_lock_on_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::RequestInputLock {
            pane_id: MISSING_PANE.to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn release_input_lock_on_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::ReleaseInputLock {
            pane_id: MISSING_PANE.to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    /// A proxied pane's input lock is the hub's to arbitrate (issue #227):
    /// the request goes to the peer, the grant makes this client the holder,
    /// and the holder's release goes to the peer too.
    #[cfg(feature = "federation")]
    #[tokio::test(start_paused = true)]
    async fn a_proxied_panes_input_lock_is_taken_and_released_through_the_peer() {
        let (mut state, mut ctrl_rx) = authenticated_client().await;
        let (mut upstream, peer) = state.app.install_channel_peer("fedlocal", "fedremote");
        let pane_id = || "fedlocal/0".to_string();
        let (data_tx, _data_rx) = tokio::sync::mpsc::channel(8);
        let client_id = state.client_id.expect("authenticated");
        assert!(state.app.federated_attach(
            &pane_id(),
            client_id,
            data_tx,
            state.ctrl_tx.clone(),
            TermSize::default(),
        ));
        while upstream.try_recv().is_ok() {}

        let request = ClientMessage::RequestInputLock { pane_id: pane_id() };
        assert!(handle_message(&mut state, request, &NoopAttacher).await);
        assert!(matches!(
            upstream.try_recv(),
            Ok(ClientMessage::RequestInputLock { pane_id }) if pane_id == "fedremote/0"
        ));
        peer.send(ServerMessage::InputLockGranted {
            pane_id: "fedremote/0".to_string(),
        })
        .unwrap();
        let granted = tokio::time::timeout(std::time::Duration::from_secs(60), ctrl_rx.recv())
            .await
            .expect("the grant within the bound");
        assert!(matches!(
            granted,
            Some(ServerMessage::InputLockGranted { pane_id }) if pane_id == "fedlocal/0"
        ));

        let release = ClientMessage::ReleaseInputLock { pane_id: pane_id() };
        assert!(handle_message(&mut state, release, &NoopAttacher).await);
        assert!(matches!(
            upstream.try_recv(),
            Ok(ClientMessage::ReleaseInputLock { pane_id }) if pane_id == "fedremote/0"
        ));
    }
}
