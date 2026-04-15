//! Checkpoint reading and version validation.

use std::path::Path;

use super::{PersistedDaemonState, PersistedPane, PersistedSession, STATE_VERSION};

/// Read and deserialize a checkpoint file from `path`.
///
/// # Errors
/// - I/O error reading the file.
/// - Deserialization failure (corrupt or truncated file).
/// - Version mismatch (`version > STATE_VERSION`).
pub fn read_checkpoint(path: &Path) -> anyhow::Result<PersistedDaemonState> {
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("failed to read checkpoint {}: {e}", path.display()))?;

    // Peek at just the version field first (it is always the first varint in
    // the postcard-serialized struct).  Then deserialize using the appropriate
    // versioned schema so that older checkpoints can be migrated.
    #[derive(serde::Deserialize)]
    struct VersionProbe {
        version: u32,
    }
    let probe: VersionProbe = postcard::from_bytes(&bytes)
        .map_err(|e| anyhow::anyhow!("failed to read checkpoint version: {e}"))?;

    match probe.version {
        1 => {
            let v1: v1::PersistedDaemonState = postcard::from_bytes(&bytes)
                .map_err(|e| anyhow::anyhow!("failed to deserialize v1 checkpoint: {e}"))?;
            Ok(migrate_v1_to_v2(v1))
        }
        v if v <= STATE_VERSION => {
            let state: PersistedDaemonState = postcard::from_bytes(&bytes)
                .map_err(|e| anyhow::anyhow!("failed to deserialize checkpoint: {e}"))?;
            Ok(state)
        }
        v => anyhow::bail!(
            "checkpoint version {v} is newer than supported version {STATE_VERSION}; \
             upgrade kmuxd to restore this checkpoint"
        ),
    }
}

/// Migrate a v1 checkpoint to the current (`STATE_VERSION`) schema.
///
/// v1 → v2: `PersistedPane` gains an `args: Vec<String>` field; default to empty.
fn migrate_v1_to_v2(v1: v1::PersistedDaemonState) -> PersistedDaemonState {
    PersistedDaemonState {
        version: STATE_VERSION,
        session_index_counter: v1.session_index_counter,
        used_words: v1.used_words,
        sessions: v1
            .sessions
            .into_iter()
            .map(|s| PersistedSession {
                meta: s.meta,
                next_pane_index: s.next_pane_index,
                panes: s
                    .panes
                    .into_iter()
                    .map(|p| PersistedPane {
                        pane_index: p.pane_index,
                        program: p.program,
                        args: vec![],
                        size: p.size,
                        status: p.status,
                        child_pid: p.child_pid,
                        grid: p.grid,
                        scrollback_lines: p.scrollback_lines,
                        cwd: p.cwd,
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// v1 schema types — used only for migration (and test fixtures).
mod v1 {
    use kmux_protocol::messages::{CellState, GridSnapshot, SessionMeta, SessionStatus, TermSize};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    pub struct PersistedDaemonState {
        pub version: u32,
        pub session_index_counter: u32,
        pub sessions: Vec<PersistedSession>,
        pub used_words: Vec<String>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct PersistedSession {
        pub meta: SessionMeta,
        pub next_pane_index: u32,
        pub panes: Vec<PersistedPane>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct PersistedPane {
        pub pane_index: u32,
        pub program: String,
        pub size: TermSize,
        pub status: SessionStatus,
        pub child_pid: Option<i32>,
        pub grid: GridSnapshot,
        pub scrollback_lines: Vec<Vec<CellState>>,
        pub cwd: String,
    }
}

/// Check whether a process with the given PID is still alive.
///
/// Uses `kill(pid, 0)` which sends no signal but verifies the process exists
/// and is reachable. Returns `false` for PID 0 (kernel) as a safety guard.
#[cfg(test)]
pub fn is_process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        None, // signal 0 = existence check only
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::{PersistedDaemonState, STATE_VERSION};

    fn write_state(state: &PersistedDaemonState, path: &std::path::Path) {
        let bytes = postcard::to_allocvec(state).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn empty_v2_state() -> PersistedDaemonState {
        PersistedDaemonState {
            version: STATE_VERSION,
            session_index_counter: 5,
            used_words: vec!["eagle".to_string()],
            sessions: vec![],
        }
    }

    #[test]
    fn read_valid_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");

        write_state(&empty_v2_state(), &path);

        let loaded = read_checkpoint(&path).unwrap();
        assert_eq!(loaded.session_index_counter, 5);
        assert_eq!(loaded.used_words, vec!["eagle"]);
    }

    #[test]
    fn reject_newer_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");

        let state = PersistedDaemonState {
            version: STATE_VERSION + 1, // future version
            session_index_counter: 0,
            used_words: vec![],
            sessions: vec![],
        };
        write_state(&state, &path);

        let result = read_checkpoint(&path);
        assert!(result.is_err(), "should reject newer version");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("newer"),
            "error message should mention 'newer'"
        );
    }

    #[test]
    fn missing_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.bin");
        let result = read_checkpoint(&path);
        assert!(result.is_err());
    }

    #[test]
    fn corrupt_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");
        std::fs::write(&path, b"not valid postcard data!!!").unwrap();
        let result = read_checkpoint(&path);
        assert!(result.is_err());
    }

    #[test]
    fn migrate_v1_checkpoint_to_v2() {
        use kmux_protocol::messages::{
            CursorState, GridSnapshot, SessionMeta, SessionStatus, TermModes, TermSize,
        };

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");

        // Write a v1-schema checkpoint manually.
        let v1_state = v1::PersistedDaemonState {
            version: 1,
            session_index_counter: 3,
            used_words: vec!["eagle".to_string()],
            sessions: vec![v1::PersistedSession {
                meta: SessionMeta {
                    index: 0,
                    word_id: "eagle".to_string(),
                    name: "old-session".to_string(),
                    cwd: "/tmp".to_string(),
                },
                next_pane_index: 1,
                panes: vec![v1::PersistedPane {
                    pane_index: 0,
                    program: "/bin/zsh".to_string(),
                    size: TermSize { rows: 24, cols: 80 },
                    status: SessionStatus::Running,
                    child_pid: Some(999),
                    grid: GridSnapshot {
                        rows: 24,
                        cols: 80,
                        cells: vec![Default::default(); 24 * 80],
                        cursor: CursorState::default(),
                        modes: TermModes::EMPTY,
                    },
                    scrollback_lines: vec![],
                    cwd: "/tmp".to_string(),
                }],
            }],
        };
        let bytes = postcard::to_allocvec(&v1_state).unwrap();
        std::fs::write(&path, bytes).unwrap();

        let migrated = read_checkpoint(&path).unwrap();
        assert_eq!(migrated.version, STATE_VERSION);
        assert_eq!(migrated.sessions[0].panes[0].program, "/bin/zsh");
        assert_eq!(migrated.sessions[0].panes[0].args, Vec::<String>::new());
    }

    #[test]
    fn is_process_alive_current_process() {
        // The current process is definitely alive.
        let pid = std::process::id() as i32;
        assert!(is_process_alive(pid));
    }

    #[test]
    fn is_process_alive_zero_pid_is_false() {
        assert!(!is_process_alive(0));
    }

    #[test]
    fn is_process_alive_negative_pid_is_false() {
        assert!(!is_process_alive(-1));
    }
}
