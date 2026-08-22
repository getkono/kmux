//! Layout and focus — the four fire-and-forget tree mutations.
//!
//! None of them carries a `request_id`, so their replies are a broadcast to
//! everyone viewing the tab rather than an answer to the sender.

use kmux_protocol::messages::{LayoutNode, LayoutScheme, TabIndex, WordId};

use crate::connection::classify_error;

use super::super::SharedClientState;

/// Handle [`ClientMessage::PaneSwap`](kmux_protocol::messages::ClientMessage::PaneSwap).
pub(super) async fn on_pane_swap(
    state: &mut SharedClientState,
    word_id: WordId,
    tab_index: TabIndex,
    a: u32,
    b: u32,
) {
    let result = state.app.swap_panes(&word_id, tab_index, a, b).await;
    answer_layout_change(state, &word_id, tab_index, result);
}

/// Handle [`ClientMessage::SetLayoutRatios`](kmux_protocol::messages::ClientMessage::SetLayoutRatios).
pub(super) async fn on_set_layout_ratios(
    state: &mut SharedClientState,
    word_id: WordId,
    tab_index: TabIndex,
    path: Vec<u32>,
    ratios: Vec<u16>,
) {
    let result = state
        .app
        .set_layout_ratios(&word_id, tab_index, &path, &ratios)
        .await;
    answer_layout_change(state, &word_id, tab_index, result);
}

/// Handle [`ClientMessage::ApplyLayoutScheme`](kmux_protocol::messages::ClientMessage::ApplyLayoutScheme).
pub(super) async fn on_apply_layout_scheme(
    state: &mut SharedClientState,
    word_id: WordId,
    tab_index: TabIndex,
    scheme: LayoutScheme,
) {
    let result = state
        .app
        .apply_layout_scheme(&word_id, tab_index, scheme)
        .await;
    answer_layout_change(state, &word_id, tab_index, result);
}

/// Handle [`ClientMessage::SetFocus`](kmux_protocol::messages::ClientMessage::SetFocus).
pub(super) async fn on_set_focus(
    state: &mut SharedClientState,
    word_id: WordId,
    tab_index: TabIndex,
    pane_index: u32,
) {
    let result = state
        .app
        .set_tab_focus(&word_id, tab_index, pane_index)
        .await;
    answer_layout_change(state, &word_id, tab_index, result);
}

/// Answer one of the four layout-mutating messages.
///
/// On success the daemon's new authoritative tree goes to everyone viewing the
/// tab; on failure the requester is told. All four carry no `request_id` — they
/// are fire-and-forget layout nudges — so the error is unaddressed, the same
/// shape `Resize`, `Signal` and `PtyKeyBatch` already use.
fn answer_layout_change(
    state: &mut SharedClientState,
    word_id: &str,
    tab_index: u32,
    result: kmux_pty::error::Result<(LayoutNode, u32)>,
) {
    match result {
        Ok((layout, focused)) => state
            .app
            .broadcast_layout(word_id, tab_index, layout, focused),
        Err(e) => state.error(None, classify_error(&e), e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;

    #[tokio::test]
    async fn pane_swap_in_an_unknown_session_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::PaneSwap {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            a: 0,
            b: 1,
        })
        .await;
        assert!(keep);
        // No `request_id` on the wire for the four layout nudges, so the error
        // is unaddressed — the shape `Resize` and `Signal` already use.
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    /// The other half of the four layout arms: a call that succeeds must still
    /// broadcast the daemon's new authoritative tree and send the requester
    /// nothing. Without this, an arm that reported every outcome as an error
    /// would pass the four not-found tests above.
    #[tokio::test]
    async fn a_layout_change_that_succeeds_broadcasts_and_answers_nothing() {
        let (app, word, mut state, mut ctrl_rx) = app_with_one_session().await;
        let mut events = app.subscribe_vt_events();

        let keep = handle_message(
            &mut state,
            ClientMessage::SetFocus {
                word_id: word.clone(),
                tab_index: 0,
                pane_index: 0,
            },
            &NoopAttacher,
        )
        .await;
        assert!(keep);
        assert!(
            drain(&mut ctrl_rx).is_empty(),
            "a successful layout change is answered by the broadcast, not a reply"
        );
        match events.try_recv().expect("a LayoutUpdate was broadcast") {
            ServerMessage::LayoutUpdate {
                word_id,
                tab_index,
                focused_pane,
                ..
            } => {
                assert_eq!(word_id, word);
                assert_eq!(tab_index, 0);
                assert_eq!(focused_pane, 0);
            }
            other => panic!("expected LayoutUpdate, got {other:?}"),
        }

        let _ = app.close_session(&word).await;
    }

    #[tokio::test]
    async fn set_layout_ratios_in_an_unknown_session_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetLayoutRatios {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            path: vec![],
            ratios: vec![500, 500],
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn apply_layout_scheme_in_an_unknown_session_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::ApplyLayoutScheme {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            scheme: LayoutScheme::EvenHorizontal,
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn set_focus_in_an_unknown_session_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetFocus {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            pane_index: 0,
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }
}
