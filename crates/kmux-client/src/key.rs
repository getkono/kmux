use bitflags::bitflags;

/// A framework-agnostic key representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Character(String),
    Named(NamedKey),
}

/// Named (non-character) keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Enter,
    Backspace,
    Escape,
    Tab,
    Space,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    Insert,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    // Modifier-only keys (used to detect and ignore modifier-only presses)
    Shift,
    Control,
    Alt,
    Super,
    Meta,
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Modifiers: u8 {
        const CTRL  = 0b0001;
        const SHIFT = 0b0010;
        const ALT   = 0b0100;
        const SUPER = 0b1000;
    }
}
