use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{InputMode, SessionStatus};
use kmux_pty::config::{EnvBuilder, PtyConfig};
use tracing::warn;

use crate::backend::{
    BackendConfig, BackendSize, CapabilityHandles, DEFAULT_SCROLLBACK, NullEventSink,
};
use crate::relay::session_diff_loop;
use crate::scrollback::DiffBuffer;
use crate::term_state::new_term_state;

use super::ansi_emit::{seed_pane_with_preamble, snapshot_to_ansi};
use super::helpers::resolve_cwd;
use super::persistence::RestoreReport;
use super::{ClientMap, PaneRelay, SCROLLBACK_CAPACITY, ServerApp, SessionState};

impl ServerApp {
    /// Restore sessions from a [`PersistedDaemonState`].
    ///
    /// For each persisted pane, spawns a fresh shell using the same program and
    /// args as the original.  Before the new shell outputs its prompt, the old
    /// terminal grid is replayed as ANSI bytes so the client sees the previous
    /// visual state above a "session restored" separator line.
    ///
    /// Returns a [`RestoreReport`] suitable for logging.
    pub async fn restore_from(&self, state: crate::persist::PersistedDaemonState) -> RestoreReport {
        let mut report = RestoreReport::default();

        // Restore the session_index_counter to at least the checkpoint value.
        let _ = self
            .session_index_counter
            .fetch_max(state.session_index_counter, Ordering::Relaxed);

        // Reserve word IDs so the wordlist doesn't re-issue them.
        {
            let mut wl = self.wordlist.lock().unwrap();
            for word in &state.used_words {
                wl.reserve(word);
            }
        }

        for persisted_session in state.sessions {
            let word_id = persisted_session.meta.word_id.clone();
            let mut panes_map: HashMap<u32, PaneRelay> = HashMap::new();
            let session_cwd = PathBuf::from(&persisted_session.meta.cwd);
            let effective_cwd = resolve_cwd(&session_cwd);

            for persisted_pane in persisted_session.panes {
                let pane_index = persisted_pane.pane_index;
                let pane_id = format!("{word_id}/{pane_index}");
                let size = persisted_pane.size.to_term_size();

                // Spawn a fresh shell using the persisted program and args.
                let config = PtyConfig::new(&persisted_pane.program)
                    .args(persisted_pane.args.clone())
                    .size(size.rows, size.cols)
                    .cwd(&effective_cwd)
                    .env(EnvBuilder::new().auto_term(false));

                if let Err(e) = self.manager.spawn(&pane_id, &config).await {
                    warn!("restore: failed to spawn fresh shell for {pane_id}: {e}");
                    continue;
                }

                let session = match self.manager.get_session(&pane_id).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("restore: could not get session {pane_id}: {e}");
                        continue;
                    }
                };

                let (reader, writer) = match session.split().await {
                    Ok(rw) => rw,
                    Err(e) => {
                        warn!("restore: could not split session {pane_id}: {e}");
                        continue;
                    }
                };

                let kitty_graphics_enabled = Arc::new(AtomicBool::new(false));
                let kitty_keyboard_enabled = Arc::new(AtomicBool::new(false));
                let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
                let scrollback = Arc::new(Mutex::new(DiffBuffer::new(SCROLLBACK_CAPACITY)));
                let term_state = Arc::new(Mutex::new(new_term_state(BackendConfig {
                    size: BackendSize::from(size),
                    capabilities: CapabilityHandles {
                        kitty_graphics: kitty_graphics_enabled.clone(),
                        kitty_keyboard: kitty_keyboard_enabled.clone(),
                    },
                    events: Arc::new(NullEventSink),
                    scrollback: DEFAULT_SCROLLBACK,
                })));
                let seqno_counter = Arc::new(AtomicU64::new(1));

                // Pre-feed the old scrollback history + visible grid as ANSI
                // bytes so the client can scroll up through the full history.
                // Done synchronously before spawning the diff loop so that
                // `snapshot()` already has the restored content when a client
                // attaches immediately after daemon restart.
                let preamble =
                    snapshot_to_ansi(&persisted_pane.grid, &persisted_pane.scrollback_lines);
                seed_pane_with_preamble(&term_state, &scrollback, &seqno_counter, &preamble);

                let task = tokio::spawn(session_diff_loop(
                    reader,
                    pane_id.clone(),
                    clients.clone(),
                    scrollback.clone(),
                    term_state.clone(),
                    seqno_counter.clone(),
                ));

                panes_map.insert(
                    pane_index,
                    PaneRelay {
                        clients,
                        writer,
                        _task: task,
                        program: persisted_pane.program.clone(),
                        args: persisted_pane.args.clone(),
                        size,
                        scrollback,
                        term_state,
                        seqno_counter,
                        input_mode: InputMode::Open,
                        status: SessionStatus::Running,
                        kitty_graphics_enabled,
                        kitty_keyboard_enabled,
                    },
                );
                report.restored += 1;
            }

            if panes_map.is_empty() {
                // All panes failed to spawn — release the word ID.
                self.wordlist
                    .lock()
                    .unwrap()
                    .release(&persisted_session.meta.word_id);
                continue;
            }

            let session_state = SessionState {
                meta: persisted_session.meta.clone(),
                panes: panes_map,
                next_pane_index: persisted_session.next_pane_index,
            };

            self.sessions
                .write()
                .await
                .insert(word_id.clone(), session_state);
        }

        report
    }
}
