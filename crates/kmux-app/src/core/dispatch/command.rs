//! The command palette's line editor.
//!
//! Nine of these are cursor and text movement over one `String`. They live
//! together because they share every invariant about where the caret may be.

use super::super::COMMAND_HISTORY_CAP;
use super::super::{AppCore, KeyResult};
use super::apply_hint_to_buffer;
use crate::cmd;
use crate::mode::Mode;

impl AppCore {
    /// ── Command palette editing ──────────────────────────────────────
    pub(super) fn on_command_char(&mut self, ch: char) -> KeyResult {
        if let Mode::Command(state) = &mut self.mode {
            let pos = state.cursor.min(state.buffer.len());
            state.buffer.insert(pos, ch);
            state.cursor = pos + ch.len_utf8();
            state.selected = 0;
            state.history_pos = None;
        }
        KeyResult::Continue
    }

    /// Handle [`Action::CommandBackspace`](crate::mode::Action::CommandBackspace).
    pub(super) fn on_command_backspace(&mut self) -> KeyResult {
        if let Mode::Command(state) = &mut self.mode {
            let pos = state.cursor.min(state.buffer.len());
            if pos > 0 {
                // Find the previous char boundary so we delete a full
                // grapheme rather than splitting a multi-byte char.
                let mut new_pos = pos - 1;
                while !state.buffer.is_char_boundary(new_pos) && new_pos > 0 {
                    new_pos -= 1;
                }
                state.buffer.replace_range(new_pos..pos, "");
                state.cursor = new_pos;
                state.selected = 0;
                state.history_pos = None;
            }
        }
        KeyResult::Continue
    }

    /// Handle [`Action::CommandLeft`](crate::mode::Action::CommandLeft).
    pub(super) fn on_command_left(&mut self) -> KeyResult {
        if let Mode::Command(state) = &mut self.mode
            && state.cursor > 0
        {
            let mut new_pos = state.cursor - 1;
            while !state.buffer.is_char_boundary(new_pos) && new_pos > 0 {
                new_pos -= 1;
            }
            state.cursor = new_pos;
        }
        KeyResult::Continue
    }

    /// Handle [`Action::CommandRight`](crate::mode::Action::CommandRight).
    pub(super) fn on_command_right(&mut self) -> KeyResult {
        if let Mode::Command(state) = &mut self.mode
            && state.cursor < state.buffer.len()
        {
            let mut new_pos = state.cursor + 1;
            while new_pos < state.buffer.len() && !state.buffer.is_char_boundary(new_pos) {
                new_pos += 1;
            }
            state.cursor = new_pos;
        }
        KeyResult::Continue
    }

    /// Handle [`Action::CommandHome`](crate::mode::Action::CommandHome).
    pub(super) fn on_command_home(&mut self) -> KeyResult {
        if let Mode::Command(state) = &mut self.mode {
            state.cursor = 0;
        }
        KeyResult::Continue
    }

    /// Handle [`Action::CommandEnd`](crate::mode::Action::CommandEnd).
    pub(super) fn on_command_end(&mut self) -> KeyResult {
        if let Mode::Command(state) = &mut self.mode {
            state.cursor = state.buffer.len();
        }
        KeyResult::Continue
    }

    /// Handle [`Action::CommandClearLine`](crate::mode::Action::CommandClearLine).
    pub(super) fn on_command_clear_line(&mut self) -> KeyResult {
        if let Mode::Command(state) = &mut self.mode {
            state.buffer.clear();
            state.cursor = 0;
            state.selected = 0;
            state.history_pos = None;
        }
        KeyResult::Continue
    }

    /// Handle [`Action::CommandDeleteWordBack`](crate::mode::Action::CommandDeleteWordBack).
    pub(super) fn on_command_delete_word_back(&mut self) -> KeyResult {
        if let Mode::Command(state) = &mut self.mode {
            let mut end = state.cursor.min(state.buffer.len());
            // Skip trailing whitespace.
            while end > 0 {
                let prev = state.buffer[..end].chars().next_back();
                match prev {
                    Some(c) if c.is_whitespace() => {
                        end -= c.len_utf8();
                    }
                    _ => break,
                }
            }
            let mut start = end;
            while start > 0 {
                let prev = state.buffer[..start].chars().next_back();
                match prev {
                    Some(c) if !c.is_whitespace() => {
                        start -= c.len_utf8();
                    }
                    _ => break,
                }
            }
            state.buffer.replace_range(start..state.cursor, "");
            state.cursor = start;
            state.selected = 0;
            state.history_pos = None;
        }
        KeyResult::Continue
    }

    /// Handle [`Action::CommandSubmit`](crate::mode::Action::CommandSubmit).
    pub(super) fn on_command_submit(&mut self) -> KeyResult {
        // Compute hints BEFORE we extract the state — they depend on
        // the live `Mode::Command` and we'll fall back to the selected
        // hint if the typed buffer doesn't parse cleanly.
        let hints = cmd::hint::build_hints(self);
        let Mode::Command(state) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return KeyResult::Continue;
        };
        let typed = state.buffer.trim().to_string();
        if typed.is_empty() {
            return KeyResult::Continue;
        }
        // Pick the buffer to actually run. If the typed text already
        // resolves to a known command, run it. Otherwise, if there's a
        // highlighted hint that completes a command name, apply it
        // (matches user expectation: "press Enter on the highlighted
        // suggestion"). Falls back to typed on no hints.
        let parses_cleanly = cmd::parse::parse(&typed, cmd::registry::ALL).is_ok();
        let buf = if parses_cleanly {
            typed.clone()
        } else if let Some(hint) = hints.get(state.selected.min(hints.len().saturating_sub(1))) {
            apply_hint_to_buffer(&state.buffer, hint).trim().to_string()
        } else {
            typed.clone()
        };
        // Push the *typed* form into history (so users can recall what
        // they actually pressed, not the auto-completed expansion).
        if self.command_history.back().map(String::as_str) != Some(typed.as_str()) {
            self.command_history.push_back(typed.clone());
            while self.command_history.len() > COMMAND_HISTORY_CAP {
                self.command_history.pop_front();
            }
        }
        let outcome = cmd::exec::run(self, &buf);
        match outcome {
            cmd::exec::Outcome::Continue => {}
            cmd::exec::Outcome::Quit => return KeyResult::Quit,
            cmd::exec::Outcome::Reconnect => return KeyResult::Reconnect,
        }
        KeyResult::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::fixture_core;
    use super::*;
    use crate::mode::CommandState;

    /// One editor action, named for the assertion message.
    type Edit = (&'static str, fn(&mut AppCore) -> KeyResult);

    /// A core sitting in the command palette with `buffer` typed and the caret
    /// at `cursor` bytes in.
    fn editing(buffer: &str, cursor: usize) -> AppCore {
        let mut core = fixture_core();
        core.mode = Mode::Command(CommandState {
            buffer: buffer.to_string(),
            cursor,
            selected: 3,
            history_pos: Some(1),
        });
        core
    }

    /// The `(buffer, cursor)` a core is showing, or `None` outside the palette.
    fn shown(core: &AppCore) -> Option<(String, usize)> {
        match &core.mode {
            Mode::Command(state) => Some((state.buffer.clone(), state.cursor)),
            _ => None,
        }
    }

    fn hint_state(core: &AppCore) -> Option<(usize, Option<usize>)> {
        match &core.mode {
            Mode::Command(state) => Some((state.selected, state.history_pos)),
            _ => None,
        }
    }

    #[test]
    fn typing_inserts_at_the_caret_and_moves_it_past_the_character() {
        let mut core = editing("ab", 1);
        assert_eq!(core.on_command_char('X'), KeyResult::Continue);
        assert_eq!(shown(&core), Some(("aXb".to_string(), 2)));
    }

    /// Editing invalidates both the highlighted hint and the history position:
    /// the dropdown was computed for text that no longer exists, and the buffer
    /// is no longer the recalled one.
    #[test]
    fn every_edit_resets_the_hint_selection_and_leaves_history() {
        let edits: [Edit; 4] = [
            ("insert", |c| c.on_command_char('z')),
            ("backspace", |c| c.on_command_backspace()),
            ("clear", |c| c.on_command_clear_line()),
            ("delete word", |c| c.on_command_delete_word_back()),
        ];
        for (label, edit) in edits {
            let mut core = editing("hello world", 11);
            edit(&mut core);
            assert_eq!(hint_state(&core), Some((0, None)), "after {label}");
        }
    }

    /// Moving the caret is not editing, so the dropdown and the recalled
    /// history entry both survive it.
    #[test]
    fn caret_movement_leaves_the_hint_selection_alone() {
        let moves: [Edit; 4] = [
            ("left", |c| c.on_command_left()),
            ("right", |c| c.on_command_right()),
            ("home", |c| c.on_command_home()),
            ("end", |c| c.on_command_end()),
        ];
        for (label, mv) in moves {
            let mut core = editing("hello", 2);
            mv(&mut core);
            assert_eq!(hint_state(&core), Some((3, Some(1))), "after {label}");
        }
    }

    #[test]
    fn backspace_at_the_start_of_the_buffer_changes_nothing() {
        let mut core = editing("ab", 0);
        core.on_command_backspace();
        assert_eq!(shown(&core), Some(("ab".to_string(), 0)));
        // Nothing was edited, so the dropdown state is untouched too.
        assert_eq!(hint_state(&core), Some((3, Some(1))));
    }

    /// The caret is a byte offset into a `String`, so every movement and
    /// deletion has to land on a char boundary. `é` is two bytes, `→` three,
    /// `🦀` four — one of each catches an off-by-one that ASCII would not.
    #[test]
    fn the_caret_never_splits_a_multibyte_character() {
        let text = "aé→🦀b";
        for start in [0, text.len()] {
            let mut core = editing(text, start);
            for _ in 0..text.chars().count() + 2 {
                if start == 0 {
                    core.on_command_right();
                } else {
                    core.on_command_left();
                }
                let (buf, cur) = shown(&core).expect("still editing");
                assert!(
                    buf.is_char_boundary(cur),
                    "caret {cur} split {buf:?} (started at {start})"
                );
            }
        }
    }

    #[test]
    fn backspace_removes_one_whole_character_not_one_byte() {
        let mut core = editing("a🦀", "a🦀".len());
        core.on_command_backspace();
        assert_eq!(shown(&core), Some(("a".to_string(), 1)));
        core.on_command_backspace();
        assert_eq!(shown(&core), Some((String::new(), 0)));
    }

    #[test]
    fn home_and_end_go_to_the_two_ends_of_the_buffer() {
        let mut core = editing("hello", 2);
        core.on_command_home();
        assert_eq!(shown(&core), Some(("hello".to_string(), 0)));
        core.on_command_end();
        assert_eq!(shown(&core), Some(("hello".to_string(), 5)));
    }

    #[test]
    fn the_caret_stops_at_both_ends_rather_than_wrapping() {
        let mut core = editing("ab", 0);
        core.on_command_left();
        assert_eq!(shown(&core), Some(("ab".to_string(), 0)));
        core.on_command_end();
        core.on_command_right();
        assert_eq!(shown(&core), Some(("ab".to_string(), 2)));
    }

    #[test]
    fn clear_line_empties_the_buffer_and_parks_the_caret_at_zero() {
        let mut core = editing("some text", 4);
        core.on_command_clear_line();
        assert_eq!(shown(&core), Some((String::new(), 0)));
    }

    #[test]
    fn delete_word_back_eats_the_word_and_the_gap_before_the_caret() {
        let mut core = editing("open some file", 14);
        core.on_command_delete_word_back();
        assert_eq!(shown(&core), Some(("open some ".to_string(), 10)));
        core.on_command_delete_word_back();
        assert_eq!(shown(&core), Some(("open ".to_string(), 5)));
    }

    /// Deleting from inside a word keeps everything to the right of the caret.
    #[test]
    fn delete_word_back_keeps_the_tail_after_the_caret() {
        let mut core = editing("open some file", 9);
        core.on_command_delete_word_back();
        assert_eq!(shown(&core), Some(("open  file".to_string(), 5)));
    }

    #[test]
    fn delete_word_back_on_trailing_space_only_eats_the_space_run() {
        let mut core = editing("word   ", 7);
        core.on_command_delete_word_back();
        assert_eq!(
            shown(&core),
            Some((String::new(), 0)),
            "the run and the word"
        );
    }

    #[test]
    fn every_editor_action_is_a_no_op_outside_the_palette() {
        let edits: [fn(&mut AppCore) -> KeyResult; 8] = [
            |c| c.on_command_char('x'),
            |c| c.on_command_backspace(),
            |c| c.on_command_left(),
            |c| c.on_command_right(),
            |c| c.on_command_home(),
            |c| c.on_command_end(),
            |c| c.on_command_clear_line(),
            |c| c.on_command_delete_word_back(),
        ];
        for edit in edits {
            let mut core = fixture_core();
            assert_eq!(edit(&mut core), KeyResult::Continue);
            assert!(matches!(core.mode, Mode::Normal), "mode must not change");
        }
    }

    #[test]
    fn submitting_an_empty_buffer_leaves_the_palette_without_recording_history() {
        let mut core = editing("   ", 3);
        assert_eq!(core.on_command_submit(), KeyResult::Continue);
        assert!(matches!(core.mode, Mode::Normal), "the palette closes");
        assert!(
            core.command_history.is_empty(),
            "blank input is not history"
        );
    }

    #[test]
    fn submitting_outside_the_palette_does_nothing_at_all() {
        let mut core = fixture_core();
        assert_eq!(core.on_command_submit(), KeyResult::Continue);
        assert!(core.command_history.is_empty());
    }

    #[test]
    fn submit_records_what_was_typed_and_closes_the_palette() {
        let mut core = editing("quit", 4);
        assert_eq!(core.on_command_submit(), KeyResult::Quit);
        assert!(matches!(core.mode, Mode::Normal));
        assert_eq!(
            core.command_history
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["quit"]
        );
    }

    /// History is for recall, so the same command twice in a row is one entry —
    /// otherwise holding Enter fills the ring with copies.
    #[test]
    fn an_immediate_repeat_is_not_recorded_twice() {
        let mut core = editing("quit", 4);
        core.on_command_submit();
        core.mode = Mode::Command(CommandState {
            buffer: "quit".to_string(),
            cursor: 4,
            selected: 0,
            history_pos: None,
        });
        core.on_command_submit();
        assert_eq!(core.command_history.len(), 1, "{:?}", core.command_history);
    }

    #[test]
    fn history_is_capped_and_drops_the_oldest_entry_first() {
        let mut core = fixture_core();
        for i in 0..COMMAND_HISTORY_CAP + 5 {
            core.mode = Mode::Command(CommandState {
                buffer: format!("cmd{i}"),
                cursor: 0,
                selected: 0,
                history_pos: None,
            });
            core.on_command_submit();
        }
        assert_eq!(core.command_history.len(), COMMAND_HISTORY_CAP);
        assert_eq!(
            core.command_history.front().map(String::as_str),
            Some("cmd5")
        );
        assert_eq!(
            core.command_history.back().map(String::as_str),
            Some(format!("cmd{}", COMMAND_HISTORY_CAP + 4).as_str())
        );
    }
}
