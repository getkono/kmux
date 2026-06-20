use std::path::Path;
use std::sync::Arc;

use kmux_protocol::messages::{
    ClientCapabilities, LayoutNode, PaneId, PaneInfo, PaneProcesses, SessionEntry, SessionMeta,
    SessionStatus, TermSize,
};
use kmux_pty::error::{KmuxError, Result};

use super::helpers::resolve_cwd;
use super::{ServerApp, SessionState};

impl ServerApp {
    /// Create a new session with one initial pane. Returns the full `SessionEntry`.
    pub async fn create_session(
        &self,
        name: Option<String>,
        cwd: Option<String>,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
        seed_caps: &ClientCapabilities,
    ) -> Result<SessionEntry> {
        use std::collections::HashMap;
        use std::sync::atomic::Ordering;

        // Check the session limit
        {
            let sessions = self.sessions.read().await;
            if sessions.len() >= super::MAX_SESSIONS {
                return Err(KmuxError::SessionAlreadyExists {
                    name: format!("session limit ({}) reached", super::MAX_SESSIONS),
                });
            }
        }

        // Draw a unique word ID
        let word_id = {
            let mut wl = self.wordlist.lock().unwrap();
            let mut rng = self.rng.lock().unwrap();
            wl.draw(&mut *rng)
                .ok_or_else(|| KmuxError::SessionAlreadyExists {
                    name: "word pool exhausted".to_string(),
                })?
        };

        // Resolve CWD
        let resolved_cwd = resolve_cwd(
            cwd.as_deref()
                .map(Path::new)
                .unwrap_or_else(|| Path::new(".")),
        );

        // Default name = basename of cwd
        let display_name = name.unwrap_or_else(|| {
            resolved_cwd
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&word_id)
                .to_string()
        });

        let index = self.session_index_counter.fetch_add(1, Ordering::Relaxed);

        let meta = SessionMeta {
            index,
            word_id: word_id.clone(),
            name: display_name,
            cwd: resolved_cwd.to_string_lossy().into_owned(),
        };

        // Spawn initial pane (index 0)
        let pane_index = 0u32;
        let pane_id = format!("{word_id}/{pane_index}");
        let relay = self
            .spawn_pane_relay(
                &pane_id,
                program,
                args,
                size,
                Some(&resolved_cwd),
                seed_caps,
            )
            .await?;

        let progress = *relay.progress.lock().unwrap();
        let pane_info = PaneInfo {
            pane_id: pane_id.clone(),
            pane_index,
            program: relay.program.clone(),
            size: relay.size,
            attached_clients: vec![],
            status: SessionStatus::Running,
            title: relay.title.lock().unwrap().clone(),
            progress_state: progress.state,
            progress: progress.progress,
        };

        let mut panes = HashMap::new();
        panes.insert(pane_index, relay);

        // Seed a single default tab containing the initial pane.
        let tab = super::TabState {
            tab_index: 0,
            name: "1".to_string(),
            layout: LayoutNode::single(pane_index),
            focused_pane: pane_index,
        };
        let tab_info = tab.to_info();

        let state = SessionState {
            meta: meta.clone(),
            panes,
            next_pane_index: 1,
            tabs: vec![tab],
            next_tab_index: 1,
            active_tab: 0,
        };

        self.sessions.write().await.insert(word_id.clone(), state);

        Ok(SessionEntry {
            meta,
            panes: vec![pane_info],
            tabs: vec![tab_info],
            active_tab: 0,
            // Local session: federated attribution is added by the hub's
            // `localize_entry` only when proxying a remote peer.
            peer: None,
        })
    }

    /// Gracefully close all panes of a session and remove it.
    pub async fn close_session(&self, word_id: &str) -> Result<Option<i32>> {
        let pane_ids: Vec<(u32, String)> = {
            let sessions = self.sessions.read().await;
            sessions
                .get(word_id)
                .map(|s| {
                    s.panes
                        .keys()
                        .map(|&idx| (idx, format!("{word_id}/{idx}")))
                        .collect()
                })
                .unwrap_or_default()
        };

        for (_, pane_id) in &pane_ids {
            let _ = self.manager.close_nowait(pane_id).await;
        }

        // Remove session state
        let mut sessions = self.sessions.write().await;
        sessions.remove(word_id);

        // Return word to pool
        self.wordlist.lock().unwrap().release(word_id);

        Ok(None)
    }

    /// List all active sessions with their pane metadata.
    pub async fn list_sessions(&self) -> Vec<SessionEntry> {
        let sessions = self.sessions.read().await;
        let mut entries: Vec<SessionEntry> = sessions
            .values()
            .map(|state| {
                let mut panes: Vec<PaneInfo> = state
                    .panes
                    .iter()
                    .map(|(&pane_index, relay)| {
                        let attached_clients =
                            relay.clients.lock().unwrap().keys().copied().collect();
                        let progress = *relay.progress.lock().unwrap();
                        PaneInfo {
                            pane_id: format!("{}/{pane_index}", state.meta.word_id),
                            pane_index,
                            program: relay.program.clone(),
                            size: relay.size,
                            attached_clients,
                            status: relay.status.clone(),
                            title: relay.title.lock().unwrap().clone(),
                            progress_state: progress.state,
                            progress: progress.progress,
                        }
                    })
                    .collect();
                panes.sort_by_key(|p| p.pane_index);
                SessionEntry {
                    meta: state.meta.clone(),
                    panes,
                    tabs: state.tab_infos(),
                    active_tab: state.active_tab,
                    peer: None,
                }
            })
            .collect();
        entries.sort_by_key(|e| e.meta.index);
        entries
    }

    /// Sample the process tree of every locally-hosted pane (issue #122).
    ///
    /// Gathers each pane's PTY child pid, then runs the (blocking) `sysinfo`
    /// scan on a blocking thread so the async runtime is never stalled. The
    /// sampler refreshes lazily, so this is cheap to call repeatedly while the
    /// overview is open and free when it is not. Federated panes are handled
    /// separately by the peer subsystem and merged by the dispatch layer.
    pub async fn local_process_overview(&self) -> Vec<PaneProcesses> {
        let roots = self.collect_pane_roots().await;
        let sampler = Arc::clone(&self.sampler);
        tokio::task::spawn_blocking(move || {
            let now = std::time::Instant::now();
            sampler.lock().unwrap().sample(now, &roots)
        })
        .await
        .unwrap_or_default()
    }

    /// The `(pane_id, child_pid)` of every locally-hosted pane. The session read
    /// lock is dropped before querying pids so it is never held across an await.
    async fn collect_pane_roots(&self) -> Vec<(PaneId, Option<i32>)> {
        let pane_ids: Vec<PaneId> = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .flat_map(|state| {
                    let word = state.meta.word_id.clone();
                    state
                        .panes
                        .keys()
                        .map(move |idx| format!("{word}/{idx}"))
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        let mut roots = Vec::with_capacity(pane_ids.len());
        for pane_id in pane_ids {
            let pid = self.manager.child_pid(&pane_id).await.map(|p| p.as_raw());
            roots.push((pane_id, pid));
        }
        roots
    }

    /// Rename a session's display name.
    pub async fn rename_session(&self, word_id: &str, new_name: &str) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;
        state.meta.name = new_name.to_string();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::TermSize;

    /// End-to-end (issue #122): a session running a real child appears in the
    /// local process overview, rooted at that pane's PTY child pid, with the
    /// child present in its process tree.
    #[tokio::test]
    async fn local_process_overview_reports_pane_child() {
        let size = TermSize::default();
        let caps = ClientCapabilities::default();
        let app = ServerApp::new("tok".to_string());

        // A long-lived, childless process makes the assertion deterministic.
        let entry = app
            .create_session(
                None,
                Some("/tmp".to_string()),
                Some("/bin/sleep".to_string()),
                vec!["30".to_string()],
                size,
                &caps,
            )
            .await
            .expect("create_session");
        let word = entry.meta.word_id.clone();
        let pane_id = format!("{word}/0");
        let child_pid = app
            .manager
            .child_pid(&pane_id)
            .await
            .expect("pane child pid")
            .as_raw();

        let overview = app.local_process_overview().await;
        let pane = overview
            .iter()
            .find(|p| p.pane_id == pane_id)
            .expect("pane present in overview");
        assert_eq!(pane.root_pid, Some(child_pid));
        assert!(
            pane.processes.iter().any(|p| p.pid == child_pid),
            "pane's child pid {child_pid} should be in its process tree: {:?}",
            pane.processes
        );

        // Clean up the spawned child.
        let _ = app.close_session(&word).await;
    }
}
