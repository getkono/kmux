//! Checkpoint reading and version validation.

use std::path::Path;

use kmux_protocol::messages::LayoutNode;

use super::{PersistedDaemonState, PersistedPane, PersistedSession, PersistedTab, STATE_VERSION};

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
            Ok(migrate_v2_to_v3(migrate_v1_to_v2(v1)))
        }
        2 => {
            let v2: v2::PersistedDaemonState = postcard::from_bytes(&bytes)
                .map_err(|e| anyhow::anyhow!("failed to deserialize v2 checkpoint: {e}"))?;
            Ok(migrate_v2_to_v3(v2))
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

/// Migrate a v1 checkpoint to the v2 schema.
///
/// v1 → v2: `PersistedPane` gains an `args: Vec<String>` field; default to empty.
fn migrate_v1_to_v2(v1: v1::PersistedDaemonState) -> v2::PersistedDaemonState {
    v2::PersistedDaemonState {
        version: 2,
        session_index_counter: v1.session_index_counter,
        used_words: v1.used_words,
        sessions: v1
            .sessions
            .into_iter()
            .map(|s| v2::PersistedSession {
                meta: s.meta,
                next_pane_index: s.next_pane_index,
                panes: s
                    .panes
                    .into_iter()
                    .map(|p| v2::PersistedPane {
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

/// Migrate a v2 checkpoint to the current (`STATE_VERSION`) schema.
///
/// v2 → v3: sessions gain a tab layer. Each pre-tab pane was a separate
/// switchable view, so we wrap each pane in its own single-pane tab to preserve
/// that behavior exactly.
fn migrate_v2_to_v3(v2: v2::PersistedDaemonState) -> PersistedDaemonState {
    PersistedDaemonState {
        version: STATE_VERSION,
        session_index_counter: v2.session_index_counter,
        used_words: v2.used_words,
        sessions: v2
            .sessions
            .into_iter()
            .map(|s| {
                let (tabs, next_tab_index, active_tab) = wrap_panes_in_tabs(&s.panes);
                PersistedSession {
                    meta: s.meta,
                    next_pane_index: s.next_pane_index,
                    panes: s
                        .panes
                        .into_iter()
                        .map(|p| PersistedPane {
                            pane_index: p.pane_index,
                            program: p.program,
                            args: p.args,
                            size: p.size,
                            status: p.status,
                            child_pid: p.child_pid,
                            grid: p.grid,
                            scrollback_lines: p.scrollback_lines,
                            cwd: p.cwd,
                        })
                        .collect(),
                    tabs,
                    next_tab_index,
                    active_tab,
                }
            })
            .collect(),
    }
}

/// Build one single-pane tab per pane (the pre-tab "each pane is a view"
/// semantics). Returns `(tabs, next_tab_index, active_tab)`.
fn wrap_panes_in_tabs(panes: &[v2::PersistedPane]) -> (Vec<PersistedTab>, u32, u32) {
    let tabs: Vec<PersistedTab> = panes
        .iter()
        .enumerate()
        .map(|(i, p)| PersistedTab {
            tab_index: i as u32,
            name: format!("{}", i + 1),
            layout: LayoutNode::single(p.pane_index),
            focused_pane: p.pane_index,
        })
        .collect();
    let next_tab_index = tabs.len() as u32;
    (tabs, next_tab_index, 0)
}

/// v1 schema types — used only for migration (and test fixtures).
mod v1 {
    use crate::persist::PersistedTermSize;
    use kmux_protocol::messages::{CellState, GridSnapshot, SessionMeta, SessionStatus};
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
        pub size: PersistedTermSize,
        pub status: SessionStatus,
        pub child_pid: Option<i32>,
        pub grid: GridSnapshot,
        pub scrollback_lines: Vec<Vec<CellState>>,
        pub cwd: String,
    }
}

/// v2 schema types — the pre-tab layout. Used only for migration (and tests).
mod v2 {
    use crate::persist::PersistedTermSize;
    use kmux_protocol::messages::{CellState, GridSnapshot, SessionMeta, SessionStatus};
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
        pub args: Vec<String>,
        pub size: PersistedTermSize,
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
        use crate::persist::PersistedTermSize;
        use kmux_protocol::messages::{
            CursorState, GridSnapshot, SessionMeta, SessionStatus, TermModes,
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
                    size: PersistedTermSize { rows: 24, cols: 80 },
                    status: SessionStatus::Running,
                    child_pid: Some(999),
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
            }],
        };
        let bytes = postcard::to_allocvec(&v1_state).unwrap();
        std::fs::write(&path, bytes).unwrap();

        let migrated = read_checkpoint(&path).unwrap();
        assert_eq!(migrated.version, STATE_VERSION);
        assert_eq!(migrated.sessions[0].panes[0].program, "/bin/zsh");
        assert_eq!(migrated.sessions[0].panes[0].args, Vec::<String>::new());
        // v1 chains through v2→v3, so the single pane is wrapped in one tab.
        let session = &migrated.sessions[0];
        assert_eq!(session.tabs.len(), 1);
        assert_eq!(session.tabs[0].layout, LayoutNode::single(0));
        assert_eq!(session.tabs[0].focused_pane, 0);
        assert_eq!(session.next_tab_index, 1);
    }

    #[test]
    fn migrate_v2_checkpoint_wraps_panes_in_tabs() {
        use crate::persist::PersistedTermSize;
        use kmux_protocol::messages::{
            CursorState, GridSnapshot, SessionMeta, SessionStatus, TermModes,
        };

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");

        let mk_pane = |idx: u32| v2::PersistedPane {
            pane_index: idx,
            program: "/bin/zsh".to_string(),
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
        };

        // A v2 session with two panes — each must become its own tab.
        let v2_state = v2::PersistedDaemonState {
            version: 2,
            session_index_counter: 1,
            used_words: vec!["eagle".to_string()],
            sessions: vec![v2::PersistedSession {
                meta: SessionMeta {
                    index: 0,
                    word_id: "eagle".to_string(),
                    name: "s".to_string(),
                    cwd: "/tmp".to_string(),
                },
                next_pane_index: 2,
                panes: vec![mk_pane(0), mk_pane(1)],
            }],
        };
        let bytes = postcard::to_allocvec(&v2_state).unwrap();
        std::fs::write(&path, bytes).unwrap();

        let migrated = read_checkpoint(&path).unwrap();
        assert_eq!(migrated.version, STATE_VERSION);
        let session = &migrated.sessions[0];
        assert_eq!(session.panes.len(), 2);
        assert_eq!(session.tabs.len(), 2, "each pane becomes its own tab");
        assert_eq!(session.tabs[0].layout, LayoutNode::single(0));
        assert_eq!(session.tabs[1].layout, LayoutNode::single(1));
        assert_eq!(session.next_tab_index, 2);
        assert_eq!(session.active_tab, 0);
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
