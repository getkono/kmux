use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{ClientCapabilities, InputMode, LayoutNode, SessionStatus};
use kmux_pty::config::{EnvBuilder, PtyConfig};
use tracing::warn;

use crate::backend::{BackendConfig, BackendSize, CapabilityHandles, DEFAULT_SCROLLBACK};
use crate::capability::pane_spawn_env;
use crate::relay::session_diff_loop;
use crate::scrollback::DiffBuffer;
use crate::term_state::new_term_state;

use super::ansi_emit::{seed_pane_with_preamble, snapshot_to_ansi};
use super::helpers::resolve_cwd;
use super::persistence::RestoreReport;
use super::{ClientMap, PaneEventSink, PaneRelay, SCROLLBACK_CAPACITY, ServerApp, SessionState};

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
                // Apply the same canonical env (`TERM`, `COLORTERM`,
                // `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`) that `spawn_pane_relay`
                // uses for fresh panes — without this, restored shells would
                // inherit whatever the daemon process happened to be launched
                // with. The seed caps are the default because `pane_spawn_env`
                // ignores them today; restored panes have no live client.
                let config = PtyConfig::new(&persisted_pane.program)
                    .args(persisted_pane.args.clone())
                    .size(size.rows, size.cols)
                    .cwd(&effective_cwd)
                    .env(
                        EnvBuilder::new()
                            .auto_term(false)
                            .extend(pane_spawn_env(&ClientCapabilities::default())),
                    );

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
                let title = Arc::new(Mutex::new(String::new()));
                let title_sink = Arc::new(PaneEventSink::new(
                    pane_id.clone(),
                    title.clone(),
                    self.vt_events_tx.clone(),
                ));
                let relay_sink =
                    Arc::clone(&title_sink) as Arc<dyn crate::backend::BackendEventSink>;
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
                    relay_sink,
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
                        title,
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

            // Reconcile the persisted tabs against the panes that actually
            // restored: drop dead leaves (collapsing splits) and tabs whose
            // panes all failed, and refocus if the focused pane is gone.
            let mut tabs: Vec<super::TabState> = Vec::new();
            for pt in &persisted_session.tabs {
                let mut layout = pt.layout.clone();
                let missing: Vec<u32> = layout
                    .leaves()
                    .into_iter()
                    .filter(|i| !panes_map.contains_key(i))
                    .collect();
                for m in missing {
                    super::layout::remove_pane(&mut layout, m);
                }
                let live: Vec<u32> = layout
                    .leaves()
                    .into_iter()
                    .filter(|i| panes_map.contains_key(i))
                    .collect();
                if live.is_empty() {
                    continue;
                }
                let focused = if panes_map.contains_key(&pt.focused_pane) {
                    pt.focused_pane
                } else {
                    live[0]
                };
                tabs.push(super::TabState {
                    tab_index: pt.tab_index,
                    name: pt.name.clone(),
                    layout,
                    focused_pane: focused,
                });
            }
            // Fallback for a checkpoint with no usable tabs: wrap each restored
            // pane in its own single-pane tab.
            if tabs.is_empty() {
                let mut indices: Vec<u32> = panes_map.keys().copied().collect();
                indices.sort_unstable();
                for (i, idx) in indices.into_iter().enumerate() {
                    tabs.push(super::TabState {
                        tab_index: i as u32,
                        name: format!("{}", i + 1),
                        layout: LayoutNode::single(idx),
                        focused_pane: idx,
                    });
                }
            }
            let next_tab_index = persisted_session
                .next_tab_index
                .max(tabs.iter().map(|t| t.tab_index + 1).max().unwrap_or(0));
            let active_tab = if tabs
                .iter()
                .any(|t| t.tab_index == persisted_session.active_tab)
            {
                persisted_session.active_tab
            } else {
                tabs[0].tab_index
            };

            let session_state = SessionState {
                meta: persisted_session.meta.clone(),
                panes: panes_map,
                next_pane_index: persisted_session.next_pane_index,
                tabs,
                next_tab_index,
                active_tab,
            };

            self.sessions
                .write()
                .await
                .insert(word_id.clone(), session_state);
        }

        report
    }
}
