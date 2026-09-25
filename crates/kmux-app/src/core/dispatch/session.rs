//! Session-level actions: opening, closing and jumping between sessions.

use super::super::{AppCore, KeyResult};

use crate::mode::Mode;

impl AppCore {
    /// Handle [`Action::CreateSession`](crate::mode::Action::CreateSession).
    pub(super) fn on_create_session(&mut self) -> KeyResult {
        // Never assume where a new session opens: default to the focused
        // session's cwd, falling back to the app's initial cwd. A bare
        // create with no cwd would resolve against the *daemon's* working
        // directory, not the user's.
        let cwd = self
            .active_session_cwd()
            .unwrap_or_else(|| self.initial_cwd.clone());
        self.mgr.create_session(None, Some(&cwd), self.term_size);
        KeyResult::Continue
    }

    /// Handle [`Action::ConfirmCloseSession`](crate::mode::Action::ConfirmCloseSession).
    pub(super) fn on_confirm_close_session(&mut self) -> KeyResult {
        if let Mode::ConfirmCloseSession { word_id, .. } =
            std::mem::replace(&mut self.mode, Mode::Normal)
        {
            self.mgr.close_session(&word_id);
        }
        KeyResult::Continue
    }

    /// Handle [`Action::JumpToSession`](crate::mode::Action::JumpToSession).
    pub(super) fn on_jump_to_session(&mut self, idx: usize) -> KeyResult {
        if idx < self.mgr.session_list().len() {
            let word_id = self.mgr.session_list()[idx].meta.word_id.clone();
            self.mgr.select_session(word_id);
        }
        KeyResult::Continue
    }

    /// Handle [`Action::Disconnect`](crate::mode::Action::Disconnect).
    pub(super) fn on_disconnect(&mut self) -> KeyResult {
        self.mgr.disconnect();
        self.mode = Mode::Normal;
        KeyResult::Continue
    }
}
