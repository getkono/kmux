//! The session picker overlay: move the selection, filter the list.

use super::super::{AppCore, KeyResult};

impl AppCore {
    /// Handle [`Action::PickerUp`](crate::mode::Action::PickerUp).
    pub(super) fn on_picker_up(&mut self) -> KeyResult {
        if self.session_picker_selected > 0 {
            self.session_picker_selected -= 1;
        }
        KeyResult::Continue
    }

    /// Handle [`Action::PickerDown`](crate::mode::Action::PickerDown).
    pub(super) fn on_picker_down(&mut self) -> KeyResult {
        // total rows = 1 ("[+] New session") + filtered sessions.
        let total = self.session_picker_matches().len() + 1;
        if self.session_picker_selected + 1 < total {
            self.session_picker_selected += 1;
        }
        KeyResult::Continue
    }

    /// Handle [`Action::PickerSearchChar`](crate::mode::Action::PickerSearchChar).
    pub(super) fn on_picker_search_char(&mut self, ch: char) -> KeyResult {
        self.session_picker_search.push(ch);
        self.session_picker_selected = 0;
        KeyResult::Continue
    }

    /// Handle [`Action::PickerSearchBackspace`](crate::mode::Action::PickerSearchBackspace).
    pub(super) fn on_picker_search_backspace(&mut self) -> KeyResult {
        self.session_picker_search.pop();
        self.session_picker_selected = 0;
        KeyResult::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::fixture_core;
    use super::*;

    /// The selection is an index into a list that starts with a synthetic
    /// "[+] New session" row, so zero is always reachable and always valid.
    #[test]
    fn moving_up_stops_at_the_new_session_row() {
        let mut core = fixture_core();
        core.session_picker_selected = 1;
        assert_eq!(core.on_picker_up(), KeyResult::Continue);
        assert_eq!(core.session_picker_selected, 0);
        core.on_picker_up();
        assert_eq!(core.session_picker_selected, 0, "no wrap past the top");
    }

    /// With no sessions the only row is "[+] New session", so down is a no-op.
    /// This is the case an off-by-one would turn into a selection pointing at
    /// a row that does not exist.
    #[test]
    fn moving_down_stops_at_the_last_row() {
        let mut core = fixture_core();
        assert_eq!(core.session_picker_matches().len(), 0, "no sessions yet");
        core.on_picker_down();
        assert_eq!(core.session_picker_selected, 0);
    }

    #[test]
    fn typing_a_filter_appends_and_returns_to_the_first_row() {
        let mut core = fixture_core();
        core.session_picker_selected = 4;
        core.on_picker_search_char('e');
        core.on_picker_search_char('a');
        assert_eq!(core.session_picker_search, "ea");
        assert_eq!(
            core.session_picker_selected, 0,
            "the old selection indexed a list that no longer exists"
        );
    }

    #[test]
    fn erasing_the_filter_also_returns_to_the_first_row() {
        let mut core = fixture_core();
        core.session_picker_search = "ea".to_string();
        core.session_picker_selected = 4;
        core.on_picker_search_backspace();
        assert_eq!(core.session_picker_search, "e");
        assert_eq!(core.session_picker_selected, 0);
    }

    #[test]
    fn erasing_an_empty_filter_is_harmless() {
        let mut core = fixture_core();
        core.on_picker_search_backspace();
        assert_eq!(core.session_picker_search, "");
        assert_eq!(core.session_picker_selected, 0);
    }
}
