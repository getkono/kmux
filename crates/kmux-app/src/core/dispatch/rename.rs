//! The rename overlay, shared by sessions and tabs — open, type, erase.

use super::super::{AppCore, KeyResult};
use crate::mode::Mode;

impl AppCore {
    /// Handle [`Action::RenameSession`](crate::mode::Action::RenameSession).
    pub(super) fn on_rename_session(&mut self) -> KeyResult {
        if let Some(word_id) = self.mgr.active_session().map(ToString::to_string) {
            let current_name = self
                .mgr
                .session_list()
                .iter()
                .find(|e| e.meta.word_id == word_id)
                .map(|e| e.meta.name.clone())
                .unwrap_or_default();
            self.mode = Mode::RenameSession {
                buffer: current_name,
                word_id,
            };
        }
        KeyResult::Continue
    }

    /// Handle [`Action::RenameTab`](crate::mode::Action::RenameTab).
    pub(super) fn on_rename_tab(&mut self) -> KeyResult {
        if let (Some(word_id), Some(tab_index)) = (
            self.mgr.active_session().map(ToString::to_string),
            self.mgr.active_tab(),
        ) {
            let buffer = self.mgr.active_tab_name().unwrap_or_default();
            self.mode = Mode::RenameTab {
                word_id,
                tab_index,
                buffer,
            };
        }
        KeyResult::Continue
    }

    /// Handle [`Action::RenameChar`](crate::mode::Action::RenameChar).
    pub(super) fn on_rename_char(&mut self, ch: char) -> KeyResult {
        if let Mode::RenameSession { buffer, .. } | Mode::RenameTab { buffer, .. } = &mut self.mode
        {
            buffer.push(ch);
        }
        KeyResult::Continue
    }

    /// Handle [`Action::RenameBackspace`](crate::mode::Action::RenameBackspace).
    pub(super) fn on_rename_backspace(&mut self) -> KeyResult {
        if let Mode::RenameSession { buffer, .. } | Mode::RenameTab { buffer, .. } = &mut self.mode
        {
            buffer.pop();
        }
        KeyResult::Continue
    }
}
