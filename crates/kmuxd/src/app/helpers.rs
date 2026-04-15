use std::path::{Path, PathBuf};

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
