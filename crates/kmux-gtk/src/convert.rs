//! GTK/GDK → toolkit-agnostic conversions. The GTK analog of the TUI's
//! `key_convert` (crossterm → key) and `theme::rgb` (Rgb → ratatui Color).

use gtk4::gdk;
use kmux_client::key::{Key, Modifiers, NamedKey};
use kmux_protocol::messages::{
    KeyAction as ProtoAction, KeyCode as ProtoKey, KeyEvent as ProtoKeyEvent, KeyMods as ProtoMods,
};

// NOTE: the toolkit render-leaf color conversion (the GTK analog of the TUI's
// `Rgb -> ratatui::Color`) is currently done inline in `main::render` via
// cairo's `set_source_rgb(f64, f64, f64)`. A `gdk::RGBA` conversion would be
// added here once widget/CSS styling (rather than raw cairo) needs it.

/// Translate a GDK key press to the agnostic [`Key`] + [`Modifiers`] that
/// `kmux_app::mode::resolve` understands. Returns `None` for modifier-only
/// presses and keys we don't map.
pub fn convert(keyval: gdk::Key, state: gdk::ModifierType) -> Option<(Key, Modifiers)> {
    let mut mods = Modifiers::empty();
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        mods |= Modifiers::CTRL;
    }
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        mods |= Modifiers::SHIFT;
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        mods |= Modifiers::ALT;
    }
    if state.contains(gdk::ModifierType::SUPER_MASK) {
        mods |= Modifiers::SUPER;
    }

    use gdk::Key as G;
    let named = match keyval {
        G::Return | G::KP_Enter => Some(NamedKey::Enter),
        G::BackSpace => Some(NamedKey::Backspace),
        G::Escape => Some(NamedKey::Escape),
        G::Tab | G::ISO_Left_Tab => Some(NamedKey::Tab),
        G::space => Some(NamedKey::Space),
        G::Left => Some(NamedKey::ArrowLeft),
        G::Right => Some(NamedKey::ArrowRight),
        G::Up => Some(NamedKey::ArrowUp),
        G::Down => Some(NamedKey::ArrowDown),
        G::Page_Up => Some(NamedKey::PageUp),
        G::Page_Down => Some(NamedKey::PageDown),
        G::Home => Some(NamedKey::Home),
        G::End => Some(NamedKey::End),
        G::Delete => Some(NamedKey::Delete),
        G::Insert => Some(NamedKey::Insert),
        G::F1 => Some(NamedKey::F1),
        G::F2 => Some(NamedKey::F2),
        G::F3 => Some(NamedKey::F3),
        G::F4 => Some(NamedKey::F4),
        G::F5 => Some(NamedKey::F5),
        G::F6 => Some(NamedKey::F6),
        G::F7 => Some(NamedKey::F7),
        G::F8 => Some(NamedKey::F8),
        G::F9 => Some(NamedKey::F9),
        G::F10 => Some(NamedKey::F10),
        G::F11 => Some(NamedKey::F11),
        G::F12 => Some(NamedKey::F12),
        _ => None,
    };
    if let Some(n) = named {
        return Some((Key::Named(n), mods));
    }

    // Printable character.
    let ch = keyval.to_unicode()?;
    if ch.is_control() {
        return None;
    }
    Some((Key::Character(ch.to_string()), mods))
}

/// Convert a GDK key press into the structured wire form for
/// `ClientMessage::PtyKeyBatch`, mirroring the TUI's `convert_to_protocol_key`.
///
/// The daemon owns a per-pane Ghostty key encoder and turns this into the right
/// bytes under the live terminal mode state (DECCKM, kitty kbd flags,
/// modifyOtherKeys), so the GUI never hand-rolls escape sequences. The
/// character→physical-key mapping is shared with the TUI via
/// [`kmux_client::input::char_to_proto_key`].
///
/// Returns `None` for keyvals we don't forward (modifier-only presses, control
/// codepoints with no named key, exotic keys without a `ProtoKey`).
pub fn convert_to_protocol_key(
    keyval: gdk::Key,
    state: gdk::ModifierType,
) -> Option<ProtoKeyEvent> {
    let mut mods = ProtoMods::empty();
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        mods |= ProtoMods::SHIFT;
    }
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        mods |= ProtoMods::CTRL;
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        mods |= ProtoMods::ALT;
    }
    if state.contains(gdk::ModifierType::SUPER_MASK) {
        mods |= ProtoMods::SUPER;
    }

    use gdk::Key as G;
    // `ISO_Left_Tab` is what X11 reports for Shift+Tab; map it to Tab and let
    // the SHIFT modifier (set above) carry the shift, matching the TUI.
    let named = match keyval {
        G::Return | G::KP_Enter => Some(ProtoKey::Enter),
        G::Tab | G::ISO_Left_Tab => Some(ProtoKey::Tab),
        G::BackSpace => Some(ProtoKey::Backspace),
        G::Escape => Some(ProtoKey::Escape),
        G::Left => Some(ProtoKey::ArrowLeft),
        G::Right => Some(ProtoKey::ArrowRight),
        G::Up => Some(ProtoKey::ArrowUp),
        G::Down => Some(ProtoKey::ArrowDown),
        G::Page_Up => Some(ProtoKey::PageUp),
        G::Page_Down => Some(ProtoKey::PageDown),
        G::Home => Some(ProtoKey::Home),
        G::End => Some(ProtoKey::End),
        G::Delete => Some(ProtoKey::Delete),
        G::Insert => Some(ProtoKey::Insert),
        G::F1 => Some(ProtoKey::F1),
        G::F2 => Some(ProtoKey::F2),
        G::F3 => Some(ProtoKey::F3),
        G::F4 => Some(ProtoKey::F4),
        G::F5 => Some(ProtoKey::F5),
        G::F6 => Some(ProtoKey::F6),
        G::F7 => Some(ProtoKey::F7),
        G::F8 => Some(ProtoKey::F8),
        G::F9 => Some(ProtoKey::F9),
        G::F10 => Some(ProtoKey::F10),
        G::F11 => Some(ProtoKey::F11),
        G::F12 => Some(ProtoKey::F12),
        _ => None,
    };
    if let Some(code) = named {
        return Some(ProtoKeyEvent {
            code,
            mods,
            action: ProtoAction::Press,
            text: String::new(),
            unshifted_codepoint: 0,
        });
    }

    // Printable character → physical key + text via the shared mapping.
    let ch = keyval.to_unicode()?;
    if ch.is_control() {
        return None;
    }
    let (code, text, unshifted_codepoint) = kmux_client::input::char_to_proto_key(ch);
    Some(ProtoKeyEvent {
        code,
        mods,
        action: ProtoAction::Press,
        text,
        unshifted_codepoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_map_to_proto_codes() {
        let cases = [
            (gdk::Key::Return, ProtoKey::Enter),
            (gdk::Key::BackSpace, ProtoKey::Backspace),
            (gdk::Key::Escape, ProtoKey::Escape),
            (gdk::Key::Tab, ProtoKey::Tab),
            (gdk::Key::Left, ProtoKey::ArrowLeft),
            (gdk::Key::Up, ProtoKey::ArrowUp),
            (gdk::Key::Page_Up, ProtoKey::PageUp),
            (gdk::Key::F5, ProtoKey::F5),
        ];
        for (keyval, expected) in cases {
            let ev = convert_to_protocol_key(keyval, gdk::ModifierType::empty())
                .expect("named key must map");
            assert_eq!(ev.code, expected);
            assert_eq!(ev.action, ProtoAction::Press);
        }
    }

    #[test]
    fn shift_tab_maps_to_tab_with_shift() {
        let ev =
            convert_to_protocol_key(gdk::Key::ISO_Left_Tab, gdk::ModifierType::SHIFT_MASK).unwrap();
        assert_eq!(ev.code, ProtoKey::Tab);
        assert!(ev.mods.contains(ProtoMods::SHIFT));
    }

    #[test]
    fn modifiers_translate_to_proto_mods() {
        let ev = convert_to_protocol_key(
            gdk::Key::Return,
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK,
        )
        .unwrap();
        assert!(ev.mods.contains(ProtoMods::CTRL));
        assert!(ev.mods.contains(ProtoMods::ALT));
        assert!(!ev.mods.contains(ProtoMods::SHIFT));
    }

    #[test]
    fn letter_uses_shared_physical_key_mapping() {
        // 'a' keyval → physical A, text "a"; the shared char mapping owns this.
        let ev = convert_to_protocol_key(gdk::Key::a, gdk::ModifierType::empty()).unwrap();
        assert_eq!(ev.code, ProtoKey::A);
        assert_eq!(ev.text, "a");
        assert_eq!(ev.unshifted_codepoint, 'a' as u32);
    }

    #[test]
    fn ctrl_letter_keeps_letter_and_sets_ctrl() {
        // Ctrl+C: keyval stays 'c'; the daemon encoder applies CTRL → 0x03.
        let ev = convert_to_protocol_key(gdk::Key::c, gdk::ModifierType::CONTROL_MASK).unwrap();
        assert_eq!(ev.code, ProtoKey::C);
        assert_eq!(ev.text, "c");
        assert!(ev.mods.contains(ProtoMods::CTRL));
    }
}
