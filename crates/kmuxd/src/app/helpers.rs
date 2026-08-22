use std::collections::HashMap;
use std::path::{Path, PathBuf};

use kmux_pty::error::{KmuxError, Result};

use super::{PaneRelay, SessionState};

/// The error every pane lookup answers with when `pane_id` names no live pane.
///
/// One constructor for all three ways a lookup can miss — unparseable id,
/// unknown session, unknown index within a known session — because the client
/// asked about a pane and all three mean the same thing to it. Reporting them
/// apart would leak which sessions exist to a client that only guessed an id.
pub(super) fn pane_not_found(pane_id: &str) -> KmuxError {
    KmuxError::PaneNotFound {
        id: pane_id.to_string(),
    }
}

/// Re-label a PTY-registry miss as a pane miss.
///
/// [`kmux_pty::registry::SessionManager`] is a registry of *named* PTYs and
/// reports a miss as `SessionNotFound`; kmuxd names those PTYs by pane id, so
/// at this boundary the same miss means "no such pane". Every other error passes
/// through untouched — only the lookup failure is being renamed, not mapped.
pub(super) fn as_pane_error(pane_id: &str, e: KmuxError) -> KmuxError {
    match e {
        KmuxError::SessionNotFound { .. } => pane_not_found(pane_id),
        other => other,
    }
}

/// Look up a pane relay by `pane_id` in a read-locked sessions map.
pub(super) fn get_pane_relay<'a>(
    sessions: &'a HashMap<String, SessionState>,
    pane_id: &str,
) -> Result<&'a PaneRelay> {
    let (word_id, pane_index) = parse_pane_id(pane_id).ok_or_else(|| pane_not_found(pane_id))?;
    let state = sessions
        .get(word_id)
        .ok_or_else(|| pane_not_found(pane_id))?;
    state
        .panes
        .get(&pane_index)
        .ok_or_else(|| pane_not_found(pane_id))
}

/// Look up a pane relay mutably by `pane_id` in a write-locked sessions map.
pub(super) fn get_pane_relay_mut<'a>(
    sessions: &'a mut HashMap<String, SessionState>,
    pane_id: &str,
) -> Result<&'a mut PaneRelay> {
    let (word_id, pane_index) = parse_pane_id(pane_id).ok_or_else(|| pane_not_found(pane_id))?;
    let state = sessions
        .get_mut(word_id)
        .ok_or_else(|| pane_not_found(pane_id))?;
    state
        .panes
        .get_mut(&pane_index)
        .ok_or_else(|| pane_not_found(pane_id))
}

/// Stamp the owning session's `last_active` with the current time (issue #64).
///
/// Called from the input path under the `sessions` *read* lock — the timestamp
/// lives behind an `AtomicU64`, so no write lock is needed. A missing session or
/// unparseable pane id is a no-op (the input path reports those errors itself).
pub(super) fn touch_session_for_pane(sessions: &HashMap<String, SessionState>, pane_id: &str) {
    if let Some((word_id, _)) = parse_pane_id(pane_id)
        && let Some(state) = sessions.get(word_id)
    {
        state.last_active.store(
            kmux_protocol::messages::epoch_millis(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

/// Parse a pane ID `"{word_id}/{pane_index}"` into its components.
pub use kmux_protocol::parse_pane_id;

/// Walk up the directory tree to find the nearest existing ancestor.
pub(super) fn resolve_cwd(desired: &Path) -> PathBuf {
    let mut p = desired.to_path_buf();
    loop {
        if p.exists() {
            return p;
        }
        if !p.pop() {
            return home_dir();
        }
    }
}

/// Return the user's home directory, falling back to `/`.
pub(super) fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}
