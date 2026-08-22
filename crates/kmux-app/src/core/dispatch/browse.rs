//! The two fuzzy-filtered list overlays — the directory picker and the
//! launcher. Same shape, same keys, different rows.

use super::super::{AppCore, KeyResult};

impl AppCore {
    /// Handle [`Action::DirPickerChar`](crate::mode::Action::DirPickerChar).
    pub(super) fn on_dir_picker_char(&mut self, ch: char) -> KeyResult {
        self.dir_picker_buffer.push(ch);
        self.dir_picker_selected = 0;
        KeyResult::Continue
    }

    /// Handle [`Action::DirPickerBackspace`](crate::mode::Action::DirPickerBackspace).
    pub(super) fn on_dir_picker_backspace(&mut self) -> KeyResult {
        self.dir_picker_buffer.pop();
        self.dir_picker_selected = 0;
        KeyResult::Continue
    }

    /// Handle [`Action::DirPickerDown`](crate::mode::Action::DirPickerDown).
    pub(super) fn on_dir_picker_down(&mut self) -> KeyResult {
        let count = self.dir_browser_rows().len();
        if count > 0 && self.dir_picker_selected + 1 < count {
            self.dir_picker_selected += 1;
        }
        KeyResult::Continue
    }

    /// Handle [`Action::LaunchSearchChar`](crate::mode::Action::LaunchSearchChar).
    pub(super) fn on_launch_search_char(&mut self, ch: char) -> KeyResult {
        self.launch_search.push(ch);
        self.launch_selected = 0;
        KeyResult::Continue
    }

    /// Handle [`Action::LaunchSearchBackspace`](crate::mode::Action::LaunchSearchBackspace).
    pub(super) fn on_launch_search_backspace(&mut self) -> KeyResult {
        self.launch_search.pop();
        self.launch_selected = 0;
        KeyResult::Continue
    }

    /// Handle [`Action::LaunchDown`](crate::mode::Action::LaunchDown).
    pub(super) fn on_launch_down(&mut self) -> KeyResult {
        let count = self.launch_rows().len();
        if count > 0 && self.launch_selected + 1 < count {
            self.launch_selected += 1;
        }
        KeyResult::Continue
    }
}
