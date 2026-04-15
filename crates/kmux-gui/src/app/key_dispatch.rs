use iced::{Event, Task};
use tracing::debug;

use crate::shortcut::{self, LeaderState};

use super::{Message, kmuxApp};

impl kmuxApp {
    /// Handle a raw key event with leader key interception.
    pub(super) fn handle_key_event(
        &mut self,
        key: iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
        text_val: Option<String>,
    ) -> Task<Message> {
        use iced::keyboard::Key;
        use iced::keyboard::key::Named;

        match &self.leader_state {
            LeaderState::Idle => {
                // Shift+PageUp / Shift+PageDown scroll by one page.
                if modifiers.shift() && key == Key::Named(Named::PageUp) {
                    if let Some(grid) = self.mgr.active_grid_mut() {
                        grid.scroll_up(grid.rows);
                    }
                    return Task::none();
                }
                if modifiers.shift() && key == Key::Named(Named::PageDown) {
                    if let Some(grid) = self.mgr.active_grid_mut() {
                        grid.scroll_down(grid.rows);
                    }
                    return Task::none();
                }

                // Ctrl+Shift+H or F12 toggles HUD
                if key == Key::Named(Named::F12)
                    || (matches!(&key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("h"))
                        && modifiers.shift()
                        && modifiers.control())
                {
                    self.hud_visible = !self.hud_visible;
                    return Task::none();
                }

                // Ctrl+B → enter leader mode
                if shortcut::is_leader_key(&key, modifiers) {
                    self.leader_state = LeaderState::AwaitingAction {
                        entered_at: std::time::Instant::now(),
                    };
                    return Task::none();
                }

                // Cmd+C (macOS) or Ctrl+Shift+C (Linux/Windows) → copy selection
                if matches!(&key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("c")) {
                    let is_copy = if cfg!(target_os = "macos") {
                        modifiers.logo()
                    } else {
                        modifiers.control() && modifiers.shift()
                    };
                    if is_copy {
                        if let Some(grid) = self.mgr.active_grid()
                            && let Some(text) = grid.selected_text()
                        {
                            return iced::clipboard::write(text);
                        }
                        return Task::none();
                    }
                }

                // Ctrl+Shift+V (Linux/Windows) or Cmd+V (macOS) → clipboard paste
                if matches!(&key, Key::Character(c) if c.as_str().eq_ignore_ascii_case("v")) {
                    let is_paste = if cfg!(target_os = "macos") {
                        modifiers.logo()
                    } else {
                        modifiers.control() && modifiers.shift()
                    };
                    if is_paste {
                        return iced::clipboard::read().map(Message::ClipboardPaste);
                    }
                }

                // Escape clears selection (and still forwards to PTY below)
                if key == Key::Named(Named::Escape)
                    && let Some(grid) = self.mgr.active_grid_mut()
                {
                    grid.clear_selection();
                }

                // Normal key → forward to PTY
                self.forward_key_to_pty(key, modifiers, text_val)
            }

            LeaderState::AwaitingAction { .. } => {
                // Ignore modifier-only key presses (Shift, Ctrl, Alt, Super)
                // so that shifted characters like '?' (Shift+/) work.
                if matches!(
                    key,
                    Key::Named(
                        Named::Shift | Named::Control | Named::Alt | Named::Super | Named::Meta
                    )
                ) {
                    return Task::none();
                }

                if let Some(action) = shortcut::resolve_key(&key, modifiers) {
                    self.dispatch_action(action)
                } else {
                    // Unrecognized key in leader mode → cancel and discard
                    self.leader_state = LeaderState::Idle;
                    Task::none()
                }
            }

            LeaderState::RenameEditing { .. } => {
                // Escape cancels rename
                if key == Key::Named(Named::Escape) {
                    self.leader_state = LeaderState::Idle;
                    return Task::none();
                }
                // Enter and text input handled by iced text_input widget messages
                Task::none()
            }

            LeaderState::ConfirmClose { .. } => {
                let session = if let LeaderState::ConfirmClose { session } = &self.leader_state {
                    session.clone()
                } else {
                    unreachable!()
                };

                match &key {
                    Key::Character(c) if c.as_str() == "y" => {
                        self.leader_state = LeaderState::Idle;
                        self.mgr.close_session(&session);
                        Task::none()
                    }
                    _ => {
                        // Any other key (including 'n') cancels
                        self.leader_state = LeaderState::Idle;
                        Task::none()
                    }
                }
            }

            LeaderState::SignalMenu { .. } => {
                let session = if let LeaderState::SignalMenu { session } = &self.leader_state {
                    session.clone()
                } else {
                    unreachable!()
                };

                if key == Key::Named(Named::Escape) {
                    self.leader_state = LeaderState::Idle;
                    return Task::none();
                }

                if let Some(signal) = shortcut::resolve_signal_key(&key) {
                    self.leader_state = LeaderState::Idle;
                    self.mgr.send_signal(&session, signal);
                } else {
                    self.leader_state = LeaderState::Idle;
                }
                Task::none()
            }

            LeaderState::HelpVisible => {
                // Any key closes help
                self.leader_state = LeaderState::Idle;
                Task::none()
            }

            LeaderState::CommandPalette { .. } => {
                // Escape closes
                if key == Key::Named(Named::Escape) {
                    self.leader_state = LeaderState::Idle;
                    return Task::none();
                }
                // Arrow keys navigate
                if key == Key::Named(Named::ArrowUp) {
                    return self.update(Message::CommandPaletteNavigate(-1));
                }
                if key == Key::Named(Named::ArrowDown) {
                    return self.update(Message::CommandPaletteNavigate(1));
                }
                // Enter selects
                if key == Key::Named(Named::Enter) {
                    return self.update(Message::CommandPaletteSelect);
                }
                // Other keys are handled by the text_input widget
                Task::none()
            }
        }
    }

    /// Forward a key to the PTY as bytes.
    pub(super) fn forward_key_to_pty(
        &mut self,
        key: iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
        text_val: Option<String>,
    ) -> Task<Message> {
        // Snap to bottom on any keypress while scrolled.
        if let Some(grid) = self.mgr.active_grid_mut() {
            grid.scroll_to_bottom();
        }

        let app_cursor = self
            .mgr
            .active_grid()
            .map(|b| b.app_cursor())
            .unwrap_or(false);
        let bytes = crate::key_convert::convert_key(&key, modifiers).and_then(|(k, mods)| {
            kmux_client::input::key_to_bytes(&k, mods, text_val.as_deref(), app_cursor)
        });
        if let Some(bytes) = bytes {
            if self.mgr.active_session().is_some() {
                if !self.mgr.send_input(bytes) {
                    // input_locked; status_msg already updated by mgr
                }
            } else {
                self.mgr.set_status_msg(
                    "No active session -- press Ctrl+B then c to create one".to_string(),
                );
            }
        }
        Task::none()
    }
}

pub(super) fn keyboard_filter(
    event: Event,
    status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    match event {
        Event::Keyboard(ref kbd_event) => {
            debug!(
                ?kbd_event,
                ?status,
                "keyboard_filter: received keyboard event"
            );
            match kbd_event {
                iced::keyboard::Event::KeyPressed {
                    key,
                    modifiers,
                    text,
                    ..
                } => {
                    let text_owned = text.as_ref().map(|t| t.to_string());
                    Some(Message::RawKeyEvent {
                        key: key.clone(),
                        modifiers: *modifiers,
                        text: text_owned,
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}
