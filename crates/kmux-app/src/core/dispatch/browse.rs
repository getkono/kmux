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

#[cfg(test)]
mod tests {
    use super::super::testing::fixture_core;

    #[test]
    fn typing_in_the_directory_picker_appends_and_resets_the_selection() {
        let mut core = fixture_core();
        core.dir_picker_selected = 3;
        core.on_dir_picker_char('s');
        core.on_dir_picker_char('r');
        assert_eq!(core.dir_picker_buffer, "sr");
        assert_eq!(core.dir_picker_selected, 0);
    }

    #[test]
    fn erasing_in_the_directory_picker_resets_the_selection_too() {
        let mut core = fixture_core();
        core.dir_picker_buffer = "src".to_string();
        core.dir_picker_selected = 3;
        core.on_dir_picker_backspace();
        assert_eq!(core.dir_picker_buffer, "sr");
        assert_eq!(core.dir_picker_selected, 0);
    }

    #[test]
    fn erasing_an_empty_directory_filter_is_harmless() {
        let mut core = fixture_core();
        core.on_dir_picker_backspace();
        assert_eq!(core.dir_picker_buffer, "");
    }

    /// The selection must never run past the last row, however many times the
    /// key is pressed — an index nothing can render is the bug this guards.
    #[test]
    fn moving_down_the_directory_list_stops_at_the_last_row() {
        let mut core = fixture_core();
        let rows = core.dir_browser_rows().len();
        for _ in 0..rows + 3 {
            core.on_dir_picker_down();
        }
        assert_eq!(
            core.dir_picker_selected,
            rows.saturating_sub(1),
            "{rows} rows"
        );
    }

    #[test]
    fn typing_in_the_launcher_appends_and_resets_the_selection() {
        let mut core = fixture_core();
        core.launch_selected = 2;
        core.on_launch_search_char('z');
        assert_eq!(core.launch_search, "z");
        assert_eq!(core.launch_selected, 0);
    }

    #[test]
    fn erasing_in_the_launcher_resets_the_selection_too() {
        let mut core = fixture_core();
        core.launch_search = "zsh".to_string();
        core.launch_selected = 2;
        core.on_launch_search_backspace();
        assert_eq!(core.launch_search, "zs");
        assert_eq!(core.launch_selected, 0);
    }

    #[test]
    fn erasing_an_empty_launcher_filter_is_harmless() {
        let mut core = fixture_core();
        core.on_launch_search_backspace();
        assert_eq!(core.launch_search, "");
    }

    #[test]
    fn moving_down_the_launcher_stops_at_the_last_row() {
        let mut core = fixture_core();
        let rows = core.launch_rows().len();
        for _ in 0..rows + 3 {
            core.on_launch_down();
        }
        assert_eq!(
            core.launch_selected,
            rows.saturating_sub(1),
            "{rows} rows, so the last index is {}",
            rows.saturating_sub(1)
        );
    }
}
