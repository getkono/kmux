//! Wire types for structured key events.
//!
//! `ClientMessage::PtyKey` (and `PtyKeyBatch`) carry these instead of raw
//! bytes so the daemon can encode each keystroke using the live state of
//! its in-pane Ghostty terminal (DECCKM, kitty kbd flags, modifyOtherKeys,
//! etc.).  Encoding centrally eliminates the need for the client to know
//! what protocol the inner program negotiated.
//!
//! The `KeyCode` ordinal mirrors `kmux_ghostty::Key`, which in turn mirrors
//! the kmux-stable `KmuxKey` Zig enum at
//! `crates/kmux-ghostty-sys/zig/src/wrapper.zig`.  This module deliberately
//! does NOT depend on `kmux-ghostty` (cycle-prone) — the mapping is enforced
//! by a round-trip test on the daemon side.

use serde::{Deserialize, Serialize};

/// Stable key ordinal carried over the wire.  Variants and their numeric
/// values must match `kmux_ghostty::Key` (which mirrors `KmuxKey` in the
/// Zig wrapper).
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(non_camel_case_types)]
pub enum KeyCode {
    Unidentified = 0,
    A = 1,
    B = 2,
    C = 3,
    D = 4,
    E = 5,
    F = 6,
    G = 7,
    H = 8,
    I = 9,
    J = 10,
    K = 11,
    L = 12,
    M = 13,
    N = 14,
    O = 15,
    P = 16,
    Q = 17,
    R = 18,
    S = 19,
    T = 20,
    U = 21,
    V = 22,
    W = 23,
    X = 24,
    Y = 25,
    Z = 26,
    Digit0 = 27,
    Digit1 = 28,
    Digit2 = 29,
    Digit3 = 30,
    Digit4 = 31,
    Digit5 = 32,
    Digit6 = 33,
    Digit7 = 34,
    Digit8 = 35,
    Digit9 = 36,
    Backquote = 37,
    Backslash = 38,
    BracketLeft = 39,
    BracketRight = 40,
    Comma = 41,
    Equal = 42,
    Minus = 43,
    Period = 44,
    Quote = 45,
    Semicolon = 46,
    Slash = 47,
    Enter = 48,
    Tab = 49,
    Space = 50,
    Backspace = 51,
    Escape = 52,
    Insert = 53,
    Delete = 54,
    Home = 55,
    End = 56,
    PageUp = 57,
    PageDown = 58,
    ArrowUp = 59,
    ArrowDown = 60,
    ArrowLeft = 61,
    ArrowRight = 62,
    F1 = 63,
    F2 = 64,
    F3 = 65,
    F4 = 66,
    F5 = 67,
    F6 = 68,
    F7 = 69,
    F8 = 70,
    F9 = 71,
    F10 = 72,
    F11 = 73,
    F12 = 74,
    ShiftLeft = 75,
    ShiftRight = 76,
    ControlLeft = 77,
    ControlRight = 78,
    AltLeft = 79,
    AltRight = 80,
    MetaLeft = 81,
    MetaRight = 82,
    CapsLock = 83,
}

bitflags::bitflags! {
    /// Modifier bitmask. Bit layout matches `gvt.input.KeyMods` low byte and
    /// `kmux_ghostty::KeyMods`, so the daemon can pass the value through
    /// unchanged.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    pub struct KeyMods: u16 {
        const SHIFT = 1 << 0;
        const CTRL  = 1 << 1;
        const ALT   = 1 << 2;
        const SUPER = 1 << 3;
    }
}

/// Press / Repeat. Release events are not currently forwarded by kmux — the
/// clients do not request key-release reporting from their input toolkit.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyAction {
    Press = 1,
    Repeat = 2,
}

/// A single structured key event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEvent {
    pub code: KeyCode,
    #[serde(default)]
    pub mods: KeyMods,
    pub action: KeyAction,
    /// Layout-dependent text the keystroke would produce when typed in a
    /// plain text field. Empty for unmapped named keys.
    #[serde(default)]
    pub text: String,
    /// Codepoint when the key is pressed without shift, used by the kitty
    /// "report alternates" flag. 0 = unknown.
    #[serde(default)]
    pub unshifted_codepoint: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_code_ordinals_pin_canonical_values() {
        // Same pin as kmux_ghostty::Key — must stay in lockstep.
        assert_eq!(KeyCode::Unidentified as u16, 0);
        assert_eq!(KeyCode::A as u16, 1);
        assert_eq!(KeyCode::Z as u16, 26);
        assert_eq!(KeyCode::Digit0 as u16, 27);
        assert_eq!(KeyCode::Enter as u16, 48);
        assert_eq!(KeyCode::Tab as u16, 49);
        assert_eq!(KeyCode::Backspace as u16, 51);
        assert_eq!(KeyCode::Escape as u16, 52);
        assert_eq!(KeyCode::ArrowUp as u16, 59);
        assert_eq!(KeyCode::F1 as u16, 63);
        assert_eq!(KeyCode::CapsLock as u16, 83);
    }

    #[test]
    fn key_event_wire_roundtrip() {
        let ev = KeyEvent {
            code: KeyCode::Enter,
            mods: KeyMods::SHIFT | KeyMods::CTRL,
            action: KeyAction::Press,
            text: String::new(),
            unshifted_codepoint: 0,
        };
        let bytes = rmp_serde::to_vec_named(&ev).unwrap();
        let decoded: KeyEvent = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded.code as u16, ev.code as u16);
        assert_eq!(decoded.mods, ev.mods);
        assert_eq!(decoded.action as u8, ev.action as u8);
    }

    #[test]
    fn key_mods_bits_match_gvt_layout() {
        assert_eq!(KeyMods::SHIFT.bits(), 1 << 0);
        assert_eq!(KeyMods::CTRL.bits(), 1 << 1);
        assert_eq!(KeyMods::ALT.bits(), 1 << 2);
        assert_eq!(KeyMods::SUPER.bits(), 1 << 3);
    }
}
