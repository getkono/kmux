//! Mouse-event encoding and shared key helpers for the kmux client.
//!
//! Key encoding moved server-side in PROTOCOL_VERSION 18: the daemon owns a
//! per-pane Ghostty key encoder and decides the byte sequence for each
//! keystroke based on what the inner program negotiated (DECCKM, kitty kbd
//! flags, modifyOtherKeys).  The client now sends structured key events via
//! `ClientMessage::PtyKeyBatch`.  Each frontend converts its toolkit event to a
//! `ProtoKeyEvent`; the character→physical-key half of that conversion is
//! toolkit-agnostic and lives here as [`char_to_proto_key`] so the TUI and GUI
//! share one copy.

use kmux_protocol::messages::KeyCode as ProtoKey;

/// Map a signal menu key character to a Unix signal number.
///
/// Returns `None` for unrecognised keys.
pub fn signal_from_key(key: &str) -> Option<i32> {
    match key {
        "k" => Some(9),  // SIGKILL
        "t" => Some(15), // SIGTERM
        "s" => Some(19), // SIGSTOP
        "c" => Some(18), // SIGCONT
        _ => None,
    }
}

/// Encode mouse scroll events as terminal escape sequences.
///
/// `col` and `row` are 1-based terminal coordinates.
/// `lines` > 0 means scroll up, < 0 means scroll down.
/// Each line generates one escape sequence (matching xterm behavior).
pub fn encode_mouse_scroll(col: u16, row: u16, lines: i32, sgr: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let count = lines.unsigned_abs() as usize;
    // Button 64 = scroll up, 65 = scroll down (xterm convention).
    let button: u8 = if lines > 0 { 64 } else { 65 };

    for _ in 0..count.min(255) {
        if sgr {
            // SGR format: \x1b[<{button};{col};{row}M
            let seq = format!("\x1b[<{};{};{}M", button, col, row);
            out.extend_from_slice(seq.as_bytes());
        } else {
            // Legacy X10/normal format: \x1b[M{cb}{cx}{cy}
            // cb = button + 32, cx = col + 32, cy = row + 32
            let cb = button + 32;
            let cx = (col as u8).saturating_add(32);
            let cy = (row as u8).saturating_add(32);
            out.extend_from_slice(&[0x1b, b'[', b'M', cb, cx, cy]);
        }
    }
    out
}

/// A mouse button reportable to the inner program's mouse-tracking modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseButton {
    /// The low button bits of the xterm `cb` byte (left=0, middle=1, right=2).
    fn code(self) -> u8 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        }
    }
}

/// Whether a pointer event is a button press, a release, or motion (drag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    Press,
    Release,
    Motion,
}

/// Keyboard modifiers active during a mouse event, packed into the `cb` byte.
///
/// `shift` is carried here so the encoder and the decision policy share one
/// event type, but it is the terminal's *bypass* key: `report_mouse` never
/// forwards a shift-held event (it falls through to local selection instead).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseMods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// A pointer event to forward to the inner program's mouse-tracking modes.
///
/// `col`/`row` are 1-based *visible viewport* cells, like [`encode_mouse_scroll`]
/// — the inner program only knows its on-screen grid, never the scrollback.
#[derive(Debug, Clone, Copy)]
pub struct MouseEvent {
    pub button: MouseButton,
    pub kind: MouseEventKind,
    pub col: u16,
    pub row: u16,
    pub mods: MouseMods,
}

/// Encode a mouse button/motion event as a terminal mouse-tracking sequence.
///
/// Mirrors [`encode_mouse_scroll`] (same 1-based coordinates and `+32` legacy
/// offsets), but for the buttons. The `cb` byte packs, from the low bits: the
/// button (left=0, middle=1, right=2), `+4` shift, `+8` alt/meta, `+16` ctrl,
/// and `+32` for a motion (drag) event.
///
/// With `sgr` (DEC mode 1006) the form is `\x1b[<{cb};{col};{row}{M|m}` — final
/// `M` for press/motion, `m` for release, with the real button preserved.
/// Without it the legacy X10 form `\x1b[M{cb+32}{col+32}{row+32}` is used;
/// legacy can't say *which* button was released, so a release reports button 3.
/// Legacy coordinates saturate at 223 (the 255−32 ceiling of a single byte).
pub fn encode_mouse_button(ev: &MouseEvent, sgr: bool) -> Vec<u8> {
    let mut mods: u8 = 0;
    if ev.mods.shift {
        mods += 4;
    }
    if ev.mods.alt {
        mods += 8;
    }
    if ev.mods.ctrl {
        mods += 16;
    }
    let motion: u8 = if ev.kind == MouseEventKind::Motion {
        32
    } else {
        0
    };

    if sgr {
        // SGR keeps the real button number; the final byte distinguishes
        // press/motion (`M`) from release (`m`).
        let cb = ev.button.code() + mods + motion;
        let final_byte = if ev.kind == MouseEventKind::Release {
            'm'
        } else {
            'M'
        };
        format!("\x1b[<{};{};{}{}", cb, ev.col, ev.row, final_byte).into_bytes()
    } else {
        // Legacy collapses every release to button 3 (it has no per-button
        // release); press/motion carry the real button plus the motion bit.
        let button = if ev.kind == MouseEventKind::Release {
            3
        } else {
            ev.button.code() + motion
        };
        let cb = (button + mods).saturating_add(32);
        let cx = (ev.col.min(223) as u8).saturating_add(32);
        let cy = (ev.row.min(223) as u8).saturating_add(32);
        vec![0x1b, b'[', b'M', cb, cx, cy]
    }
}

/// Map a typed character to a `(physical-key, text, unshifted-codepoint)` triple
/// for `ClientMessage::PtyKey` / `PtyKeyBatch`.
///
/// Letters and digits get their dedicated physical [`ProtoKey`] so the daemon's
/// kitty-keyboard encoder reports the right ordinal; everything else (punctuation
/// that isn't on a dedicated US-keyboard physical key, layout-dependent symbols)
/// falls back to [`ProtoKey::Unidentified`] plus the text, letting the encoder
/// write the utf-8 directly.
///
/// Toolkit-agnostic: each frontend maps its own *named* keys (Enter, arrows, …),
/// but shares this character mapping so there is one source of truth.
pub fn char_to_proto_key(c: char) -> (ProtoKey, String, u32) {
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

    #[test]
    fn signal_k_is_sigkill() {
        assert_eq!(signal_from_key("k"), Some(9));
    }

    #[test]
    fn signal_unknown_is_none() {
        assert_eq!(signal_from_key("z"), None);
    }

    #[test]
    fn sgr_scroll_up() {
        let bytes = encode_mouse_scroll(10, 5, 1, true);
        assert_eq!(bytes, b"\x1b[<64;10;5M");
    }

    #[test]
    fn sgr_scroll_down() {
        let bytes = encode_mouse_scroll(10, 5, -1, true);
        assert_eq!(bytes, b"\x1b[<65;10;5M");
    }

    #[test]
    fn legacy_scroll_up() {
        let bytes = encode_mouse_scroll(10, 5, 1, false);
        assert_eq!(bytes, &[0x1b, b'[', b'M', 96, 42, 37]);
    }

    #[test]
    fn legacy_scroll_down() {
        let bytes = encode_mouse_scroll(10, 5, -1, false);
        assert_eq!(bytes, &[0x1b, b'[', b'M', 97, 42, 37]);
    }

    #[test]
    fn multiple_lines_generate_multiple_sequences() {
        let bytes = encode_mouse_scroll(1, 1, 3, true);
        assert_eq!(bytes, b"\x1b[<64;1;1M\x1b[<64;1;1M\x1b[<64;1;1M");
    }

    #[test]
    fn zero_lines_produces_empty() {
        let bytes = encode_mouse_scroll(1, 1, 0, true);
        assert!(bytes.is_empty());
    }

    fn ev(button: MouseButton, kind: MouseEventKind, mods: MouseMods) -> MouseEvent {
        MouseEvent {
            button,
            kind,
            col: 10,
            row: 5,
            mods,
        }
    }

    #[test]
    fn sgr_button_press_left() {
        let bytes = encode_mouse_button(
            &ev(
                MouseButton::Left,
                MouseEventKind::Press,
                MouseMods::default(),
            ),
            true,
        );
        assert_eq!(bytes, b"\x1b[<0;10;5M");
    }

    #[test]
    fn sgr_button_release_keeps_button_uses_lowercase_m() {
        let bytes = encode_mouse_button(
            &ev(
                MouseButton::Left,
                MouseEventKind::Release,
                MouseMods::default(),
            ),
            true,
        );
        assert_eq!(bytes, b"\x1b[<0;10;5m");
    }

    #[test]
    fn sgr_motion_sets_the_32_bit() {
        let bytes = encode_mouse_button(
            &ev(
                MouseButton::Left,
                MouseEventKind::Motion,
                MouseMods::default(),
            ),
            true,
        );
        assert_eq!(bytes, b"\x1b[<32;10;5M");
    }

    #[test]
    fn sgr_middle_and_right_button_codes() {
        let mid = encode_mouse_button(
            &ev(
                MouseButton::Middle,
                MouseEventKind::Press,
                MouseMods::default(),
            ),
            true,
        );
        assert_eq!(mid, b"\x1b[<1;10;5M");
        let right = encode_mouse_button(
            &ev(
                MouseButton::Right,
                MouseEventKind::Press,
                MouseMods::default(),
            ),
            true,
        );
        assert_eq!(right, b"\x1b[<2;10;5M");
    }

    #[test]
    fn sgr_modifiers_add_4_8_16() {
        let mods = MouseMods {
            ctrl: true,
            alt: true,
            shift: true,
        };
        // 0 (left) + 4 (shift) + 8 (alt) + 16 (ctrl) = 28
        let bytes = encode_mouse_button(&ev(MouseButton::Left, MouseEventKind::Press, mods), true);
        assert_eq!(bytes, b"\x1b[<28;10;5M");
    }

    #[test]
    fn legacy_button_press_left() {
        let bytes = encode_mouse_button(
            &ev(
                MouseButton::Left,
                MouseEventKind::Press,
                MouseMods::default(),
            ),
            false,
        );
        // cb=0+32=32, cx=10+32=42, cy=5+32=37
        assert_eq!(bytes, &[0x1b, b'[', b'M', 32, 42, 37]);
    }

    #[test]
    fn legacy_release_reports_button_3() {
        let bytes = encode_mouse_button(
            &ev(
                MouseButton::Right,
                MouseEventKind::Release,
                MouseMods::default(),
            ),
            false,
        );
        // release collapses to button 3 regardless of which button: cb=3+32=35
        assert_eq!(bytes, &[0x1b, b'[', b'M', 35, 42, 37]);
    }

    #[test]
    fn legacy_motion_sets_the_32_bit() {
        let bytes = encode_mouse_button(
            &ev(
                MouseButton::Left,
                MouseEventKind::Motion,
                MouseMods::default(),
            ),
            false,
        );
        // button 0 + motion 32 = 32, cb=32+32=64
        assert_eq!(bytes, &[0x1b, b'[', b'M', 64, 42, 37]);
    }

    #[test]
    fn legacy_coordinates_saturate_at_223() {
        let bytes = encode_mouse_button(
            &MouseEvent {
                button: MouseButton::Left,
                kind: MouseEventKind::Press,
                col: 300,
                row: 1,
                mods: MouseMods::default(),
            },
            false,
        );
        // col clamps to 223, +32 = 255; row 1 + 32 = 33
        assert_eq!(bytes, &[0x1b, b'[', b'M', 32, 255, 33]);
    }

    #[test]
    fn lowercase_letter_maps_to_physical_key() {
        let (code, text, unshifted) = char_to_proto_key('a');
        assert_eq!(code, ProtoKey::A);
        assert_eq!(text, "a");
        assert_eq!(unshifted, 'a' as u32);
    }

    #[test]
    fn uppercase_letter_shares_physical_key_with_unshifted_codepoint() {
        let (code, text, unshifted) = char_to_proto_key('A');
        assert_eq!(code, ProtoKey::A, "physical key is layout-independent");
        assert_eq!(text, "A", "text preserves the shifted glyph");
        assert_eq!(unshifted, 'a' as u32);
    }

    #[test]
    fn digit_maps_to_physical_key() {
        assert_eq!(char_to_proto_key('5').0, ProtoKey::Digit5);
    }

    #[test]
    fn space_maps_to_physical_space() {
        assert_eq!(char_to_proto_key(' ').0, ProtoKey::Space);
    }

    #[test]
    fn punctuation_falls_back_to_unidentified_with_text() {
        let (code, text, unshifted) = char_to_proto_key('!');
        assert_eq!(code, ProtoKey::Unidentified);
        assert_eq!(text, "!");
        assert_eq!(
            unshifted, '!' as u32,
            "ascii punctuation still reports a codepoint"
        );
    }

    #[test]
    fn non_ascii_has_zero_unshifted_codepoint() {
        let (code, text, unshifted) = char_to_proto_key('é');
        assert_eq!(code, ProtoKey::Unidentified);
        assert_eq!(text, "é");
        assert_eq!(unshifted, 0);
    }
}
