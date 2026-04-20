use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{ClientCapabilities, InputMode, PaneId, SessionStatus, TermSize};
use kmux_pty::config::{EnvBuilder, PtyConfig};
use kmux_pty::error::Result;

use crate::backend::{BackendConfig, BackendSize, CapabilityHandles, DEFAULT_SCROLLBACK};
use crate::capability::{intersect_for_atomics, pane_spawn_env};
use crate::relay::session_diff_loop;
use crate::scrollback::DiffBuffer;
use crate::term_state::new_term_state;

use super::helpers::resolve_cwd;
use super::{ClientMap, PaneRelay, PaneTitleSink, SCROLLBACK_CAPACITY, ServerApp};

impl ServerApp {
    /// Add a new pane to an existing session.
    pub async fn create_pane(
        &self,
        word_id: &str,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
        seed_caps: &ClientCapabilities,
    ) -> Result<PaneId> {
        use kmux_pty::error::KmuxError;
        use std::path::PathBuf;

        // Grab pane index and CWD with a short write lock, then drop it before IO.
        let (pane_index, pane_id, effective_cwd) = {
            let mut sessions = self.sessions.write().await;
            let state = sessions
                .get_mut(word_id)
                .ok_or_else(|| KmuxError::SessionNotFound {
                    name: word_id.to_string(),
                })?;
            let pane_index = state.next_pane_index;
            state.next_pane_index += 1;
            let pane_id = format!("{word_id}/{pane_index}");
            let effective_cwd = resolve_cwd(&PathBuf::from(&state.meta.cwd));
            (pane_index, pane_id, effective_cwd)
        };

        let relay = self
            .spawn_pane_relay(
                &pane_id,
                program,
                args,
                size,
                Some(&effective_cwd),
                seed_caps,
            )
            .await?;

        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;
        state.panes.insert(pane_index, relay);

        Ok(pane_id)
    }

    /// Gracefully close a single pane.
    /// If it was the last pane in its session, also removes the session.
    pub async fn close_pane(&self, pane_id: &str) -> Result<Option<i32>> {
        use kmux_pty::error::KmuxError;

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

        self.manager.close_nowait(pane_id).await?;

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

        Ok(None)
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
        let title = Arc::new(Mutex::new(String::new()));
        let title_sink = Arc::new(PaneTitleSink::new(
            pane_id.to_string(),
            title.clone(),
            self.vt_events_tx.clone(),
        ));
        let relay_sink = Arc::clone(&title_sink) as Arc<dyn crate::backend::BackendEventSink>;
        let term_state = Arc::new(Mutex::new(new_term_state(BackendConfig {
            size: BackendSize::from(size),
            capabilities: CapabilityHandles {
                kitty_graphics: kitty_graphics_enabled.clone(),
                kitty_keyboard: kitty_keyboard_enabled.clone(),
            },
            events: title_sink,
            scrollback: DEFAULT_SCROLLBACK,
        })));
        let seqno_counter = Arc::new(AtomicU64::new(1));

        let task = tokio::spawn(session_diff_loop(
            reader,
            pane_id.to_string(),
            relay_sink,
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
            title,
        })
    }
}
