use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{
    ClientCapabilities, InputMode, PaneId, PaneInfo, SessionEntry, SessionMeta, SessionStatus,
    TermSize,
};
use kmux_pty::config::{EnvBuilder, PtyConfig};
use kmux_pty::error::{KmuxError, Result};

use crate::capability::{intersect_for_atomics, pane_spawn_env};
use crate::relay::session_diff_loop;
use crate::scrollback::DiffBuffer;
use crate::term_state::new_term_state;

use super::helpers::resolve_cwd;
use super::{ClientMap, PaneRelay, SCROLLBACK_CAPACITY, ServerApp, SessionState};

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

    /// Add a new pane to an existing session.
    pub async fn create_pane(
        &self,
        word_id: &str,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
        seed_caps: &ClientCapabilities,
    ) -> Result<PaneId> {
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;

        let pane_index = state.next_pane_index;
        state.next_pane_index += 1;
        let pane_id = format!("{word_id}/{pane_index}");

        let cwd = PathBuf::from(&state.meta.cwd);
        let effective_cwd = resolve_cwd(&cwd);

        // Drop the write lock before spawning (IO)
        let prog = match program {
            Some(ref p) => p.clone(),
            None => kmux_pty::shell::detect_shell()?,
        };
        let resolved_cwd_clone = effective_cwd.clone();
        drop(sessions);

        let (kg_init, kk_init) = intersect_for_atomics([seed_caps]);
        let kitty_graphics_enabled = Arc::new(AtomicBool::new(kg_init));
        let kitty_keyboard_enabled = Arc::new(AtomicBool::new(kk_init));

        let config = PtyConfig::new(&prog)
            .args(args.clone())
            .size(size.rows, size.cols)
            .cwd(resolved_cwd_clone)
            .env(
                EnvBuilder::new()
                    .auto_term(false)
                    .extend(pane_spawn_env(seed_caps)),
            );
        self.manager.spawn(&pane_id, &config).await?;

        let session = self.manager.get_session(&pane_id).await?;
        let (reader, writer) = session.split().await?;

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        let scrollback = Arc::new(Mutex::new(DiffBuffer::new(SCROLLBACK_CAPACITY)));
        let term_state = Arc::new(Mutex::new(new_term_state(
            size.rows,
            size.cols,
            kitty_graphics_enabled.clone(),
            kitty_keyboard_enabled.clone(),
        )));
        let seqno_counter = Arc::new(AtomicU64::new(1));

        let task = tokio::spawn(session_diff_loop(
            reader,
            pane_id.clone(),
            clients.clone(),
            scrollback.clone(),
            term_state.clone(),
            seqno_counter.clone(),
        ));

        let relay = PaneRelay {
            clients,
            writer,
            _task: task,
            program: prog,
            args: args.clone(),
            size,
            scrollback,
            term_state,
            seqno_counter,
            input_mode: InputMode::Open,
            status: SessionStatus::Running,
            kitty_graphics_enabled,
            kitty_keyboard_enabled,
        };

        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;
        state.panes.insert(pane_index, relay);

        Ok(pane_id)
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

        let mut last_exit = None;
        for (_, pane_id) in &pane_ids {
            if let Ok(code) = self.manager.close(pane_id).await {
                last_exit = code.code();
            }
        }

        // Remove session state
        let mut sessions = self.sessions.write().await;
        sessions.remove(word_id);

        // Return word to pool
        self.wordlist.lock().unwrap().release(word_id);

        Ok(last_exit)
    }

    /// Gracefully close a single pane.
    /// If it was the last pane in its session, also removes the session.
    pub async fn close_pane(&self, pane_id: &str) -> Result<Option<i32>> {
        use super::helpers::parse_pane_id;

        let (word_id, pane_index) =
            parse_pane_id(pane_id).ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;

        // Detach all clients from this pane
        {
            let sessions = self.sessions.read().await;
            if let Some(state) = sessions.get(word_id)
                && let Some(relay) = state.panes.get(&pane_index)
            {
                relay.clients.lock().unwrap().clear();
            }
        }

        let status = self.manager.close(pane_id).await?;
        let exit_code = status.code();

        // Remove pane from session; remove session if empty
        let mut sessions = self.sessions.write().await;
        if let Some(state) = sessions.get_mut(word_id) {
            state.panes.remove(&pane_index);
            if state.panes.is_empty() {
                let word = word_id.to_string();
                sessions.remove(&word);
                self.wordlist.lock().unwrap().release(&word);
            }
        }

        Ok(exit_code)
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

    /// Spawn a PTY process and create a `PaneRelay` for it.
    pub(super) async fn spawn_pane_relay(
        &self,
        pane_id: &str,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
        cwd: Option<&Path>,
        seed_caps: &ClientCapabilities,
    ) -> Result<PaneRelay> {
        let prog = match program {
            Some(p) => p,
            None => kmux_pty::shell::detect_shell()?,
        };

        let (kg_init, kk_init) = intersect_for_atomics([seed_caps]);
        let kitty_graphics_enabled = Arc::new(AtomicBool::new(kg_init));
        let kitty_keyboard_enabled = Arc::new(AtomicBool::new(kk_init));

        let mut config = PtyConfig::new(&prog)
            .args(args.clone())
            .size(size.rows, size.cols)
            .env(
                EnvBuilder::new()
                    .auto_term(false)
                    .extend(pane_spawn_env(seed_caps)),
            );
        if let Some(cwd_path) = cwd {
            config = config.cwd(cwd_path);
        }

        self.manager.spawn(pane_id, &config).await?;
        let session = self.manager.get_session(pane_id).await?;
        let (reader, writer) = session.split().await?;

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        let scrollback = Arc::new(Mutex::new(DiffBuffer::new(SCROLLBACK_CAPACITY)));
        let term_state = Arc::new(Mutex::new(new_term_state(
            size.rows,
            size.cols,
            kitty_graphics_enabled.clone(),
            kitty_keyboard_enabled.clone(),
        )));
        let seqno_counter = Arc::new(AtomicU64::new(1));

        let task = tokio::spawn(session_diff_loop(
            reader,
            pane_id.to_string(),
            clients.clone(),
            scrollback.clone(),
            term_state.clone(),
            seqno_counter.clone(),
        ));

        Ok(PaneRelay {
            clients,
            writer,
            _task: task,
            program: prog,
            args,
            size,
            scrollback,
            term_state,
            seqno_counter,
            input_mode: InputMode::Open,
            status: SessionStatus::Running,
            kitty_graphics_enabled,
            kitty_keyboard_enabled,
        })
    }
}
