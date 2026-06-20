use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{ClientCapabilities, InputMode, LayoutNode, SessionStatus};
use kmux_pty::config::{EnvBuilder, PtyConfig};
use kmux_pty::{PtyProcess, PtyReader, PtySession, PtyWriter};
use nix::unistd::Pid;
use tracing::warn;

use crate::backend::{BackendConfig, BackendSize, CapabilityHandles, DEFAULT_SCROLLBACK};
use crate::capability::pane_spawn_env;
use crate::conversions::term_size_to_window;
use crate::persist::PersistedPane;
use crate::relay::session_diff_loop;
use crate::scrollback::DiffBuffer;
use crate::term_state::new_term_state;

use super::ansi_emit::{seed_pane_with_preamble, snapshot_to_ansi};
use super::helpers::resolve_cwd;
use super::persistence::RestoreReport;
use super::{
    ClientMap, PaneEventSink, PaneProgress, PaneRelay, SCROLLBACK_CAPACITY, ServerApp, SessionState,
};

/// How a restored pane's emulator should be seeded from its checkpoint snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SeedMode {
    /// A fresh shell was respawned: replay the old grid + scrollback as ANSI
    /// with a dim "[kmux: session restored]" separator above the new prompt.
    Respawned,
    /// A live PTY was inherited across a handoff: seed the grid + scrollback so
    /// the last-known screen shows immediately, but with **no** separator — the
    /// live stream simply continues. Seamless, not a respawn.
    Inherited,
}

impl ServerApp {
    /// Restore sessions from a [`PersistedDaemonState`], spawning a fresh shell
    /// for every pane (replaying the old grid/scrollback as ANSI history).
    ///
    /// This is the cold-start path and the fallback when a graceful handoff is
    /// unavailable. See [`restore_with_handoff`](Self::restore_with_handoff) for
    /// the live-migration variant.
    pub async fn restore_from(&self, state: crate::persist::PersistedDaemonState) -> RestoreReport {
        self.restore_inner(state, HashMap::new()).await
    }

    /// Restore sessions, adopting any live PTY master fds handed off from a
    /// previous daemon. `inherited` maps `pane_id` → (master fd, child pid);
    /// panes present in the map keep their **live** process, others respawn from
    /// the snapshot exactly as [`restore_from`](Self::restore_from) would.
    pub async fn restore_with_handoff(
        &self,
        state: crate::persist::PersistedDaemonState,
        inherited: HashMap<String, (OwnedFd, Pid)>,
    ) -> RestoreReport {
        self.restore_inner(state, inherited).await
    }

    async fn restore_inner(
        &self,
        state: crate::persist::PersistedDaemonState,
        mut inherited: HashMap<String, (OwnedFd, Pid)>,
    ) -> RestoreReport {
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
                let inherited_fd = inherited.remove(&pane_id);

                match self
                    .restore_one_pane(&pane_id, &persisted_pane, inherited_fd, &effective_cwd)
                    .await
                {
                    Some((relay, was_live)) => {
                        panes_map.insert(pane_index, relay);
                        report.restored += 1;
                        if was_live {
                            report.alive += 1;
                        } else {
                            report.dead += 1;
                        }
                    }
                    None => continue,
                }
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

    /// Restore a single pane: adopt its inherited live fd if one was handed off,
    /// otherwise spawn a fresh shell. Returns the relay and whether the original
    /// live process was migrated (`true`) or the pane was respawned (`false`).
    async fn restore_one_pane(
        &self,
        pane_id: &str,
        persisted_pane: &PersistedPane,
        inherited_fd: Option<(OwnedFd, Pid)>,
        effective_cwd: &Path,
    ) -> Option<(PaneRelay, bool)> {
        let size = persisted_pane.size.to_term_size();

        // Live-migration path: adopt the inherited PTY master fd.
        if let Some((fd, pid)) = inherited_fd {
            match PtyProcess::from_inherited(fd, pid, term_size_to_window(size))
                .map(PtySession::from_process)
            {
                Ok(session) => {
                    if let Err(e) = self.manager.register(pane_id, session).await {
                        warn!("restore: could not register inherited pane {pane_id}: {e}");
                    } else {
                        match self.split_registered(pane_id).await {
                            Some((reader, writer)) => {
                                let relay = self.build_pane_relay(
                                    pane_id,
                                    persisted_pane,
                                    reader,
                                    writer,
                                    SeedMode::Inherited,
                                );
                                return Some((relay, true));
                            }
                            None => {
                                // Registered but unsplittable — drop it and fall
                                // through to respawn so the pane is not lost.
                                let _ = self.manager.close(pane_id).await;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("restore: could not adopt inherited pane {pane_id}: {e}");
                    // The dup we received is dropped here; fall through to respawn.
                }
            }
        }

        // Respawn path: spawn a fresh shell with the persisted program/args/env.
        // Apply the same canonical env (`TERM`, `COLORTERM`, `TERM_PROGRAM`,
        // `TERM_PROGRAM_VERSION`) that `spawn_pane_relay` uses for fresh panes —
        // without this, restored shells would inherit whatever the daemon
        // process happened to be launched with. Seed caps default because
        // `pane_spawn_env` ignores them and restored panes have no live client.
        let config = PtyConfig::new(&persisted_pane.program)
            .args(persisted_pane.args.clone())
            .size(size.rows, size.cols)
            .cwd(effective_cwd)
            .env(
                EnvBuilder::new()
                    .auto_term(false)
                    .extend(pane_spawn_env(&ClientCapabilities::default())),
            );

        if let Err(e) = self.manager.spawn(pane_id, &config).await {
            warn!("restore: failed to spawn fresh shell for {pane_id}: {e}");
            return None;
        }
        let (reader, writer) = self.split_registered(pane_id).await?;
        let relay =
            self.build_pane_relay(pane_id, persisted_pane, reader, writer, SeedMode::Respawned);
        Some((relay, false))
    }

    /// Fetch a registered session and split it into reader/writer halves,
    /// logging and returning `None` on failure.
    async fn split_registered(&self, pane_id: &str) -> Option<(PtyReader, PtyWriter)> {
        let session = match self.manager.get_session(pane_id).await {
            Ok(s) => s,
            Err(e) => {
                warn!("restore: could not get session {pane_id}: {e}");
                return None;
            }
        };
        match session.split().await {
            Ok(rw) => Some(rw),
            Err(e) => {
                warn!("restore: could not split session {pane_id}: {e}");
                None
            }
        }
    }

    /// Assemble a [`PaneRelay`] around already-split reader/writer halves: build
    /// the client map, scrollback, VT emulator, seed it from the snapshot per
    /// `seed`, and spawn the relay loop. Shared by the respawn and inherited
    /// paths (and by [`spawn_pane_relay`](Self::spawn_pane_relay) callers via the
    /// same `PaneRelay` shape).
    pub(super) fn build_pane_relay(
        &self,
        pane_id: &str,
        persisted_pane: &PersistedPane,
        reader: PtyReader,
        writer: PtyWriter,
        seed: SeedMode,
    ) -> PaneRelay {
        let size = persisted_pane.size.to_term_size();

        let kitty_graphics_enabled = Arc::new(AtomicBool::new(false));
        let kitty_keyboard_enabled = Arc::new(AtomicBool::new(false));
        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        let scrollback = Arc::new(Mutex::new(DiffBuffer::new(SCROLLBACK_CAPACITY)));
        let title = Arc::new(Mutex::new(String::new()));
        let progress = Arc::new(Mutex::new(PaneProgress::default()));
        let title_sink = Arc::new(PaneEventSink::new(
            pane_id.to_string(),
            title.clone(),
            progress.clone(),
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

        // Pre-feed the old scrollback history + visible grid as ANSI bytes so the
        // client can scroll up through the full history. Done synchronously
        // before spawning the diff loop so `snapshot()` already has the restored
        // content when a client attaches immediately after restart/handoff.
        let separator = matches!(seed, SeedMode::Respawned);
        let preamble = snapshot_to_ansi(
            &persisted_pane.grid,
            &persisted_pane.scrollback_lines,
            separator,
        );
        seed_pane_with_preamble(&term_state, &scrollback, &seqno_counter, &preamble);

        let task = tokio::spawn(session_diff_loop(
            reader,
            pane_id.to_string(),
            relay_sink,
            clients.clone(),
            scrollback.clone(),
            term_state.clone(),
            seqno_counter.clone(),
            self.manager.clone(),
        ));

        PaneRelay {
            clients,
            engine: crate::engine::PaneEngine::InProcess(crate::engine::InProcessEngine::new(
                term_state, writer, task,
            )),
            program: persisted_pane.program.clone(),
            args: persisted_pane.args.clone(),
            size,
            scrollback,
            seqno_counter,
            input_mode: InputMode::Open,
            status: SessionStatus::Running,
            kitty_graphics_enabled,
            kitty_keyboard_enabled,
            title,
            progress,
        }
    }
}
