//! Server-side support for graceful daemon handoff (issue #35): building the
//! pane manifest advertised to a successor daemon, holding the relay loops for
//! the final checkpoint (and releasing them on a rollback), and quiescing them
//! once the handoff has committed.
//!
//! The transport and orchestration live in [`crate::handoff`]; see
//! `docs/daemon-handoff.md` for the full sequence.

use std::time::Duration;

use kmux_protocol::control_rpc::HandoffPaneMeta;
use kmux_protocol::format_pane_id;

use super::ServerApp;

impl ServerApp {
    /// Build the per-pane manifest advertised to a successor daemon.
    ///
    /// Each entry records the child PID and whether it is still live, so the
    /// successor can choose per pane between live migration (a master fd will be
    /// streamed) and snapshot respawn (no fd).
    pub async fn collect_handoff_panes(&self) -> Vec<HandoffPaneMeta> {
        // Snapshot pane ids first so we don't hold the sessions lock across the
        // per-pane manager queries below.
        let pane_ids: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions
                .iter()
                .flat_map(|(word_id, state)| {
                    state
                        .panes
                        .keys()
                        .map(move |idx| format_pane_id(word_id, *idx))
                })
                .collect()
        };

        let mut out = Vec::with_capacity(pane_ids.len());
        for pane_id in pane_ids {
            let pid = self.manager.child_pid(&pane_id).await;
            let alive = matches!(self.manager.is_exited(&pane_id).await, Some(false));
            out.push(HandoffPaneMeta {
                pid: pid.map_or(0, nix::unistd::Pid::as_raw),
                has_live_fd: pid.is_some() && alive,
                pane_id,
            });
        }
        out
    }

    /// Park every in-process pane's PTY reader between two reads, for a
    /// handoff's final checkpoint (issue #207): once this returns `true`, the
    /// checkpoint holds exactly the output this daemon consumed. Unlike
    /// [`Self::quiesce_relays`] it can be undone, by
    /// [`Self::release_relays`], so a handoff that fails afterwards rolls back
    /// to panes that read again.
    ///
    /// Returns `false`, with every reader released again, when one has not
    /// parked within `timeout`. Worker panes are not held: their worker reads
    /// the PTY itself.
    pub async fn hold_relays(&self, timeout: Duration) -> bool {
        let holds: Vec<_> = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .flat_map(|state| state.panes.values())
                .filter_map(|relay| relay.engine.hold_reader())
                .collect()
        };
        let deadline = tokio::time::Instant::now() + timeout;
        for hold in holds {
            if tokio::time::timeout_at(deadline, hold).await.is_err() {
                self.release_relays().await;
                return false;
            }
        }
        true
    }

    /// Let every reader parked by [`Self::hold_relays`] go on.
    pub async fn release_relays(&self) {
        let sessions = self.sessions.read().await;
        for relay in sessions.values().flat_map(|state| state.panes.values()) {
            relay.engine.release_reader();
        }
    }

    /// Abort every pane's relay read task and wait for them to stop.
    ///
    /// After this returns, the outgoing daemon reads no PTY masters, so the
    /// successor can become the sole reader of each inherited fd without a
    /// split-read race. Output produced in the gap stays buffered in the kernel
    /// PTY until the successor's relay drains it.
    pub async fn quiesce_relays(&self) {
        let mut handles = Vec::new();
        {
            let mut sessions = self.sessions.write().await;
            for state in sessions.values_mut() {
                for relay in state.panes.values_mut() {
                    // Abort the engine's relay task and take the real handle so we
                    // can await its cancellation below.
                    handles.push(relay.engine.abort_relay_task());
                }
            }
        }
        for handle in handles {
            let _ = handle.await; // JoinError::Cancelled is expected
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use kmux_protocol::messages::{ClientCapabilities, TermSize};

    use super::ServerApp;

    /// Whether the pane's emulator shows `text` anywhere on its grid.
    async fn grid_shows(app: &ServerApp, pane_id: &str, text: &str) -> bool {
        let snapshot = {
            let sessions = app.sessions.read().await;
            super::super::helpers::get_pane_relay(&sessions, pane_id)
                .expect("the pane")
                .engine
                .snapshot()
        };
        let chars: String = snapshot.cells.iter().map(|c| c.c).collect();
        chars.contains(text)
    }

    /// While the readers are held, a pane's output waits in the PTY instead
    /// of reaching its emulator; once released, it is read and shown. A real
    /// PTY, because what is held is the read of it (R7).
    #[tokio::test]
    async fn held_relays_read_nothing_until_released() {
        let app = crate::fixtures::fixture_app();
        let size = TermSize {
            rows: 4,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        };
        let entry = app
            .create_session(
                None,
                None,
                Some("/bin/cat".into()),
                vec![],
                size,
                &ClientCapabilities::default(),
            )
            .await
            .expect("a session");
        let pane_id = kmux_protocol::format_pane_id(&entry.meta.word_id, 0);

        assert!(app.hold_relays(Duration::from_secs(10)).await, "parked");
        app.write_input(
            &pane_id,
            kmux_protocol::messages::ClientId(1),
            b"held\n".to_vec(),
        )
        .await
        .expect("queued");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !grid_shows(&app, &pane_id, "held").await,
            "nothing read while held"
        );

        app.release_relays().await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !grid_shows(&app, &pane_id, "held").await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("read once released");
    }

    /// End-to-end (in-process) live migration: a session's PTY child is handed
    /// off to a successor `ServerApp` by transferring its master fd, and the
    /// **same child PID** keeps running on the other side — proving the live
    /// process migrated rather than being respawned.
    #[tokio::test]
    async fn live_pty_migrates_with_same_pid() {
        let size = TermSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let caps = ClientCapabilities::default();

        // Predecessor: a session running `cat` (a long-lived child).
        let old = ServerApp::new("tok-old".to_string());
        let entry = old
            .create_session(
                None,
                Some("/tmp".to_string()),
                Some("/bin/cat".to_string()),
                vec![],
                size,
                &caps,
            )
            .await
            .expect("create_session");
        let pane_id = kmux_protocol::format_pane_id(&entry.meta.word_id, 0);
        let pid_before = old
            .manager
            .child_pid(&pane_id)
            .await
            .expect("child pid before");

        // Simulate the sender side of a handoff.
        let manifest = old.collect_handoff_panes().await;
        assert!(
            manifest
                .iter()
                .any(|p| p.pane_id == pane_id && p.has_live_fd),
            "live pane should advertise a transferable fd"
        );
        let fd = old.manager.dup_master_fd(&pane_id).await.expect("dup fd");
        old.manager.set_all_keep_alive(true).await;
        old.quiesce_relays().await;
        let state = old.checkpoint_state().await;

        // Successor: adopt the inherited fd.
        let new = ServerApp::new("tok-new".to_string());
        let mut inherited = HashMap::new();
        inherited.insert(pane_id.clone(), (fd, pid_before));
        let report = new.restore_with_handoff(state, inherited).await;
        assert_eq!(report.alive, 1, "one pane should have migrated live");

        let pid_after = new
            .manager
            .child_pid(&pane_id)
            .await
            .expect("child pid after");
        assert_eq!(
            pid_before, pid_after,
            "the SAME child process must survive the handoff"
        );
        assert!(
            nix::sys::signal::kill(pid_before, None).is_ok(),
            "migrated child must still be alive"
        );

        // Cleanup: SIGKILL the shared child.
        let _ = nix::sys::signal::kill(pid_before, nix::sys::signal::Signal::SIGKILL);
    }
}
