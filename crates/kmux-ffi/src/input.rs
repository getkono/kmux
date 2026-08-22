//! Keyboard and mouse events crossing the boundary.

use super::*;

/// Keyboard modifier state for a structured key event. Maps to [`KeyMods`].
#[derive(uniffi::Record)]
pub struct FfiKeyMods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// The Command (⌘) key on macOS; maps to `KeyMods::SUPER`.
    pub command: bool,
}

impl FfiKeyMods {
    pub(crate) fn to_proto(&self) -> KeyMods {
        let mut m = KeyMods::empty();
        m.set(KeyMods::SHIFT, self.shift);
        m.set(KeyMods::CTRL, self.ctrl);
        m.set(KeyMods::ALT, self.alt);
        m.set(KeyMods::SUPER, self.command);
        m
    }
}

/// A non-printable key the frontend forwards by name; printable keys go through
/// [`KmuxDriver::send_char`]. Mirrors the named arm of the GTK frontend's
/// `convert_to_protocol_key`. The daemon turns the resulting [`KeyEvent`] into
/// bytes under the live terminal mode (DECCKM, kitty kbd, modifyOtherKeys).
#[derive(uniffi::Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiNamedKey {
    Enter,
    Tab,
    Backspace,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
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
}

impl FfiNamedKey {
    pub(crate) fn to_code(self) -> KeyCode {
        match self {
            Self::Enter => KeyCode::Enter,
            Self::Tab => KeyCode::Tab,
            Self::Backspace => KeyCode::Backspace,
            Self::Escape => KeyCode::Escape,
            Self::ArrowUp => KeyCode::ArrowUp,
            Self::ArrowDown => KeyCode::ArrowDown,
            Self::ArrowLeft => KeyCode::ArrowLeft,
            Self::ArrowRight => KeyCode::ArrowRight,
            Self::PageUp => KeyCode::PageUp,
            Self::PageDown => KeyCode::PageDown,
            Self::Home => KeyCode::Home,
            Self::End => KeyCode::End,
            Self::Delete => KeyCode::Delete,
            Self::Insert => KeyCode::Insert,
            Self::F1 => KeyCode::F1,
            Self::F2 => KeyCode::F2,
            Self::F3 => KeyCode::F3,
            Self::F4 => KeyCode::F4,
            Self::F5 => KeyCode::F5,
            Self::F6 => KeyCode::F6,
            Self::F7 => KeyCode::F7,
            Self::F8 => KeyCode::F8,
            Self::F9 => KeyCode::F9,
            Self::F10 => KeyCode::F10,
            Self::F11 => KeyCode::F11,
            Self::F12 => KeyCode::F12,
        }
    }
}

/// Mouse button forwarded to a mouse-tracking inner program (left only is wired
/// today; middle/right are encodable for future use).
#[derive(uniffi::Enum)]
pub enum FfiMouseButton {
    Left,
    Middle,
    Right,
}

impl FfiMouseButton {
    pub(crate) fn to_client(&self) -> MouseButton {
        match self {
            Self::Left => MouseButton::Left,
            Self::Middle => MouseButton::Middle,
            Self::Right => MouseButton::Right,
        }
    }
}

/// Whether a pointer event is a button press, release, or motion (drag).
#[derive(uniffi::Enum)]
pub enum FfiMouseKind {
    Press,
    Release,
    Motion,
}

impl FfiMouseKind {
    pub(crate) fn to_client(&self) -> MouseEventKind {
        match self {
            Self::Press => MouseEventKind::Press,
            Self::Release => MouseEventKind::Release,
            Self::Motion => MouseEventKind::Motion,
        }
    }
}

/// Modifiers active during a mouse event. `shift` is the local-selection bypass
/// (never forwarded — see `SessionManager::report_mouse`).
#[derive(uniffi::Record)]
pub struct FfiMouseMods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl FfiMouseMods {
    pub(crate) fn to_client(&self) -> MouseMods {
        MouseMods {
            ctrl: self.ctrl,
            alt: self.alt,
            shift: self.shift,
        }
    }
}
