//! Layout and focus — the four fire-and-forget tree mutations.
//!
//! None of them carries a `request_id`, so their replies are a broadcast to
//! everyone viewing the tab rather than an answer to the sender.

use kmux_protocol::messages::{LayoutNode, LayoutScheme, TabIndex, WordId};

use crate::connection::classify_error;

use super::super::SharedClientState;

/// Handle [`ClientMessage::PaneSwap`].
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

/// Handle [`ClientMessage::SetLayoutRatios`].
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

/// Handle [`ClientMessage::ApplyLayoutScheme`].
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

/// Handle [`ClientMessage::SetFocus`].
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
