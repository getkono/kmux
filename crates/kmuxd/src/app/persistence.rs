use std::sync::atomic::Ordering;

use super::ServerApp;

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
        use crate::persist::{
            MAX_SCROLLBACK_LINES, PersistedDaemonState, PersistedPane, PersistedSession,
            STATE_VERSION,
        };

        let sessions_guard = self.sessions.read().await;
        let session_index_counter = self.session_index_counter.load(Ordering::Relaxed);
        let used_words: Vec<String> = sessions_guard.keys().cloned().collect();
        let mut persisted_sessions = Vec::with_capacity(sessions_guard.len());

        for (word_id, session_state) in sessions_guard.iter() {
            let mut persisted_panes = Vec::with_capacity(session_state.panes.len());

            for (&pane_index, relay) in session_state.panes.iter() {
                let pane_id = format!("{word_id}/{pane_index}");

                // Snapshot grid state and extract scrollback in one lock scope.
                let (grid, scrollback_lines) = {
                    let ts = relay.term_state.lock().unwrap();
                    let grid = ts.snapshot();
                    let size = ts.history_size();
                    let start = size.saturating_sub(MAX_SCROLLBACK_LINES);
                    let count = size - start;
                    let scrollback_lines = if count > 0 {
                        ts.read_history_lines(start, count)
                    } else {
                        vec![]
                    };
                    (grid, scrollback_lines)
                };

                // Get child PID from the PTY registry.
                let child_pid = self.manager.child_pid(&pane_id).await.map(|p| p.as_raw());

                persisted_panes.push(PersistedPane {
                    pane_index,
                    program: relay.program.clone(),
                    args: relay.args.clone(),
                    size: relay.size,
                    status: relay.status.clone(),
                    child_pid,
                    grid,
                    scrollback_lines,
                    cwd: session_state.meta.cwd.clone(),
                });
            }

            persisted_panes.sort_by_key(|p| p.pane_index);

            persisted_sessions.push(PersistedSession {
                meta: session_state.meta.clone(),
                next_pane_index: session_state.next_pane_index,
                panes: persisted_panes,
            });
        }

        persisted_sessions.sort_by_key(|s| s.meta.index);

        PersistedDaemonState {
            version: STATE_VERSION,
            session_index_counter,
            sessions: persisted_sessions,
            used_words,
        }
    }
}
