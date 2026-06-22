use std::path::Path;

use kmux_protocol::format_pane_id;
use kmux_protocol::messages::{ClientMessage, PeerId, TermSize};

use super::SessionManager;

impl SessionManager {
    /// Switch to a different session, attaching its active tab's visible pane
    /// set and focusing that tab's focused pane.
    pub fn select_session(&mut self, word_id: String) {
        let Some(entry) = self.session_list.iter().find(|e| e.meta.word_id == word_id) else {
            return;
        };
        let tab_index = entry.active_tab;
        // The active tab's focus + visible set; fall back to the first pane if
        // the session has no tab metadata yet.
        let (focus_idx, visible) = match entry.tabs.iter().find(|t| t.tab_index == tab_index) {
            Some(t) => (
                t.focused_pane,
                t.layout
                    .leaves()
                    .into_iter()
                    .map(|i| format_pane_id(&word_id, i))
                    .collect::<Vec<_>>(),
            ),
            None => match entry.panes.first() {
                Some(p) => (p.pane_index, vec![p.pane_id.clone()]),
                None => (0, vec![]),
            },
        };
        self.active_session = Some(word_id.clone());
        self.active_tab = Some(tab_index);
        self.set_visible_set(visible);
        self.focus_from_tab(&word_id, focus_idx);
    }

    /// Select a specific pane: switch session/tab as needed so it becomes
    /// visible, then focus it within its tab.
    pub fn select_pane(&mut self, pane_id: String) {
        match self.locate_pane(&pane_id) {
            Some((word_id, tab_index)) => {
                if self.active_session.as_deref() != Some(word_id.as_str()) {
                    self.select_session(word_id);
                } else if self.active_tab != Some(tab_index) {
                    self.select_tab(tab_index);
                }
                self.focus_pane(pane_id);
            }
            None => {
                // Pane not in the cached tabs: fall back to a single fresh attach.
                for prev in std::mem::take(&mut self.visible_panes) {
                    if prev != pane_id {
                        self.send_ws(ClientMessage::Detach { pane_id: prev });
                    }
                }
                if let Some(buf) = self.buffers.get_mut(&pane_id) {
                    buf.clear();
                }
                self.active_pane = Some(pane_id.clone());
                self.visible_panes = vec![pane_id.clone()];
                self.attach_fresh(pane_id);
            }
        }
    }

    /// Cycle to the next/previous session by offset.
    pub fn cycle_session(&mut self, offset: i32) {
        if self.session_list.is_empty() {
            return;
        }
        let current_idx = self
            .active_session
            .as_ref()
            .and_then(|wid| {
                self.session_list
                    .iter()
                    .position(|e| &e.meta.word_id == wid)
            })
            .unwrap_or(0);
        let len = self.session_list.len() as i32;
        let new_idx = ((current_idx as i32 + offset).rem_euclid(len)) as usize;
        let word_id = self.session_list[new_idx].meta.word_id.clone();
        self.select_session(word_id);
    }

    /// Cycle to the next/previous pane within the active session.
    pub fn cycle_pane(&mut self, offset: i32) {
        let word_id = match &self.active_session {
            Some(w) => w.clone(),
            None => return,
        };
        let panes: Vec<String> = self
            .session_list
            .iter()
            .find(|e| e.meta.word_id == word_id)
            .map(|e| e.panes.iter().map(|p| p.pane_id.clone()).collect())
            .unwrap_or_default();
        if panes.is_empty() {
            return;
        }
        let current_idx = self
            .active_pane
            .as_ref()
            .and_then(|pid| panes.iter().position(|p| p == pid))
            .unwrap_or(0);
        let len = panes.len() as i32;
        let new_idx = ((current_idx as i32 + offset).rem_euclid(len)) as usize;
        self.select_pane(panes[new_idx].clone());
    }

    /// Create a new session.
    ///
    /// `name` — optional display name; defaults to the basename of `cwd`.
    /// `cwd`  — working directory for the new session. Callers should supply an
    ///          explicit path: the app layer never assumes where a session
    ///          opens (the GUI seeds it from the focused session's cwd or a
    ///          directory the user picks). `None` is forwarded verbatim; relying
    ///          on it is a bug, since the daemon then resolves the path against
    ///          *its own* working directory, not the client's.
    pub fn create_session(&mut self, name: Option<&str>, cwd: Option<&str>, size: TermSize) {
        self.create_session_with_program(name, cwd, None, &[], size);
    }

    /// Like [`create_session`](Self::create_session) but runs an explicit
    /// `program` (with `args`) in the initial pane instead of the system shell.
    /// Used by `kmux diagnostic <test>` to spawn the render-diagnostic emitter
    /// (issue #145). `program == None` is equivalent to [`create_session`].
    pub fn create_session_with_program(
        &mut self,
        name: Option<&str>,
        cwd: Option<&str>,
        program: Option<&str>,
        args: &[String],
        size: TermSize,
    ) {
        if self.ws_sender.is_some() {
            let rid = self.next_rid();
            self.send_ws(ClientMessage::SessionCreate {
                request_id: rid,
                name: name.map(|n| n.to_string()),
                cwd: cwd.map(|c| c.to_string()),
                program: program.map(|p| p.to_string()),
                args: args.to_vec(),
                size,
                // Local create; remote creates go through create_session_on_peer.
                peer: None,
            });
        }
    }

    /// Create a new session on a federated `peer` (issue #121). Like
    /// [`create_session`](Self::create_session) but the hub forwards the create
    /// upstream to `peer` and registers the result under a local word, replying
    /// `SessionCreated` with the localized entry.
    pub fn create_session_on_peer(
        &mut self,
        name: Option<&str>,
        cwd: Option<&str>,
        peer: PeerId,
        size: TermSize,
    ) {
        if self.ws_sender.is_some() {
            let rid = self.next_rid();
            self.send_ws(ClientMessage::SessionCreate {
                request_id: rid,
                name: name.map(|n| n.to_string()),
                cwd: cwd.map(|c| c.to_string()),
                program: None,
                args: vec![],
                size,
                peer: Some(peer),
            });
        }
    }

    /// Ask the daemon to list the directories under `path` (empty ⇒ the daemon
    /// resolves a default). Records the request id so a later
    /// [`ServerMessage::DirectoryListing`](kmux_protocol::messages::ServerMessage::DirectoryListing)
    /// can be matched and stale replies dropped. Drives the app-layer directory
    /// browser used to pick where a new session is created.
    ///
    /// Drops any previous listing so the browser shows only the freshly-targeted
    /// directory while the reply is in flight (the stale listing must not back a
    /// "create here" in the directory we just navigated away from).
    pub fn request_list_directory(&mut self, path: String) {
        if self.ws_sender.is_some() {
            let rid = self.next_rid();
            self.pending_dir_request = Some(rid);
            self.dir_listing = None;
            self.send_ws(ClientMessage::ListDirectory {
                request_id: rid,
                path,
            });
        }
    }

    /// Create a new pane in the active session.
    pub fn create_pane(&mut self, size: TermSize) {
        if let Some(word_id) = self.active_session.clone() {
            let rid = self.next_rid();
            self.send_ws(ClientMessage::PaneCreate {
                request_id: rid,
                word_id,
                program: None,
                args: vec![],
                size,
            });
        }
    }

    /// Close the active pane.
    pub fn close_pane(&mut self) {
        if let Some(pane_id) = self.active_pane.clone() {
            self.close_pane_id(&pane_id);
        }
    }

    /// Send `PaneClose` for a specific pane (issue #86: the deferred soft-close
    /// fires this once its grace window elapses, since the target pane may no
    /// longer be the active one). Harmless if the pane is already gone.
    pub fn close_pane_id(&mut self, pane_id: &str) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::PaneClose {
            request_id: rid,
            pane_id: pane_id.to_string(),
        });
    }

    /// Whether the pane's shell is still running (issue #86 health check). False
    /// for an already-exited or unknown pane, so the soft-close grace is skipped
    /// for those (they close immediately).
    pub fn is_pane_running(&self, pane_id: &str) -> bool {
        self.session_list
            .iter()
            .flat_map(|e| &e.panes)
            .find(|p| p.pane_id == pane_id)
            .is_some_and(|p| matches!(p.status, kmux_protocol::messages::SessionStatus::Running))
    }

    /// Close the entire active session (all its panes).
    pub fn close_session(&mut self, word_id: &str) {
        let rid = self.next_rid();
        self.send_ws(ClientMessage::SessionClose {
            request_id: rid,
            word_id: word_id.to_string(),
        });
    }

    /// Rename the active session's display name.
    pub fn rename_session(&mut self, word_id: &str, new_name: &str) {
        if !new_name.is_empty() {
            let rid = self.next_rid();
            self.send_ws(ClientMessage::SessionRename {
                request_id: rid,
                word_id: word_id.to_string(),
                new_name: new_name.to_string(),
            });
        }
    }

    /// Find the first session whose CWD exactly matches `cwd`, returning its word_id.
    pub fn find_session_by_cwd(&self, cwd: &str) -> Option<String> {
        self.session_list
            .iter()
            .find(|e| e.meta.cwd == cwd)
            .map(|e| e.meta.word_id.clone())
    }

    /// Find the first session whose display name or word_id matches `name`.
    pub fn find_session_by_name(&self, name: &str) -> Option<String> {
        self.session_list
            .iter()
            .find(|e| e.meta.name == name || e.meta.word_id == name)
            .map(|e| e.meta.word_id.clone())
    }

    /// Compute the display name for a session, disambiguating by parent directory
    /// if multiple sessions share the same name.
    pub fn display_name_for(&self, word_id: &str) -> String {
        let Some(entry) = self.session_list.iter().find(|e| e.meta.word_id == word_id) else {
            return word_id.to_string();
        };
        let name = &entry.meta.name;
        let cwd = &entry.meta.cwd;

        // Count how many sessions share the same display name
        let same_name_count = self
            .session_list
            .iter()
            .filter(|e| &e.meta.name == name)
            .count();

        if same_name_count <= 1 {
            name.clone()
        } else {
            // Show the parent directory to disambiguate
            let parent = Path::new(cwd)
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or(cwd.as_str());
            format!("{name} ({parent})")
        }
    }
}
