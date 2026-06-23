use std::collections::HashMap;
use std::path::{Path, PathBuf};

use kmux_pty::error::{KmuxError, Result};

use super::{PaneRelay, SessionState};

/// Look up a pane relay by `pane_id` in a read-locked sessions map.
pub(super) fn get_pane_relay<'a>(
    sessions: &'a HashMap<String, SessionState>,
    pane_id: &str,
) -> Result<&'a PaneRelay> {
    let (word_id, pane_index) =
        parse_pane_id(pane_id).ok_or_else(|| KmuxError::SessionNotFound {
            name: pane_id.to_string(),
        })?;
    let state = sessions
        .get(word_id)
        .ok_or_else(|| KmuxError::SessionNotFound {
            name: pane_id.to_string(),
        })?;
    state
        .panes
        .get(&pane_index)
        .ok_or_else(|| KmuxError::SessionNotFound {
            name: pane_id.to_string(),
        })
}

/// Look up a pane relay mutably by `pane_id` in a write-locked sessions map.
pub(super) fn get_pane_relay_mut<'a>(
    sessions: &'a mut HashMap<String, SessionState>,
    pane_id: &str,
) -> Result<&'a mut PaneRelay> {
    let (word_id, pane_index) =
        parse_pane_id(pane_id).ok_or_else(|| KmuxError::SessionNotFound {
            name: pane_id.to_string(),
        })?;
    let state = sessions
        .get_mut(word_id)
        .ok_or_else(|| KmuxError::SessionNotFound {
            name: pane_id.to_string(),
        })?;
    state
        .panes
        .get_mut(&pane_index)
        .ok_or_else(|| KmuxError::SessionNotFound {
            name: pane_id.to_string(),
        })
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
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}
