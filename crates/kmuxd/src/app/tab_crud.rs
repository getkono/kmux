//! Server-authoritative tab + layout operations.
//!
//! Each public method validates and mutates session state under the `sessions`
//! write lock, then returns the new authoritative tree (and focus) so the caller
//! ([`crate::client_handler`]) can reply to the requester and broadcast a
//! `LayoutUpdate` to every other client viewing the tab. PTY spawning happens
//! outside the lock (mirroring [`ServerApp::create_pane`]).

use std::path::PathBuf;

use kmux_protocol::messages::{
    ClientCapabilities, LayoutNode, LayoutScheme, PaneInfo, SessionStatus, SplitDir, TabInfo,
    TermSize,
};
use kmux_pty::error::{KmuxError, Result};

use super::helpers::resolve_cwd;
use super::{ServerApp, TabState, layout};

/// Result of removing a single pane from its tab.
#[derive(Debug, Clone)]
pub enum PaneCloseOutcome {
    /// The pane's tab is still alive with an updated layout.
    TabUpdated {
        tab_index: u32,
        layout: LayoutNode,
        focused_pane: u32,
    },
    /// The pane was the last one in its tab; the tab was removed.
    TabClosed { tab_index: u32 },
    /// The pane was the last one in the session's last tab; the session closed.
    SessionClosed,
    /// The pane or its session was already gone.
    Gone,
}

impl ServerApp {
    /// Create a new tab (with one fresh pane) inside an existing session.
    pub async fn create_tab(
        &self,
        word_id: &str,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
        seed_caps: &ClientCapabilities,
    ) -> Result<(TabInfo, PaneInfo)> {
        // Reserve a pane index, a tab index, and resolve the CWD under a short lock.
        let (pane_index, tab_index, pane_id, cwd) = {
            let mut sessions = self.sessions.write().await;
            let state = sessions
                .get_mut(word_id)
                .ok_or_else(|| KmuxError::SessionNotFound {
                    name: word_id.to_string(),
                })?;
            let pane_index = state.next_pane_index;
            state.next_pane_index += 1;
            let tab_index = state.next_tab_index;
            state.next_tab_index += 1;
            let pane_id = format!("{word_id}/{pane_index}");
            let cwd = resolve_cwd(&PathBuf::from(&state.meta.cwd));
            (pane_index, tab_index, pane_id, cwd)
        };

        let relay = self
            .spawn_pane_relay(&pane_id, program, args, size, Some(&cwd), seed_caps)
            .await?;
        let pane_info = PaneInfo {
            pane_id,
            pane_index,
            program: relay.program.clone(),
            size: relay.size,
            attached_clients: vec![],
            status: SessionStatus::Running,
            title: relay.title.lock().unwrap().clone(),
        };

        let tab_info = {
            let mut sessions = self.sessions.write().await;
            let state = sessions
                .get_mut(word_id)
                .ok_or_else(|| KmuxError::SessionNotFound {
                    name: word_id.to_string(),
                })?;
            state.panes.insert(pane_index, relay);
            let tab = TabState {
                tab_index,
                name: format!("{}", tab_index + 1),
                layout: LayoutNode::single(pane_index),
                focused_pane: pane_index,
            };
            let info = tab.to_info();
            state.tabs.push(tab);
            info
        };

        Ok((tab_info, pane_info))
    }

    /// Close an entire tab and every pane in it. If it was the session's last
    /// tab, the session is closed too. Returns `session_closed`.
    pub async fn close_tab(&self, word_id: &str, tab_index: u32) -> Result<bool> {
        // Collect the tab's pane ids (under a read lock), then kill the PTYs.
        let pane_ids: Vec<(u32, String)> = {
            let sessions = self.sessions.read().await;
            let Some(state) = sessions.get(word_id) else {
                return Ok(false);
            };
            let Some(tab) = state.tab(tab_index) else {
                return Ok(false);
            };
            tab.layout
                .leaves()
                .into_iter()
                .map(|idx| (idx, format!("{word_id}/{idx}")))
                .collect()
        };

        for (idx, pane_id) in &pane_ids {
            // Detach clients, then SIGTERM the child.
            {
                let sessions = self.sessions.read().await;
                if let Some(state) = sessions.get(word_id)
                    && let Some(relay) = state.panes.get(idx)
                {
                    relay.clients.lock().unwrap().clear();
                }
            }
            let _ = self.manager.close_nowait(pane_id).await;
        }

        let session_closed = {
            let mut sessions = self.sessions.write().await;
            let Some(state) = sessions.get_mut(word_id) else {
                return Ok(false);
            };
            for (idx, _) in &pane_ids {
                state.panes.remove(idx);
            }
            state.tabs.retain(|t| t.tab_index != tab_index);
            if state.active_tab == tab_index {
                state.active_tab = state.tabs.first().map(|t| t.tab_index).unwrap_or(0);
            }
            if state.tabs.is_empty() {
                let word = word_id.to_string();
                sessions.remove(&word);
                self.wordlist.lock().unwrap().release(&word);
                true
            } else {
                false
            }
        };

        Ok(session_closed)
    }

    /// Rename a tab's display name.
    pub async fn rename_tab(&self, word_id: &str, tab_index: u32, new_name: &str) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;
        let tab = state
            .tab_mut(tab_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: format!("{word_id} tab {tab_index}"),
            })?;
        tab.name = new_name.to_string();
        Ok(())
    }

    /// Split the focused pane in a tab, spawning a new pane adjacent to it.
    /// Returns the new pane, the tab's new layout tree, and the focused pane.
    // The split target (word/tab/from_pane/dir) plus the spawn parameters
    // (program/args/size/caps) are all genuinely distinct inputs; bundling them
    // would only obscure the call site in the dispatch router.
    #[allow(clippy::too_many_arguments)]
    pub async fn split_pane(
        &self,
        word_id: &str,
        tab_index: u32,
        from_pane: u32,
        dir: SplitDir,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
        seed_caps: &ClientCapabilities,
    ) -> Result<(PaneInfo, LayoutNode, u32)> {
        // Validate the tab/pane and reserve a pane index + CWD under the lock.
        let (new_index, pane_id, cwd) = {
            let mut sessions = self.sessions.write().await;
            let state = sessions
                .get_mut(word_id)
                .ok_or_else(|| KmuxError::SessionNotFound {
                    name: word_id.to_string(),
                })?;
            let tab = state
                .tab(tab_index)
                .ok_or_else(|| KmuxError::SessionNotFound {
                    name: format!("{word_id} tab {tab_index}"),
                })?;
            if !tab.layout.leaves().contains(&from_pane) {
                return Err(KmuxError::SessionNotFound {
                    name: format!("{word_id}/{from_pane}"),
                });
            }
            let new_index = state.next_pane_index;
            state.next_pane_index += 1;
            let pane_id = format!("{word_id}/{new_index}");
            let cwd = resolve_cwd(&PathBuf::from(&state.meta.cwd));
            (new_index, pane_id, cwd)
        };

        let relay = self
            .spawn_pane_relay(&pane_id, program, args, size, Some(&cwd), seed_caps)
            .await?;
        let pane_info = PaneInfo {
            pane_id,
            pane_index: new_index,
            program: relay.program.clone(),
            size: relay.size,
            attached_clients: vec![],
            status: SessionStatus::Running,
            title: relay.title.lock().unwrap().clone(),
        };

        let (new_layout, focused) = {
            let mut sessions = self.sessions.write().await;
            let state = sessions
                .get_mut(word_id)
                .ok_or_else(|| KmuxError::SessionNotFound {
                    name: word_id.to_string(),
                })?;
            state.panes.insert(new_index, relay);
            let tab = state
                .tab_mut(tab_index)
                .ok_or_else(|| KmuxError::SessionNotFound {
                    name: format!("{word_id} tab {tab_index}"),
                })?;
            // If the original target raced away, fall back to splitting the
            // current focused leaf so the freshly spawned pane is never orphaned.
            let target = if tab.layout.leaves().contains(&from_pane) {
                from_pane
            } else {
                tab.focused_pane
            };
            layout::split(&mut tab.layout, target, new_index, dir);
            tab.focused_pane = new_index;
            (tab.layout.clone(), tab.focused_pane)
        };

        Ok((pane_info, new_layout, focused))
    }

    /// Swap two panes' positions within a tab. Returns the new layout + focus.
    pub async fn swap_panes(
        &self,
        word_id: &str,
        tab_index: u32,
        a: u32,
        b: u32,
    ) -> Result<(LayoutNode, u32)> {
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;
        let tab = state
            .tab_mut(tab_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: format!("{word_id} tab {tab_index}"),
            })?;
        layout::swap(&mut tab.layout, a, b);
        Ok((tab.layout.clone(), tab.focused_pane))
    }

    /// Adjust split weights at `path` within a tab. Returns the new layout + focus.
    pub async fn set_layout_ratios(
        &self,
        word_id: &str,
        tab_index: u32,
        path: &[u32],
        ratios: &[u16],
    ) -> Result<(LayoutNode, u32)> {
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;
        let tab = state
            .tab_mut(tab_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: format!("{word_id} tab {tab_index}"),
            })?;
        layout::set_ratios(&mut tab.layout, path, ratios);
        Ok((tab.layout.clone(), tab.focused_pane))
    }

    /// Regenerate a tab's layout into a preset [`LayoutScheme`] from its current
    /// panes (in leaf order). Returns the new layout + focus.
    pub async fn apply_layout_scheme(
        &self,
        word_id: &str,
        tab_index: u32,
        scheme: LayoutScheme,
    ) -> Result<(LayoutNode, u32)> {
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;
        let tab = state
            .tab_mut(tab_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: format!("{word_id} tab {tab_index}"),
            })?;
        let leaves = tab.layout.leaves();
        tab.layout = layout::apply_scheme(&leaves, scheme);
        Ok((tab.layout.clone(), tab.focused_pane))
    }

    /// Set the shared input focus within a tab. Returns the layout + new focus.
    pub async fn set_tab_focus(
        &self,
        word_id: &str,
        tab_index: u32,
        pane_index: u32,
    ) -> Result<(LayoutNode, u32)> {
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;
        let tab = state
            .tab_mut(tab_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: format!("{word_id} tab {tab_index}"),
            })?;
        if tab.layout.leaves().contains(&pane_index) {
            tab.focused_pane = pane_index;
        }
        Ok((tab.layout.clone(), tab.focused_pane))
    }
}
