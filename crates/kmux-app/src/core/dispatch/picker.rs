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
