//! Server-authoritative tab + layout operations.
//!
//! Each public method validates and mutates session state under the `sessions`
//! write lock, then returns the new authoritative tree (and focus) so the caller
//! ([`crate::client_handler`]) can reply to the requester and broadcast a
//! `LayoutUpdate` to every other client viewing the tab. PTY spawning happens
//! outside the lock (mirroring `ServerApp::create_pane`).

use std::path::PathBuf;

use kmux_protocol::format_pane_id;
use kmux_protocol::messages::{
    ClientCapabilities, LayoutNode, LayoutScheme, PaneInfo, SessionStatus, SplitDir, TabInfo,
    TermSize,
};
use kmux_pty::error::{KmuxError, Result};

use super::helpers::{resolve_cwd, session_not_found, tab_not_found};
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
    /// Move a tab and return the complete authoritative order.
    pub async fn reorder_tab(
        &self,
        word_id: &str,
        tab_index: u32,
        new_position: u32,
    ) -> Result<Vec<u32>> {
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;
        let Some(old_position) = state.tabs.iter().position(|tab| tab.tab_index == tab_index)
        else {
            return Ok(state.tabs.iter().map(|tab| tab.tab_index).collect());
        };
        let tab = state.tabs.remove(old_position);
        let destination = usize::try_from(new_position)
            .unwrap_or(usize::MAX)
            .min(state.tabs.len());
        state.tabs.insert(destination, tab);
        Ok(state.tabs.iter().map(|tab| tab.tab_index).collect())
    }

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
            let pane_id = format_pane_id(word_id, pane_index);
            let cwd = resolve_cwd(&PathBuf::from(&state.meta.cwd));
            (pane_index, tab_index, pane_id, cwd)
        };

        let relay = self
            .spawn_pane_relay(&pane_id, program, args, size, Some(&cwd), seed_caps)
            .await?;
        let pane_info = relay.to_pane_info(pane_id, pane_index, Vec::new(), SessionStatus::Running);

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
    ///
    /// Errors if `word_id` names no live session or the session has no tab
    /// `tab_index` — both used to answer `Ok(false)`, which is also what a
    /// successful close of a non-final tab answers.
    pub async fn close_tab(&self, word_id: &str, tab_index: u32) -> Result<bool> {
        // Collect the tab's pane ids (under a read lock), then kill the PTYs.
        let pane_ids: Vec<(u32, String)> = {
            let sessions = self.sessions.read().await;
            let Some(state) = sessions.get(word_id) else {
                return Err(session_not_found(word_id));
            };
            let Some(tab) = state.tab(tab_index) else {
                return Err(tab_not_found(word_id, tab_index));
            };
            tab.layout
                .leaves()
                .into_iter()
                .map(|idx| (idx, format_pane_id(word_id, idx)))
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
                // The session went away while its PTYs were being killed, which
                // is what `session_closed` reports. Answering `false` here said
                // the opposite.
                return Ok(true);
            };
            for (idx, _) in &pane_ids {
                state.panes.remove(idx);
            }
            state.tabs.retain(|t| t.tab_index != tab_index);
            if state.active_tab == tab_index {
                state.active_tab = state.tabs.first().map_or(0, |t| t.tab_index);
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
                    name: format_pane_id(word_id, from_pane),
                });
            }
            let new_index = state.next_pane_index;
            state.next_pane_index += 1;
            let pane_id = format_pane_id(word_id, new_index);
            let cwd = resolve_cwd(&PathBuf::from(&state.meta.cwd));
            (new_index, pane_id, cwd)
        };

        let relay = self
            .spawn_pane_relay(&pane_id, program, args, size, Some(&cwd), seed_caps)
            .await?;
        let pane_info = relay.to_pane_info(pane_id, new_index, Vec::new(), SessionStatus::Running);

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A session with one tab holding one pane, running a long-lived childless
    /// process so the tab teardown is deterministic.
    async fn app_with_one_session() -> (ServerApp, String) {
        let app = ServerApp::new("tok".to_string());
        let entry = app
            .create_session(
                None,
                Some("/tmp".to_string()),
                Some("/bin/sleep".to_string()),
                vec!["30".to_string()],
                TermSize::default(),
                &ClientCapabilities::default(),
            )
            .await
            .expect("create_session");
        let word = entry.meta.word_id;
        (app, word)
    }

    #[tokio::test]
    async fn closing_a_tab_of_an_unknown_session_errors_instead_of_answering_false() {
        let app = ServerApp::new("tok".to_string());
        let err = app
            .close_tab("nosuch", 0)
            .await
            .expect_err("a word id no session uses");
        assert!(
            matches!(&err, KmuxError::SessionNotFound { name } if name == "nosuch"),
            "expected SessionNotFound naming the word id, got {err:?}"
        );
    }

    #[tokio::test]
    async fn closing_a_tab_a_live_session_does_not_have_errors_naming_the_tab() {
        let (app, word) = app_with_one_session().await;
        let err = app
            .close_tab(&word, 7)
            .await
            .expect_err("tab 7 does not exist");
        assert!(
            matches!(&err, KmuxError::SessionNotFound { name } if *name == format!("{word} tab 7")),
            "expected the error to locate the tab, got {err:?}"
        );
        // The failed close must leave the session's real tab alone.
        assert_eq!(app.sessions.read().await[&word].tabs.len(), 1);

        let _ = app.close_session(&word).await;
    }

    /// The happy path the not-found errors have to stay distinguishable from:
    /// closing the only tab closes the session and answers `true`.
    #[tokio::test]
    async fn closing_the_last_tab_closes_the_session() {
        let (app, word) = app_with_one_session().await;
        let session_closed = app.close_tab(&word, 0).await.expect("close_tab");
        assert!(session_closed, "the session had no other tab");
        assert!(app.sessions.read().await.get(&word).is_none());
    }
}
