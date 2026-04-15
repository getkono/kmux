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

/// Parse a pane ID `"{word_id}/{pane_index}"` into its components.
pub fn parse_pane_id(pane_id: &str) -> Option<(&str, u32)> {
    let (word, idx_str) = pane_id.rsplit_once('/')?;
    let idx: u32 = idx_str.parse().ok()?;
    Some((word, idx))
}

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
