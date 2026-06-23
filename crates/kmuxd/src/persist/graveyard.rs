//! Closed-session graveyard persistence (issue #64).
//!
//! The graveyard is the set of retained, *inactive* sessions a user can
//! restore. It lives in its own file (`closed.bin`), separate from the live
//! checkpoint, because closed snapshots are large and immutable: folding them
//! into the 30 s checkpoint would re-serialize them thousands of times a day for
//! no benefit. This file is rewritten only when the graveyard set changes (a
//! close, a restore, or a prune that actually dropped an entry).

use std::path::Path;

use super::{GRAVEYARD_VERSION, PersistedGraveyard};

/// Atomically write the graveyard to `path`.
///
/// Writes to a `.tmp` sibling first, then renames into place so a crash during
/// the write cannot corrupt the existing graveyard file (mirrors
/// [`super::checkpoint::write_checkpoint`]).
pub fn write_graveyard(graveyard: &PersistedGraveyard, path: &Path) -> anyhow::Result<()> {
    let bytes = postcard::to_allocvec(graveyard)
        .map_err(|e| anyhow::anyhow!("graveyard serialization failed: {e}"))?;

    let tmp_path = path.with_extension("bin.tmp");
    std::fs::write(&tmp_path, &bytes).map_err(|e| {
        anyhow::anyhow!(
            "failed to write graveyard tmp file {}: {e}",
            tmp_path.display()
        )
    })?;

    std::fs::rename(&tmp_path, path)
        .map_err(|e| anyhow::anyhow!("failed to rename graveyard file {}: {e}", path.display()))?;

    Ok(())
}

/// Read and deserialize the graveyard from `path`.
///
/// A missing file is not an error — it yields an empty graveyard (the common
/// case on first run). A newer on-disk version is refused so a downgrade does
/// not silently drop sessions it cannot represent.
pub fn read_graveyard(path: &Path) -> anyhow::Result<PersistedGraveyard> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PersistedGraveyard::default());
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to read graveyard {}: {e}",
                path.display()
            ));
        }
    };

    let graveyard: PersistedGraveyard = postcard::from_bytes(&bytes)
        .map_err(|e| anyhow::anyhow!("failed to deserialize graveyard: {e}"))?;

    if graveyard.version > GRAVEYARD_VERSION {
        anyhow::bail!(
            "graveyard version {} is newer than supported version {GRAVEYARD_VERSION}; \
             upgrade kmuxd to read it",
            graveyard.version
        );
    }

    Ok(graveyard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::{
        PersistedClosedSession, PersistedPane, PersistedSession, PersistedTermSize,
    };
    use kmux_protocol::messages::{
        CursorState, GridSnapshot, LayoutNode, SessionMeta, SessionStatus, TermModes,
    };

    fn sample_closed(word: &str, closed_at_ms: u64) -> PersistedClosedSession {
        PersistedClosedSession {
            closed_at_ms,
            session: PersistedSession {
                meta: SessionMeta {
                    index: 0,
                    word_id: word.to_string(),
                    name: word.to_string(),
                    cwd: "/tmp".to_string(),
                },
                next_pane_index: 1,
                panes: vec![PersistedPane {
                    pane_index: 0,
                    program: "/bin/sh".to_string(),
                    args: vec![],
                    size: PersistedTermSize { rows: 24, cols: 80 },
                    status: SessionStatus::Running,
                    child_pid: None,
                    grid: GridSnapshot {
                        rows: 24,
                        cols: 80,
                        cells: vec![Default::default(); 24 * 80],
                        cursor: CursorState::default(),
                        modes: TermModes::EMPTY,
                        history_total: 0,
                        scrollback_base: 0,
                        scrollback_tail: Vec::new(),
                    },
                    scrollback_lines: vec![],
                    cwd: "/tmp".to_string(),
                }],
                tabs: vec![crate::persist::PersistedTab {
                    tab_index: 0,
                    name: "1".to_string(),
                    layout: LayoutNode::single(0),
                    focused_pane: 0,
                }],
                next_tab_index: 1,
                active_tab: 0,
                last_active_ms: closed_at_ms,
            },
        }
    }

    #[test]
    fn roundtrip() {
        let gy = PersistedGraveyard {
            version: GRAVEYARD_VERSION,
            sessions: vec![sample_closed("eagle", 100), sample_closed("falcon", 200)],
        };
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("closed.bin");

        write_graveyard(&gy, &path).unwrap();
        let decoded = read_graveyard(&path).unwrap();
        assert_eq!(decoded.sessions.len(), 2);
        assert_eq!(decoded.sessions[0].session.meta.word_id, "eagle");
        assert_eq!(decoded.sessions[1].closed_at_ms, 200);
    }

    #[test]
    fn missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.bin");
        let gy = read_graveyard(&path).unwrap();
        assert!(gy.sessions.is_empty());
        assert_eq!(gy.version, GRAVEYARD_VERSION);
    }

    #[test]
    fn atomic_tmp_gone_after_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("closed.bin");
        write_graveyard(&PersistedGraveyard::default(), &path).unwrap();
        assert!(path.exists());
        assert!(!tmp.path().join("closed.bin.tmp").exists());
    }
}
