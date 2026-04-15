//! Session persistence: serialize/deserialize daemon state to disk so that
//! sessions survive daemon restarts and crashes.
//!
//! All types in this module use only `kmux-protocol` message types to ensure
//! zero coupling to the wezterm backend. When the backend is swapped, the
//! persistence layer requires no changes.

pub mod checkpoint;
pub mod restore;

use kmux_protocol::messages::{CellState, GridSnapshot, SessionMeta, SessionStatus, TermSize};
use serde::{Deserialize, Serialize};

/// Current format version. Increment when making breaking schema changes.
///
/// On restore:
/// - `version == STATE_VERSION`: deserialize as-is.
/// - `version < STATE_VERSION`: run migration chain.
/// - `version > STATE_VERSION`: refuse (written by a newer daemon).
pub const STATE_VERSION: u32 = 2;

/// Top-level persisted daemon state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedDaemonState {
    /// Format version (see [`STATE_VERSION`]).
    pub version: u32,
    /// Value of the daemon's `session_index_counter` at checkpoint time.
    pub session_index_counter: u32,
    /// All sessions at checkpoint time.
    pub sessions: Vec<PersistedSession>,
    /// Word IDs that were in use at checkpoint time.
    ///
    /// Used to restore the wordlist's "already allocated" set so that
    /// the restored daemon does not hand out duplicate session IDs.
    pub used_words: Vec<String>,
}

/// One persisted session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    /// Session-level metadata (word_id, name, cwd, index).
    pub meta: SessionMeta,
    /// Next pane index to assign within this session.
    pub next_pane_index: u32,
    /// All panes in this session.
    pub panes: Vec<PersistedPane>,
}

/// One persisted pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedPane {
    /// Pane index within its session.
    pub pane_index: u32,
    /// Program that was running in this pane (e.g. `/bin/zsh`).
    pub program: String,
    /// Arguments passed to the program at spawn time.
    pub args: Vec<String>,
    /// Terminal dimensions at checkpoint time.
    pub size: TermSize,
    /// Pane lifecycle status at checkpoint time.
    pub status: SessionStatus,
    /// Child process PID at checkpoint time (`None` if already exited).
    ///
    /// Retained for informational purposes; no longer used for reattachment.
    pub child_pid: Option<i32>,
    /// Full terminal grid snapshot at checkpoint time.
    pub grid: GridSnapshot,
    /// Scrollback history lines at checkpoint time (oldest first).
    ///
    /// Capped at [`MAX_SCROLLBACK_LINES`] to bound checkpoint size.
    pub scrollback_lines: Vec<Vec<CellState>>,
    /// Working directory of the pane at checkpoint time.
    pub cwd: String,
}

/// Maximum number of scrollback lines to persist per pane.
///
/// The backend supports up to 50,000 lines; we cap persistence at 10,000
/// to keep checkpoint files reasonably sized while still being useful.
pub const MAX_SCROLLBACK_LINES: usize = 10_000;

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::{CellColor, CursorState, SessionMeta, SessionStatus, TermModes};

    fn sample_state() -> PersistedDaemonState {
        PersistedDaemonState {
            version: STATE_VERSION,
            session_index_counter: 3,
            used_words: vec!["eagle".to_string(), "falcon".to_string()],
            sessions: vec![PersistedSession {
                meta: SessionMeta {
                    index: 0,
                    word_id: "eagle".to_string(),
                    name: "my-session".to_string(),
                    cwd: "/home/user/project".to_string(),
                },
                next_pane_index: 2,
                panes: vec![PersistedPane {
                    pane_index: 0,
                    program: "/bin/bash".to_string(),
                    args: vec![],
                    size: TermSize { rows: 24, cols: 80 },
                    status: SessionStatus::Running,
                    child_pid: Some(12345),
                    grid: GridSnapshot {
                        rows: 24,
                        cols: 80,
                        cells: vec![
                            kmux_protocol::messages::CellState {
                                c: 'A',
                                fg: CellColor::new(0xff, 0xff, 0xff),
                                bg: CellColor::new(0x00, 0x00, 0x00),
                                ..Default::default()
                            };
                            24 * 80
                        ],
                        cursor: CursorState::default(),
                        modes: TermModes::EMPTY,
                    },
                    scrollback_lines: vec![vec![kmux_protocol::messages::CellState::default(); 80]],
                    cwd: "/home/user/project".to_string(),
                }],
            }],
        }
    }

    #[test]
    fn roundtrip_serialization() {
        let original = sample_state();
        let encoded = postcard::to_allocvec(&original).expect("serialization failed");
        let decoded: PersistedDaemonState =
            postcard::from_bytes(&encoded).expect("deserialization failed");

        assert_eq!(decoded.version, original.version);
        assert_eq!(
            decoded.session_index_counter,
            original.session_index_counter
        );
        assert_eq!(decoded.used_words, original.used_words);
        assert_eq!(decoded.sessions.len(), 1);

        let session = &decoded.sessions[0];
        assert_eq!(session.meta.word_id, "eagle");
        assert_eq!(session.meta.name, "my-session");
        assert_eq!(session.next_pane_index, 2);
        assert_eq!(session.panes.len(), 1);

        let pane = &session.panes[0];
        assert_eq!(pane.pane_index, 0);
        assert_eq!(pane.program, "/bin/bash");
        assert_eq!(pane.size.rows, 24);
        assert_eq!(pane.size.cols, 80);
        assert_eq!(pane.child_pid, Some(12345));
        assert!(matches!(pane.status, SessionStatus::Running));
        assert_eq!(pane.grid.rows, 24);
        assert_eq!(pane.grid.cols, 80);
        assert_eq!(pane.grid.cells.len(), 24 * 80);
        assert_eq!(pane.scrollback_lines.len(), 1);
        assert_eq!(pane.cwd, "/home/user/project");
    }

    #[test]
    fn version_field_preserved() {
        let state = sample_state();
        let encoded = postcard::to_allocvec(&state).unwrap();
        let decoded: PersistedDaemonState = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded.version, STATE_VERSION);
    }

    #[test]
    fn exited_pane_serializes() {
        let mut state = sample_state();
        state.sessions[0].panes[0].status = SessionStatus::Exited {
            code: Some(1),
            signal: None,
        };
        state.sessions[0].panes[0].child_pid = None;

        let encoded = postcard::to_allocvec(&state).unwrap();
        let decoded: PersistedDaemonState = postcard::from_bytes(&encoded).unwrap();
        assert!(
            matches!(
                decoded.sessions[0].panes[0].status,
                SessionStatus::Exited { code: Some(1), .. }
            ),
            "exited status not preserved"
        );
        assert_eq!(decoded.sessions[0].panes[0].child_pid, None);
    }
}
