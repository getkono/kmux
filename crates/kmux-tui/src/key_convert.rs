use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use kmux_client::key::{Key, Modifiers, NamedKey};
use kmux_protocol::messages::{
    KeyAction as ProtoAction, KeyCode as ProtoKey, KeyEvent as ProtoKeyEvent, KeyMods as ProtoMods,
};

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

/// Convert a crossterm `KeyEvent` into the structured wire-protocol form
/// for `ClientMessage::PtyKey` / `PtyKeyBatch`.
///
/// Returns `None` for events that should be dropped (`KeyEventKind::Release`
/// — kmux does not enable `REPORT_EVENT_TYPES` so releases are noise).
///
/// Letters and digits are mapped to their physical `ProtoKey` variants so
/// the kitty keyboard protocol can report alternates correctly.  Other
/// printables (punctuation that isn't on a dedicated US-keyboard physical
/// key, layout-dependent symbols) are sent as `ProtoKey::Unidentified` plus
/// the text — Ghostty's encoder falls through to writing the utf8 directly.
pub fn convert_to_protocol_key(event: &KeyEvent) -> Option<ProtoKeyEvent> {
    let action = match event.kind {
        KeyEventKind::Press => ProtoAction::Press,
        KeyEventKind::Repeat => ProtoAction::Repeat,
        KeyEventKind::Release => return None,
    };

    let mut mods = ProtoMods::empty();
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        mods |= ProtoMods::SHIFT;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        mods |= ProtoMods::CTRL;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        mods |= ProtoMods::ALT;
    }
    if event.modifiers.contains(KeyModifiers::SUPER) {
        mods |= ProtoMods::SUPER;
    }

    let (code, text, unshifted_codepoint) = match event.code {
        KeyCode::Char(c) => char_to_proto_key(c),
        KeyCode::Enter => (ProtoKey::Enter, String::new(), 0),
        KeyCode::Tab => (ProtoKey::Tab, String::new(), 0),
        KeyCode::Backspace => (ProtoKey::Backspace, String::new(), 0),
        KeyCode::Esc => (ProtoKey::Escape, String::new(), 0),
        KeyCode::Left => (ProtoKey::ArrowLeft, String::new(), 0),
        KeyCode::Right => (ProtoKey::ArrowRight, String::new(), 0),
        KeyCode::Up => (ProtoKey::ArrowUp, String::new(), 0),
        KeyCode::Down => (ProtoKey::ArrowDown, String::new(), 0),
        KeyCode::PageUp => (ProtoKey::PageUp, String::new(), 0),
        KeyCode::PageDown => (ProtoKey::PageDown, String::new(), 0),
        KeyCode::Home => (ProtoKey::Home, String::new(), 0),
        KeyCode::End => (ProtoKey::End, String::new(), 0),
        KeyCode::Delete => (ProtoKey::Delete, String::new(), 0),
        KeyCode::Insert => (ProtoKey::Insert, String::new(), 0),
        KeyCode::F(1) => (ProtoKey::F1, String::new(), 0),
        KeyCode::F(2) => (ProtoKey::F2, String::new(), 0),
        KeyCode::F(3) => (ProtoKey::F3, String::new(), 0),
        KeyCode::F(4) => (ProtoKey::F4, String::new(), 0),
        KeyCode::F(5) => (ProtoKey::F5, String::new(), 0),
        KeyCode::F(6) => (ProtoKey::F6, String::new(), 0),
        KeyCode::F(7) => (ProtoKey::F7, String::new(), 0),
        KeyCode::F(8) => (ProtoKey::F8, String::new(), 0),
        KeyCode::F(9) => (ProtoKey::F9, String::new(), 0),
        KeyCode::F(10) => (ProtoKey::F10, String::new(), 0),
        KeyCode::F(11) => (ProtoKey::F11, String::new(), 0),
        KeyCode::F(12) => (ProtoKey::F12, String::new(), 0),
        // F13–F24 / Modifier(_) / Media(_) / Pause / etc.: drop. These are
        // not commonly bound and would otherwise need their own KeyCode
        // variants in the protocol.
        _ => return None,
    };

    Some(ProtoKeyEvent {
        code,
        mods,
        action,
        text,
        unshifted_codepoint,
    })
}

/// Map a typed character to a (physical-key, text, unshifted-codepoint)
/// triple.  Letters and digits get their dedicated physical keys so kitty
/// kbd encoding includes the right ordinal; everything else falls back to
/// `Unidentified` with the text and lets the encoder handle it.
fn char_to_proto_key(c: char) -> (ProtoKey, String, u32) {
    let text = c.to_string();
    let lower = c.to_ascii_lowercase();
    let key = match lower {
        'a' => ProtoKey::A,
        'b' => ProtoKey::B,
        'c' => ProtoKey::C,
        'd' => ProtoKey::D,
        'e' => ProtoKey::E,
        'f' => ProtoKey::F,
        'g' => ProtoKey::G,
        'h' => ProtoKey::H,
        'i' => ProtoKey::I,
        'j' => ProtoKey::J,
        'k' => ProtoKey::K,
        'l' => ProtoKey::L,
        'm' => ProtoKey::M,
        'n' => ProtoKey::N,
        'o' => ProtoKey::O,
        'p' => ProtoKey::P,
        'q' => ProtoKey::Q,
        'r' => ProtoKey::R,
        's' => ProtoKey::S,
        't' => ProtoKey::T,
        'u' => ProtoKey::U,
        'v' => ProtoKey::V,
        'w' => ProtoKey::W,
        'x' => ProtoKey::X,
        'y' => ProtoKey::Y,
        'z' => ProtoKey::Z,
        '0' => ProtoKey::Digit0,
        '1' => ProtoKey::Digit1,
        '2' => ProtoKey::Digit2,
        '3' => ProtoKey::Digit3,
        '4' => ProtoKey::Digit4,
        '5' => ProtoKey::Digit5,
        '6' => ProtoKey::Digit6,
        '7' => ProtoKey::Digit7,
        '8' => ProtoKey::Digit8,
        '9' => ProtoKey::Digit9,
        ' ' => ProtoKey::Space,
        _ => ProtoKey::Unidentified,
    };
    let unshifted = if lower.is_ascii() { lower as u32 } else { 0 };
    (key, text, unshifted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, mods, kind)
    }

    #[test]
    fn shift_enter_preserved_with_shift_modifier() {
        let p = convert_to_protocol_key(&ev(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
            KeyEventKind::Press,
        ))
        .unwrap();
        assert_eq!(p.code, ProtoKey::Enter);
        assert!(p.mods.contains(ProtoMods::SHIFT));
        assert_eq!(p.action, ProtoAction::Press);
    }

    #[test]
    fn shift_tab_preserved_with_shift_modifier() {
        let p =
            convert_to_protocol_key(&ev(KeyCode::Tab, KeyModifiers::SHIFT, KeyEventKind::Press))
                .unwrap();
        assert_eq!(p.code, ProtoKey::Tab);
        assert!(p.mods.contains(ProtoMods::SHIFT));
    }

    #[test]
    fn release_events_dropped() {
        assert!(
            convert_to_protocol_key(&ev(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Release
            ))
            .is_none(),
            "Release events must be dropped — kmux does not enable REPORT_EVENT_TYPES"
        );
    }

    #[test]
    fn lowercase_letter_maps_to_physical_key() {
        let p = convert_to_protocol_key(&ev(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ))
        .unwrap();
        assert_eq!(p.code, ProtoKey::A);
        assert_eq!(p.text, "a");
        assert_eq!(p.unshifted_codepoint, 'a' as u32);
    }

    #[test]
    fn uppercase_letter_maps_to_physical_key_with_shift_text() {
        let p = convert_to_protocol_key(&ev(
            KeyCode::Char('A'),
            KeyModifiers::SHIFT,
            KeyEventKind::Press,
        ))
        .unwrap();
        assert_eq!(p.code, ProtoKey::A);
        assert_eq!(p.text, "A");
        assert!(p.mods.contains(ProtoMods::SHIFT));
        assert_eq!(p.unshifted_codepoint, 'a' as u32);
    }

    #[test]
    fn digit_maps_to_physical_key() {
        let p = convert_to_protocol_key(&ev(
            KeyCode::Char('5'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ))
        .unwrap();
        assert_eq!(p.code, ProtoKey::Digit5);
    }

    #[test]
    fn punctuation_falls_back_to_unidentified_with_text() {
        let p = convert_to_protocol_key(&ev(
            KeyCode::Char('!'),
            KeyModifiers::SHIFT,
            KeyEventKind::Press,
        ))
        .unwrap();
        assert_eq!(p.code, ProtoKey::Unidentified);
        assert_eq!(p.text, "!");
    }

    #[test]
    fn alt_enter_preserves_alt() {
        let p =
            convert_to_protocol_key(&ev(KeyCode::Enter, KeyModifiers::ALT, KeyEventKind::Press))
                .unwrap();
        assert_eq!(p.code, ProtoKey::Enter);
        assert!(p.mods.contains(ProtoMods::ALT));
    }

    #[test]
    fn arrow_keys_map_correctly() {
        let cases = &[
            (KeyCode::Up, ProtoKey::ArrowUp),
            (KeyCode::Down, ProtoKey::ArrowDown),
            (KeyCode::Left, ProtoKey::ArrowLeft),
            (KeyCode::Right, ProtoKey::ArrowRight),
        ];
        for (cc, expected) in cases {
            let p =
                convert_to_protocol_key(&ev(*cc, KeyModifiers::NONE, KeyEventKind::Press)).unwrap();
            assert_eq!(p.code, *expected);
        }
    }
}
