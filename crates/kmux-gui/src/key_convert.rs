use kmux_client::key::{Key, Modifiers, NamedKey};

/// Convert iced keyboard types to [`kmux_client::key`] types so that
/// [`kmux_client::input::key_to_bytes`] can be reused without duplicating
/// the PTY byte-sequence logic.
///
/// Returns `None` for keys or modifier combinations that have no PTY
/// representation (e.g., unknown named keys, or iced `Key::Unidentified`).
pub fn convert_key(
    key: &iced::keyboard::Key,
    modifiers: iced::keyboard::Modifiers,
) -> Option<(Key, Modifiers)> {
    use iced::keyboard::Key as IcedKey;
    use iced::keyboard::key::Named;

    let converted_key = match key {
        IcedKey::Character(c) => Key::Character(c.to_string()),
        IcedKey::Named(named) => {
            let nk = match named {
                Named::ArrowLeft => NamedKey::ArrowLeft,
                Named::ArrowRight => NamedKey::ArrowRight,
                Named::ArrowUp => NamedKey::ArrowUp,
                Named::ArrowDown => NamedKey::ArrowDown,
                Named::Enter => NamedKey::Enter,
                Named::Backspace => NamedKey::Backspace,
                Named::Escape => NamedKey::Escape,
                Named::Tab => NamedKey::Tab,
                Named::Space => NamedKey::Space,
                Named::PageUp => NamedKey::PageUp,
                Named::PageDown => NamedKey::PageDown,
                Named::Home => NamedKey::Home,
                Named::End => NamedKey::End,
                Named::Delete => NamedKey::Delete,
                Named::Insert => NamedKey::Insert,
                Named::F1 => NamedKey::F1,
                Named::F2 => NamedKey::F2,
                Named::F3 => NamedKey::F3,
                Named::F4 => NamedKey::F4,
                Named::F5 => NamedKey::F5,
                Named::F6 => NamedKey::F6,
                Named::F7 => NamedKey::F7,
                Named::F8 => NamedKey::F8,
                Named::F9 => NamedKey::F9,
                Named::F10 => NamedKey::F10,
                Named::F11 => NamedKey::F11,
                Named::F12 => NamedKey::F12,
                Named::Shift => NamedKey::Shift,
                Named::Control => NamedKey::Control,
                Named::Alt => NamedKey::Alt,
                Named::Super => NamedKey::Super,
                Named::Meta => NamedKey::Meta,
                _ => return None,
            };
            Key::Named(nk)
        }
        IcedKey::Unidentified => return None,
    };

    let mut mods = Modifiers::empty();
    if modifiers.control() {
        mods |= Modifiers::CTRL;
    }
    if modifiers.shift() {
        mods |= Modifiers::SHIFT;
    }
    if modifiers.alt() {
        mods |= Modifiers::ALT;
    }
    if modifiers.logo() {
        mods |= Modifiers::SUPER;
    }

    Some((converted_key, mods))
}
