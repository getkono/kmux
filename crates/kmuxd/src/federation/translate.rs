//! Pure remote↔local ID translation for the federation feed loop.
//!
//! These helpers rewrite the session word and pane IDs carried by upstream
//! frames into their local equivalents (and read them back). They are
//! deliberately pure — no I/O, no locks — so the feed loop's translation logic
//! is unit-testable in isolation (see the tests in the parent module).

use std::collections::HashMap;

use kmux_protocol::messages::{
    ClientInfo, PaneInfo, RequestId, ServerMessage, SessionEntry, SessionEventMsg,
};
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
        localize_pane(pane, local_word);
    }
    entry
}

/// A peer's pane in its local form: under `local_word`, with no attached
/// clients (the peer's client ids mean nothing here) and a progress state the
/// hub knows.
fn localize_pane(pane: &mut PaneInfo, local_word: &str) {
    pane.pane_id = format_pane_id(local_word, pane.pane_index);
    pane.attached_clients.clear();
    pane.progress_state = pane.progress_state.sendable();
}

/// A peer's client list, with any frontend this hub does not know reported
/// as one it does: the hub never sends `Unknown` on.
pub(super) fn sendable_clients(mut clients: Vec<ClientInfo>) -> Vec<ClientInfo> {
    for client in &mut clients {
        client.frontend = client.frontend.sendable();
    }
    clients
}

/// Rewrite the word (or pane) a [`SessionEventMsg`] references from remote to
/// local, returning the local word for routing. `None` when the referenced word
/// is not federated (e.g. an event for a remote session we never registered).
pub(super) fn rewrite_event_to_local(
    event: &mut SessionEventMsg,
    remote_to_local: &HashMap<String, String>,
) -> Option<String> {
    use SessionEventMsg::*;
    // A value this hub does not know is passed on as the one it is shown as:
    // the hub never sends `Unknown` on.
    match event {
        PaneProgressChanged { state, .. } => *state = state.sendable(),
        PaneAttention { kind, .. } => *kind = kind.sendable(),
        _ => {}
    }
    match event {
        PaneSpawned { pane_id }
        | PaneExited { pane_id, .. }
        | PaneResized { pane_id, .. }
        | PaneTitleChanged { pane_id, .. }
        | PaneBell { pane_id }
        | PaneProgressChanged { pane_id, .. }
        | PaneClipboardCopy { pane_id, .. }
        | PaneClosed { pane_id }
        | PaneFaulted { pane_id }
        | PaneAttention { pane_id, .. } => {
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
        | TabsReordered { word_id, .. } => {
            let local_word = remote_to_local.get(word_id.as_str())?.clone();
            *word_id = local_word.clone();
            Some(local_word)
        }
        // A newer peer's event this hub cannot route: dropped, like one for
        // a session it never registered.
        Unknown => None,
    }
}

/// The pane a frame of a pane's stream is for; `None` for anything else.
/// Replies naming a pane (`HistoryLines`, the lock replies) are not stream
/// frames: they go to their requester alone (issue #227).
pub(super) fn msg_pane_id(msg: &ServerMessage) -> Option<&str> {
    use ServerMessage::*;
    match msg {
        TerminalUpdate { pane_id, .. }
        | TerminalSnapshot { pane_id, .. }
        | CursorUpdate { pane_id, .. }
        | ScrollbackAppend { pane_id, .. }
        | GridDigest { pane_id, .. }
        | SyncReset { pane_id }
        | Lagged { pane_id, .. } => Some(pane_id.as_str()),
        _ => None,
    }
}

/// Overwrite the pane a stream frame is for (a no-op for anything else).
pub(super) fn set_msg_pane_id(msg: &mut ServerMessage, new_id: String) {
    use ServerMessage::*;
    match msg {
        TerminalUpdate { pane_id, .. }
        | TerminalSnapshot { pane_id, .. }
        | CursorUpdate { pane_id, .. }
        | ScrollbackAppend { pane_id, .. }
        | GridDigest { pane_id, .. }
        | SyncReset { pane_id }
        | Lagged { pane_id, .. } => *pane_id = new_id,
        _ => warn!("set_msg_pane_id called on a message with no pane_id"),
    }
}

/// The request id an answer to a forwarded request carries; `None` for
/// anything else, or an `Error` answering a request without one.
pub(super) fn reply_request_id(msg: &ServerMessage) -> Option<RequestId> {
    use ServerMessage::*;
    match msg {
        SessionCreated { request_id, .. }
        | SessionClosed { request_id, .. }
        | PaneCreated { request_id, .. }
        | PaneClosed { request_id, .. }
        | TabCreated { request_id, .. }
        | TabClosed { request_id, .. }
        | PaneSplit { request_id, .. }
        | HistoryLines { request_id, .. }
        | ClientListResult { request_id, .. }
        | ClientKicked { request_id, .. }
        | NotifyAccepted { request_id }
        | Error {
            request_id: Some(request_id),
            ..
        } => Some(*request_id),
        _ => None,
    }
}

/// Put the requester's own id back on an answer (or an `Error`) the hub
/// routes to it.
pub(super) fn set_reply_request_id(msg: &mut ServerMessage, id: RequestId) {
    use ServerMessage::*;
    match msg {
        SessionCreated { request_id, .. }
        | SessionClosed { request_id, .. }
        | PaneCreated { request_id, .. }
        | PaneClosed { request_id, .. }
        | TabCreated { request_id, .. }
        | TabClosed { request_id, .. }
        | PaneSplit { request_id, .. }
        | HistoryLines { request_id, .. }
        | ClientListResult { request_id, .. }
        | ClientKicked { request_id, .. }
        | NotifyAccepted { request_id } => *request_id = id,
        Error { request_id, .. } => *request_id = Some(id),
        _ => {}
    }
}

/// The peer's word for the session an answer without a request id names,
/// if it names one (an `Error` names none).
pub(super) fn reply_word(msg: &ServerMessage) -> Option<&str> {
    use ServerMessage::*;
    match msg {
        SessionRenamed { word_id, .. } => Some(word_id),
        InputLockGranted { pane_id }
        | InputLockDenied { pane_id, .. }
        | InputLockReleased { pane_id } => parse_pane_id(pane_id).map(|(word, _)| word),
        _ => None,
    }
}

/// Rewrite the session an answer names — its word, or its panes' — to
/// `local_word`, the requester's word for it; and pass on a client list's
/// frontends as ones the hub knows.
pub(super) fn localize_reply(msg: &mut ServerMessage, local_word: &str) {
    use ServerMessage::*;
    let pane = |pane_id: &mut String| {
        if let Some((_, idx)) = parse_pane_id(pane_id) {
            *pane_id = format_pane_id(local_word, idx);
        }
    };
    match msg {
        PaneCreated {
            pane_id,
            session_word_id,
            ..
        } => {
            pane(pane_id);
            *session_word_id = local_word.to_string();
        }
        PaneClosed { pane_id, .. }
        | HistoryLines { pane_id, .. }
        | InputLockGranted { pane_id }
        | InputLockDenied { pane_id, .. }
        | InputLockReleased { pane_id } => pane(pane_id),
        ClientListResult {
            word_id, clients, ..
        } => {
            *word_id = local_word.to_string();
            *clients = sendable_clients(std::mem::take(clients));
        }
        PaneSplit {
            word_id, new_pane, ..
        } => {
            *word_id = local_word.to_string();
            localize_pane(new_pane, local_word);
        }
        SessionClosed { word_id, .. }
        | SessionRenamed { word_id, .. }
        | TabCreated { word_id, .. }
        | TabClosed { word_id, .. }
        | ClientKicked { word_id, .. } => *word_id = local_word.to_string(),
        _ => {}
    }
}
