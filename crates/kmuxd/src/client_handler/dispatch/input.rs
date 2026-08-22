//! Everything that reaches a PTY: keys, paste, resize, signals, and the
//! exclusive input lock that arbitrates between clients competing for one pane.

use kmux_protocol::messages::{ClientId, ClientMessage, KeyEvent, PaneId, ServerMessage, TermSize};

use crate::app::InputLockOutcome;
use crate::connection::classify_error;

use super::super::SharedClientState;

/// Handle [`ClientMessage::PtyInput`].
pub(super) async fn on_pty_input(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
    data: Vec<u8>,
) {
    if state.app.is_federated_pane(&pane_id) {
        state
            .app
            .forward_peer_message(&pane_id, move |remote| ClientMessage::PtyInput {
                pane_id: remote,
                data,
            });
    } else if let Err(e) = state.app.write_input(&pane_id, client_id, data).await {
        state.error(None, classify_error(&e), e.to_string());
    }
}

/// Handle [`ClientMessage::PtyPaste`].
pub(super) async fn on_pty_paste(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
    data: String,
) {
    if state.app.is_federated_pane(&pane_id) {
        state
            .app
            .forward_peer_message(&pane_id, move |remote| ClientMessage::PtyPaste {
                pane_id: remote,
                data,
            });
    } else if let Err(e) = state.app.write_paste(&pane_id, client_id, data).await {
        state.error(None, classify_error(&e), e.to_string());
    }
}

/// Handle [`ClientMessage::PtyKeyBatch`].
pub(super) async fn on_pty_key_batch(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
    events: Vec<KeyEvent>,
) {
    if state.app.is_federated_pane(&pane_id) {
        state
            .app
            .forward_peer_message(&pane_id, move |remote| ClientMessage::PtyKeyBatch {
                pane_id: remote,
                events,
            });
    } else if let Err(e) = state
        .app
        .write_key_batch(&pane_id, client_id, &events)
        .await
    {
        state.error(None, classify_error(&e), e.to_string());
    }
}

/// Handle [`ClientMessage::Resize`].
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

/// Handle [`ClientMessage::Signal`].
pub(super) async fn on_signal(state: &mut SharedClientState, pane_id: PaneId, signal: i32) {
    if state.app.is_federated_pane(&pane_id) {
        state
            .app
            .forward_peer_message(&pane_id, move |remote| ClientMessage::Signal {
                pane_id: remote,
                signal,
            });
    } else if let Err(e) = state.app.send_signal(&pane_id, signal).await {
        state.error(None, classify_error(&e), e.to_string());
    }
}

/// Handle [`ClientMessage::RequestInputLock`].
pub(super) async fn on_request_input_lock(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
) {
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

/// Handle [`ClientMessage::ReleaseInputLock`].
pub(super) async fn on_release_input_lock(
    state: &mut SharedClientState,
    client_id: ClientId,
    pane_id: PaneId,
) {
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
}
