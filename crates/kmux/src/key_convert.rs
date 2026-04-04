use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use kmux_client::key::{Key, Modifiers, NamedKey};

/// Convert a crossterm KeyEvent into our framework-agnostic Key + Modifiers.
pub fn convert(event: &KeyEvent) -> (Key, Modifiers) {
    let key = match event.code {
        KeyCode::Char(c) => Key::Character(c.to_string()),
        KeyCode::Enter => Key::Named(NamedKey::Enter),
        KeyCode::Backspace => Key::Named(NamedKey::Backspace),
        KeyCode::Esc => Key::Named(NamedKey::Escape),
        KeyCode::Tab => Key::Named(NamedKey::Tab),
        KeyCode::Left => Key::Named(NamedKey::ArrowLeft),
        KeyCode::Right => Key::Named(NamedKey::ArrowRight),
        KeyCode::Up => Key::Named(NamedKey::ArrowUp),
        KeyCode::Down => Key::Named(NamedKey::ArrowDown),
        KeyCode::PageUp => Key::Named(NamedKey::PageUp),
        KeyCode::PageDown => Key::Named(NamedKey::PageDown),
        KeyCode::Home => Key::Named(NamedKey::Home),
        KeyCode::End => Key::Named(NamedKey::End),
        KeyCode::Delete => Key::Named(NamedKey::Delete),
        KeyCode::Insert => Key::Named(NamedKey::Insert),
        KeyCode::F(1) => Key::Named(NamedKey::F1),
        KeyCode::F(2) => Key::Named(NamedKey::F2),
        KeyCode::F(3) => Key::Named(NamedKey::F3),
        KeyCode::F(4) => Key::Named(NamedKey::F4),
        KeyCode::F(5) => Key::Named(NamedKey::F5),
        KeyCode::F(6) => Key::Named(NamedKey::F6),
        KeyCode::F(7) => Key::Named(NamedKey::F7),
        KeyCode::F(8) => Key::Named(NamedKey::F8),
        KeyCode::F(9) => Key::Named(NamedKey::F9),
        KeyCode::F(10) => Key::Named(NamedKey::F10),
        KeyCode::F(11) => Key::Named(NamedKey::F11),
        KeyCode::F(12) => Key::Named(NamedKey::F12),
        _ => Key::Character(String::new()),
    };

    let mut mods = Modifiers::empty();
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        mods |= Modifiers::CTRL;
    }
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        mods |= Modifiers::SHIFT;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        mods |= Modifiers::ALT;
    }
    if event.modifiers.contains(KeyModifiers::SUPER) {
        mods |= Modifiers::SUPER;
    }

    (key, mods)
}

/// Extract text content from a crossterm KeyEvent (for PTY forwarding).
pub fn text_from_event(event: &KeyEvent) -> Option<String> {
    match event.code {
        KeyCode::Char(c) => Some(c.to_string()),
        _ => None,
    }
}
