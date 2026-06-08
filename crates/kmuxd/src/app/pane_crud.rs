use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{ClientCapabilities, InputMode, SessionStatus, TermSize};
use kmux_pty::config::{EnvBuilder, PtyConfig};
use kmux_pty::error::Result;

use crate::backend::{BackendConfig, BackendSize, CapabilityHandles, DEFAULT_SCROLLBACK};
use crate::capability::{intersect_for_atomics, pane_spawn_env};
use crate::relay::session_diff_loop;
use crate::scrollback::DiffBuffer;
use crate::term_state::new_term_state;

use super::{ClientMap, PaneEventSink, PaneRelay, SCROLLBACK_CAPACITY, ServerApp};

impl ServerApp {
    /// Gracefully close a single pane, collapsing its tab's layout tree.
    ///
    /// Returns the child exit code (if known) and a [`PaneCloseOutcome`]
    /// describing how the tab/session changed, so the caller can broadcast the
    /// authoritative `LayoutUpdate` / tab-close event. If the pane was the last
    /// in its tab the tab is removed; if it was the last tab the session closes.
    pub async fn close_pane(
        &self,
        pane_id: &str,
    ) -> Result<(Option<i32>, super::PaneCloseOutcome)> {
        use kmux_pty::error::KmuxError;

        use super::PaneCloseOutcome;
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

        // Remove the pane and collapse its tab's layout; remove the tab if it
        // becomes empty, and the session if it has no tabs left.
        let mut sessions = self.sessions.write().await;
        let Some(state) = sessions.get_mut(word_id) else {
            return Ok((None, PaneCloseOutcome::Gone));
        };
        state.panes.remove(&pane_index);

        let Some(tab_index) = state.tab_of_pane(pane_index) else {
            // Pane wasn't in any tab's tree (shouldn't happen); fall back to the
            // legacy "drop empty session" behavior.
            if state.panes.is_empty() {
                let word = word_id.to_string();
                sessions.remove(&word);
                self.wordlist.lock().unwrap().release(&word);
                return Ok((None, PaneCloseOutcome::SessionClosed));
            }
            return Ok((None, PaneCloseOutcome::Gone));
        };

        let tab = state.tab_mut(tab_index).expect("tab_of_pane returned it");
        let leaves = tab.layout.leaves();
        let outcome = if leaves.len() <= 1 {
            // Last pane in this tab → remove the tab.
            state.tabs.retain(|t| t.tab_index != tab_index);
            if state.active_tab == tab_index {
                state.active_tab = state.tabs.first().map(|t| t.tab_index).unwrap_or(0);
            }
            if state.tabs.is_empty() {
                let word = word_id.to_string();
                sessions.remove(&word);
                self.wordlist.lock().unwrap().release(&word);
                PaneCloseOutcome::SessionClosed
            } else {
                PaneCloseOutcome::TabClosed { tab_index }
            }
        } else {
            // Collapse the leaf and refocus a sibling if it held focus.
            let refocus = super::layout::next_focus_after_removal(&tab.layout, pane_index);
            super::layout::remove_pane(&mut tab.layout, pane_index);
            if tab.focused_pane == pane_index {
                tab.focused_pane = refocus
                    .or_else(|| tab.layout.leaves().first().copied())
                    .unwrap_or(pane_index);
            }
            PaneCloseOutcome::TabUpdated {
                tab_index,
                layout: tab.layout.clone(),
                focused_pane: tab.focused_pane,
            }
        };

        Ok((None, outcome))
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
        let title_sink = Arc::new(PaneEventSink::new(
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
