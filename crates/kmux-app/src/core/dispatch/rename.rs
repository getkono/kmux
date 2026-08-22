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

#[cfg(test)]
mod tests {
    use super::super::testing::{core_with_active_pane, fixture_core};
    use super::*;
    use kmux_protocol::messages::SessionStatus;

    /// The overlay opens seeded with the *current* name, so Enter with no
    /// typing is a no-op rather than a rename to the empty string.
    #[test]
    fn opening_the_session_rename_seeds_the_buffer_with_the_current_name() {
        let mut core = core_with_active_pane(SessionStatus::Running);
        core.mgr.select_session("eagle".to_string());
        assert_eq!(core.on_rename_session(), KeyResult::Continue);
        match &core.mode {
            Mode::RenameSession { buffer, word_id } => {
                assert_eq!(word_id, "eagle");
                assert_eq!(buffer, "eagle", "seeded, not blank");
            }
            other => panic!("expected RenameSession, got {other:?}"),
        }
    }

    #[test]
    fn opening_the_tab_rename_names_the_tab_it_will_rename() {
        let mut core = core_with_active_pane(SessionStatus::Running);
        core.mgr.select_session("eagle".to_string());
        core.on_rename_tab();
        match &core.mode {
            Mode::RenameTab {
                word_id,
                tab_index,
                buffer,
            } => {
                assert_eq!(word_id, "eagle");
                assert_eq!(*tab_index, 0_u32);
                assert_eq!(buffer, "1", "the tab's current name");
            }
            other => panic!("expected RenameTab, got {other:?}"),
        }
    }

    /// With no session there is nothing to rename, and opening an overlay that
    /// targets nothing would strand the user in a mode they cannot submit.
    #[test]
    fn opening_a_rename_with_no_active_session_stays_in_normal_mode() {
        let mut core = fixture_core();
        core.on_rename_session();
        assert!(matches!(core.mode, Mode::Normal));
        core.on_rename_tab();
        assert!(matches!(core.mode, Mode::Normal));
    }

    /// One editor drives both overlays, so both have to accept the same keys.
    #[test]
    fn typing_and_erasing_edit_whichever_rename_overlay_is_open() {
        for (label, mode) in [
            (
                "session",
                Mode::RenameSession {
                    buffer: "ab".to_string(),
                    word_id: "eagle".to_string(),
                },
            ),
            (
                "tab",
                Mode::RenameTab {
                    word_id: "eagle".to_string(),
                    tab_index: 0,
                    buffer: "ab".to_string(),
                },
            ),
        ] {
            let mut core = fixture_core();
            core.mode = mode;
            core.on_rename_char('c');
            assert_eq!(rename_buffer(&core), Some("abc"), "{label}: typed");
            core.on_rename_backspace();
            core.on_rename_backspace();
            assert_eq!(rename_buffer(&core), Some("a"), "{label}: erased");
        }
    }

    #[test]
    fn erasing_an_empty_rename_buffer_is_harmless() {
        let mut core = fixture_core();
        core.mode = Mode::RenameSession {
            buffer: String::new(),
            word_id: "eagle".to_string(),
        };
        core.on_rename_backspace();
        assert_eq!(rename_buffer(&core), Some(""));
    }

    #[test]
    fn the_rename_editor_does_nothing_outside_a_rename_overlay() {
        let mut core = fixture_core();
        assert_eq!(core.on_rename_char('x'), KeyResult::Continue);
        assert_eq!(core.on_rename_backspace(), KeyResult::Continue);
        assert!(matches!(core.mode, Mode::Normal));
    }

    fn rename_buffer(core: &AppCore) -> Option<&str> {
        match &core.mode {
            Mode::RenameSession { buffer, .. } | Mode::RenameTab { buffer, .. } => {
                Some(buffer.as_str())
            }
            _ => None,
        }
    }
}
