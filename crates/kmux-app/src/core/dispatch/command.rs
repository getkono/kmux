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
