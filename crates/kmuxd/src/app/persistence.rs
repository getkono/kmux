use std::sync::atomic::Ordering;

use kmux_protocol::format_pane_id;
use kmux_protocol::messages::CellState;

use super::{ServerApp, SessionState};

/// Summary of a session restore operation.
#[derive(Debug, Default)]
pub struct RestoreReport {
    /// Total sessions attempted.
    pub restored: usize,
    /// Sessions whose child process was still alive and reattached.
    pub alive: usize,
    /// Sessions whose child process had exited (scrollback preserved).
    pub dead: usize,
}

impl ServerApp {
    /// Capture the current daemon state for checkpointing.
    ///
    /// Called by the persist layer to snapshot all sessions/panes without
    /// requiring it to access private fields directly.
    pub async fn checkpoint_state(&self) -> crate::persist::PersistedDaemonState {
        use crate::persist::{PersistedDaemonState, STATE_VERSION};

        let sessions_guard = self.sessions.read().await;
        let session_index_counter = self.session_index_counter.load(Ordering::Relaxed);
        let used_words: Vec<String> = sessions_guard.keys().cloned().collect();
        let mut persisted_sessions = Vec::with_capacity(sessions_guard.len());

        for session_state in sessions_guard.values() {
            persisted_sessions.push(self.snapshot_session(session_state).await);
        }

        persisted_sessions.sort_by_key(|s| s.meta.index);

        PersistedDaemonState {
            version: STATE_VERSION,
            session_index_counter,
            sessions: persisted_sessions,
            used_words,
        }
    }

    /// Snapshot a single live session into a [`PersistedSession`] (issue #64).
    ///
    /// Captures every pane's grid + scrollback (capped at [`MAX_SCROLLBACK_LINES`])
    /// and the session's tabs. Shared by [`checkpoint_state`](Self::checkpoint_state)
    /// and the close-session path, which retains the snapshot in the graveyard.
    pub(super) async fn snapshot_session(
        &self,
        session_state: &SessionState,
    ) -> crate::persist::PersistedSession {
        use crate::persist::{MAX_SCROLLBACK_LINES, PersistedPane, PersistedSession, PersistedTab};
        use std::sync::atomic::Ordering;

        let word_id = &session_state.meta.word_id;
        let mut persisted_panes = Vec::with_capacity(session_state.panes.len());

        for (&pane_index, relay) in session_state.panes.iter() {
            let pane_id = format_pane_id(word_id, pane_index);

            // Snapshot grid state and extract scrollback for the checkpoint.
            // The checkpoint schema stores owned `Vec<CellState>` lines (frozen
            // on-disk format); the engine now hands back shared `Arc` lines, so
            // materialise them here on this cold path.
            let (grid, scrollback_arc) = relay.engine.checkpoint_grid(MAX_SCROLLBACK_LINES);
            let scrollback_lines: Vec<Vec<CellState>> =
                scrollback_arc.iter().map(|line| line.to_vec()).collect();

            // Get child PID from the PTY registry.
            let child_pid = self.manager.child_pid(&pane_id).await.map(|p| p.as_raw());

            persisted_panes.push(PersistedPane {
                pane_index,
                program: relay.program.clone(),
                args: relay.args.clone(),
                size: relay.size.into(),
                status: relay.status.clone(),
                child_pid,
                grid,
                scrollback_lines,
                cwd: session_state.meta.cwd.clone(),
            });
        }

        persisted_panes.sort_by_key(|p| p.pane_index);

        let persisted_tabs: Vec<PersistedTab> = session_state
            .tabs
            .iter()
            .map(|t| PersistedTab {
                tab_index: t.tab_index,
                name: t.name.clone(),
                layout: t.layout.clone(),
                focused_pane: t.focused_pane,
            })
            .collect();

        PersistedSession {
            meta: session_state.meta.clone(),
            next_pane_index: session_state.next_pane_index,
            panes: persisted_panes,
            tabs: persisted_tabs,
            next_tab_index: session_state.next_tab_index,
            active_tab: session_state.active_tab,
            last_active_ms: session_state.last_active.load(Ordering::Relaxed),
        }
    }
}
