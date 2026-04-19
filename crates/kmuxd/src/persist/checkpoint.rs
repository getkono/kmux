//! Checkpoint writing: serialize daemon state to disk atomically.

use std::path::Path;

use super::PersistedDaemonState;

/// Atomically write a [`PersistedDaemonState`] to `path`.
///
/// Writes to a `.tmp` sibling first, then renames into place so that a crash
/// during the write does not corrupt the existing checkpoint file.
pub fn write_checkpoint(state: &PersistedDaemonState, path: &Path) -> anyhow::Result<()> {
    let bytes = postcard::to_allocvec(state)
        .map_err(|e| anyhow::anyhow!("checkpoint serialization failed: {e}"))?;

    let tmp_path = path.with_extension("bin.tmp");
    std::fs::write(&tmp_path, &bytes).map_err(|e| {
        anyhow::anyhow!(
            "failed to write checkpoint tmp file {}: {e}",
            tmp_path.display()
        )
    })?;

    std::fs::rename(&tmp_path, path)
        .map_err(|e| anyhow::anyhow!("failed to rename checkpoint file {}: {e}", path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::PersistedTermSize;
    use crate::persist::STATE_VERSION;
    use kmux_protocol::messages::{
        CursorState, GridSnapshot, SessionMeta, SessionStatus, TermModes,
    };

    fn empty_state() -> PersistedDaemonState {
        PersistedDaemonState {
            version: STATE_VERSION,
            session_index_counter: 0,
            used_words: vec![],
            sessions: vec![],
        }
    }

    fn one_session_state() -> PersistedDaemonState {
        PersistedDaemonState {
            version: STATE_VERSION,
            session_index_counter: 1,
            used_words: vec!["eagle".to_string()],
            sessions: vec![crate::persist::PersistedSession {
                meta: SessionMeta {
                    index: 0,
                    word_id: "eagle".to_string(),
                    name: "test".to_string(),
                    cwd: "/tmp".to_string(),
                },
                next_pane_index: 1,
                panes: vec![crate::persist::PersistedPane {
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
                        scrollback_tail: Vec::new(),
                    },
                    scrollback_lines: vec![],
                    cwd: "/tmp".to_string(),
                }],
            }],
        }
    }

    #[test]
    fn write_and_read_roundtrip() {
        let state = one_session_state();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");

        write_checkpoint(&state, &path).expect("write_checkpoint failed");
        assert!(path.exists(), "state.bin should exist after write");

        let bytes = std::fs::read(&path).unwrap();
        let decoded: PersistedDaemonState = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.version, STATE_VERSION);
        assert_eq!(decoded.used_words, vec!["eagle"]);
        assert_eq!(decoded.sessions[0].meta.word_id, "eagle");
    }

    #[test]
    fn atomic_write_tmp_gone_after_rename() {
        let state = empty_state();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");

        assert!(!path.exists());
        write_checkpoint(&state, &path).unwrap();

        // state.bin exists, .tmp is gone.
        assert!(path.exists());
        assert!(!tmp.path().join("state.bin.tmp").exists());
    }

    #[test]
    fn overwrite_existing_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");

        // Write once.
        let state1 = empty_state();
        write_checkpoint(&state1, &path).unwrap();

        // Write again with different data.
        let mut state2 = one_session_state();
        state2.session_index_counter = 99;
        write_checkpoint(&state2, &path).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let decoded: PersistedDaemonState = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.session_index_counter, 99);
    }
}
