//! GTK/GDK → toolkit-agnostic conversions. The GTK analog of the TUI's
//! `key_convert` (crossterm → key) and `theme::rgb` (Rgb → ratatui Color).

use gtk4::gdk;
use kmux_client::key::{Key, Modifiers, NamedKey};

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

/// Best-effort byte encoding for a forwarded key (scaffold-grade: the TUI uses
/// the Ghostty-mode-aware encoder; here we send the obvious bytes so typing and
/// the common control keys reach the PTY).
pub fn forward_bytes(key: &Key, mods: Modifiers) -> Vec<u8> {
    match key {
        Key::Character(s) => {
            // Ctrl+<letter> → control byte (e.g. Ctrl+C → 0x03).
            if mods.contains(Modifiers::CTRL)
                && let Some(c) = s.chars().next()
                && c.is_ascii_alphabetic()
            {
                return vec![(c.to_ascii_uppercase() as u8) & 0x1f];
            }
            s.as_bytes().to_vec()
        }
        Key::Named(n) => match n {
            NamedKey::Enter => vec![b'\r'],
            NamedKey::Backspace => vec![0x7f],
            NamedKey::Tab => vec![b'\t'],
            NamedKey::Escape => vec![0x1b],
            NamedKey::Space => vec![b' '],
            NamedKey::ArrowUp => b"\x1b[A".to_vec(),
            NamedKey::ArrowDown => b"\x1b[B".to_vec(),
            NamedKey::ArrowRight => b"\x1b[C".to_vec(),
            NamedKey::ArrowLeft => b"\x1b[D".to_vec(),
            NamedKey::Home => b"\x1b[H".to_vec(),
            NamedKey::End => b"\x1b[F".to_vec(),
            NamedKey::Delete => b"\x1b[3~".to_vec(),
            _ => Vec::new(),
        },
    }
}
