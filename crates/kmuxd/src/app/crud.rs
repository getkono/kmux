use std::path::Path;

use kmux_protocol::messages::{
    ClientCapabilities, PaneInfo, SessionEntry, SessionMeta, SessionStatus, TermSize,
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

        let pane_info = PaneInfo {
            pane_id: pane_id.clone(),
            pane_index,
            program: relay.program.clone(),
            size: relay.size,
            attached_clients: vec![],
            status: SessionStatus::Running,
            title: relay.title.lock().unwrap().clone(),
        };

        let mut panes = HashMap::new();
        panes.insert(pane_index, relay);

        let state = SessionState {
            meta: meta.clone(),
            panes,
            next_pane_index: 1,
        };

        self.sessions.write().await.insert(word_id.clone(), state);

        Ok(SessionEntry {
            meta,
            panes: vec![pane_info],
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
                        PaneInfo {
                            pane_id: format!("{}/{pane_index}", state.meta.word_id),
                            pane_index,
                            program: relay.program.clone(),
                            size: relay.size,
                            attached_clients,
                            status: relay.status.clone(),
                            title: relay.title.lock().unwrap().clone(),
                        }
                    })
                    .collect();
                panes.sort_by_key(|p| p.pane_index);
                SessionEntry {
                    meta: state.meta.clone(),
                    panes,
                }
            })
            .collect();
        entries.sort_by_key(|e| e.meta.index);
        entries
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
