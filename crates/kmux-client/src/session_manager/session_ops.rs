use std::path::Path;

use kmux_protocol::messages::{ClientMessage, TermSize};

use super::SessionManager;

impl SessionManager {
    /// Switch to a different session by word_id (attaches to its first pane).
    pub fn select_session(&mut self, word_id: String) {
        if let Some(prev_pane) = self.active_pane.take() {
            self.send_ws(ClientMessage::Detach { pane_id: prev_pane });
        }
        let first_pane = self
            .session_list
            .iter()
            .find(|e| e.meta.word_id == word_id)
            .and_then(|e| e.panes.first())
            .map(|p| p.pane_id.clone());
        self.active_session = Some(word_id);
        self.active_pane = first_pane.clone();
        if let Some(pane_id) = first_pane {
            if let Some(buf) = self.buffers.get_mut(&pane_id) {
                buf.clear();
            }
            self.attach_fresh(pane_id);
        }
    }

    /// Switch to a specific pane.
    pub fn select_pane(&mut self, pane_id: String) {
        if let Some(prev_pane) = self.active_pane.take()
            && prev_pane != pane_id
        {
            self.send_ws(ClientMessage::Detach { pane_id: prev_pane });
        }
        if let Some(buf) = self.buffers.get_mut(&pane_id) {
            buf.clear();
        }
        self.active_pane = Some(pane_id.clone());
        self.attach_fresh(pane_id);
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
    /// `cwd`  — optional working directory; defaults to the client's current directory.
    pub fn create_session(&mut self, name: Option<&str>, cwd: Option<&str>, size: TermSize) {
        if self.ws_sender.is_some() {
            let rid = self.next_rid();
            let resolved_cwd = cwd.map(|c| c.to_string()).or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|p| p.to_str().map(|s| s.to_string()))
            });
            self.send_ws(ClientMessage::SessionCreate {
                request_id: rid,
                name: name.map(|n| n.to_string()),
                cwd: resolved_cwd,
                program: None,
                args: vec![],
                size,
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
            let rid = self.next_rid();
            self.send_ws(ClientMessage::PaneClose {
                request_id: rid,
                pane_id,
            });
        }
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
