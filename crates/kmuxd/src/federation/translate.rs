//! Pure remote↔local ID translation for the federation feed loop.
//!
//! These helpers rewrite the session word and pane IDs carried by upstream
//! frames into their local equivalents (and read them back). They are
//! deliberately pure — no I/O, no locks — so the feed loop's translation logic
//! is unit-testable in isolation (see the tests in the parent module).

use std::collections::HashMap;

use kmux_protocol::messages::{ServerMessage, SessionEntry, SessionEventMsg};
use kmux_protocol::{format_pane_id, parse_pane_id};
use tracing::warn;

/// Rewrite a remote [`SessionEntry`] into its local form: a freshly-assigned
/// word, local pane IDs, a peer-decorated display name, and cleared
/// `attached_clients` (the remote's client IDs are meaningless locally).
pub(super) fn localize_entry(
    mut entry: SessionEntry,
    local_word: &str,
    peer_id: &str,
) -> SessionEntry {
    entry.meta.name = format!("{} @ {peer_id}", entry.meta.name);
    entry.meta.word_id = local_word.to_string();
    // Attribute the session to its peer so clients can group it by machine. The
    // name decoration above stays for now (older/CLI views still rely on it); a
    // frontend that groups by `peer` strips the decoration for display.
    entry.peer = Some(peer_id.to_string());
    for pane in &mut entry.panes {
        pane.pane_id = format_pane_id(local_word, pane.pane_index);
        pane.attached_clients.clear();
    }
    entry
}

/// Rewrite the word (or pane) a [`SessionEventMsg`] references from remote to
/// local, returning the local word for routing. `None` when the referenced word
/// is not federated (e.g. an event for a remote session we never registered).
pub(super) fn rewrite_event_to_local(
    event: &mut SessionEventMsg,
    remote_to_local: &HashMap<String, String>,
) -> Option<String> {
    use SessionEventMsg::*;
    match event {
        PaneSpawned { pane_id }
        | PaneExited { pane_id, .. }
        | PaneResized { pane_id, .. }
        | PaneTitleChanged { pane_id, .. }
        | PaneProgressChanged { pane_id, .. }
        | PaneClipboardCopy { pane_id, .. }
        | PaneClosed { pane_id }
        | PaneFaulted { pane_id } => {
            let (remote_word, idx) = parse_pane_id(pane_id)?;
            let local_word = remote_to_local.get(remote_word)?.clone();
            *pane_id = format_pane_id(&local_word, idx);
            Some(local_word)
        }
        SessionCreated { word_id }
        | SessionClosed { word_id }
        | SessionRenamed { word_id, .. }
        | TabCreated { word_id, .. }
        | TabClosed { word_id, .. }
        | TabRenamed { word_id, .. }
        | LayoutChanged { word_id, .. } => {
            let local_word = remote_to_local.get(word_id.as_str())?.clone();
            *word_id = local_word.clone();
            Some(local_word)
        }
    }
}

/// Borrow the single `pane_id` a [`ServerMessage`] carries, if any.
pub(super) fn msg_pane_id(msg: &ServerMessage) -> Option<&str> {
    use ServerMessage::*;
    match msg {
        TerminalUpdate { pane_id, .. }
        | TerminalSnapshot { pane_id, .. }
        | CursorUpdate { pane_id, .. }
        | ScrollbackAppend { pane_id, .. }
        | SyncReset { pane_id }
        | Lagged { pane_id, .. }
        | PaneCreated { pane_id, .. }
        | PaneClosed { pane_id, .. }
        | HistoryLines { pane_id, .. }
        | InputLockGranted { pane_id }
        | InputLockDenied { pane_id, .. }
        | InputLockReleased { pane_id } => Some(pane_id.as_str()),
        _ => None,
    }
}

/// Overwrite the `pane_id` a [`ServerMessage`] carries (no-op if it has none).
pub(super) fn set_msg_pane_id(msg: &mut ServerMessage, new_id: String) {
    use ServerMessage::*;
    match msg {
        TerminalUpdate { pane_id, .. }
        | TerminalSnapshot { pane_id, .. }
        | CursorUpdate { pane_id, .. }
        | ScrollbackAppend { pane_id, .. }
        | SyncReset { pane_id }
        | Lagged { pane_id, .. }
        | PaneCreated { pane_id, .. }
        | PaneClosed { pane_id, .. }
        | HistoryLines { pane_id, .. }
        | InputLockGranted { pane_id }
        | InputLockDenied { pane_id, .. }
        | InputLockReleased { pane_id } => *pane_id = new_id,
        _ => warn!("set_msg_pane_id called on a message with no pane_id"),
    }
}
