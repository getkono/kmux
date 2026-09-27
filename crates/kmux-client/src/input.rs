//! Mouse-event encoding and shared key helpers for the kmux client.
//!
//! Key encoding lives server-side: the daemon owns a
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
            let seq = format!("\x1b[<{button};{col};{row}M");
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
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
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
/// for `ClientMessage::PtyKeyBatch`.
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
    fn signal_from_key_menu_keys_map_to_unix_signals() {
        let cases = [
            ("k", Some(9)),
            ("t", Some(15)),
            ("s", Some(19)),
            ("c", Some(18)),
            ("z", None),
        ];
        for (key, want) in cases {
            assert_eq!(signal_from_key(key), want, "key {key:?}");
        }
    }

    #[test]
    fn encode_mouse_scroll_direction_count_and_format_encode_per_line() {
        let cases: [(&str, i32, bool, &[u8]); 6] = [
            ("sgr up", 1, true, b"\x1b[<64;10;5M"),
            ("sgr down", -1, true, b"\x1b[<65;10;5M"),
            ("legacy up", 1, false, &[0x1b, b'[', b'M', 96, 42, 37]),
            ("legacy down", -1, false, &[0x1b, b'[', b'M', 97, 42, 37]),
            (
                "three lines repeat",
                3,
                true,
                b"\x1b[<64;10;5M\x1b[<64;10;5M\x1b[<64;10;5M",
            ),
            ("zero lines is empty", 0, true, b""),
        ];
        for (label, lines, sgr, want) in cases {
            assert_eq!(encode_mouse_scroll(10, 5, lines, sgr), want, "{label}");
        }
    }

    /// A press/release/motion at column 10, row 5.
    fn ev(button: MouseButton, kind: MouseEventKind, mods: MouseMods) -> MouseEvent {
        MouseEvent {
            button,
            kind,
            col: 10,
            row: 5,
            mods,
        }
    }

    const NO_MODS: MouseMods = MouseMods {
        ctrl: false,
        alt: false,
        shift: false,
    };

    #[test]
    fn encode_mouse_button_sgr_packs_button_mods_and_motion_into_cb() {
        use MouseButton::{Left, Middle, Right};
        use MouseEventKind::{Motion, Press, Release};
        let all = MouseMods {
            ctrl: true,
            alt: true,
            shift: true,
        };
        let cases = [
            ("left press", ev(Left, Press, NO_MODS), "\x1b[<0;10;5M"),
            ("middle press", ev(Middle, Press, NO_MODS), "\x1b[<1;10;5M"),
            ("right press", ev(Right, Press, NO_MODS), "\x1b[<2;10;5M"),
            // Release keeps the real button and ends in lowercase `m`.
            ("left release", ev(Left, Release, NO_MODS), "\x1b[<0;10;5m"),
            ("motion +32", ev(Left, Motion, NO_MODS), "\x1b[<32;10;5M"),
            // shift 4 + alt 8 + ctrl 16 = 28
            ("all mods", ev(Left, Press, all), "\x1b[<28;10;5M"),
        ];
        for (label, event, want) in cases {
            assert_eq!(
                encode_mouse_button(&event, true),
                want.as_bytes(),
                "{label}"
            );
        }
    }

    #[test]
    fn encode_mouse_button_legacy_offsets_by_32_and_collapses_release_to_3() {
        use MouseButton::{Left, Right};
        use MouseEventKind::{Motion, Press, Release};
        // (label, event, cb before the +32 offset)
        let cases = [
            ("left press", ev(Left, Press, NO_MODS), 0),
            ("motion +32", ev(Left, Motion, NO_MODS), 32),
            // Legacy has no per-button release.
            ("right release is 3", ev(Right, Release, NO_MODS), 3),
        ];
        for (label, event, cb) in cases {
            // cx = 10 + 32 = 42, cy = 5 + 32 = 37
            assert_eq!(
                encode_mouse_button(&event, false),
                [0x1b, b'[', b'M', cb + 32, 42, 37],
                "{label}"
            );
        }
    }

    #[test]
    fn encode_mouse_button_legacy_coordinates_saturate_at_223() {
        let event = MouseEvent {
            button: MouseButton::Left,
            kind: MouseEventKind::Press,
            col: 300,
            row: 1,
            mods: MouseMods::default(),
        };
        // col clamps to 223, +32 = 255; row 1 + 32 = 33
        assert_eq!(
            encode_mouse_button(&event, false),
            [0x1b, b'[', b'M', 32, 255, 33]
        );
    }

    #[test]
    fn char_to_proto_key_letters_and_digits_get_distinct_physical_keys() {
        let chars: Vec<char> = ('a'..='z').chain('0'..='9').collect();
        let mut seen = std::collections::HashSet::new();
        for c in chars {
            let (key, text, unshifted) = char_to_proto_key(c);
            assert_ne!(key, ProtoKey::Unidentified, "{c:?} has a physical key");
            assert!(seen.insert(key as u16), "{c:?} maps to a duplicate key");
            assert_eq!((text, unshifted), (c.to_string(), c as u32), "{c:?}");
            let upper = c.to_ascii_uppercase();
            assert_eq!(
                char_to_proto_key(upper),
                (key, upper.to_string(), c as u32),
                "{upper:?} shares {c:?}'s key and unshifted codepoint, keeps its glyph"
            );
        }
    }

    #[test]
    fn char_to_proto_key_named_examples_map_to_expected_triples() {
        let cases = [
            ('a', ProtoKey::A, 'a' as u32),
            ('5', ProtoKey::Digit5, '5' as u32),
            (' ', ProtoKey::Space, ' ' as u32),
            // ASCII punctuation has no physical key but still reports a codepoint.
            ('!', ProtoKey::Unidentified, '!' as u32),
            // Non-ASCII has no unshifted codepoint.
            ('é', ProtoKey::Unidentified, 0),
        ];
        for (c, key, unshifted) in cases {
            assert_eq!(
                char_to_proto_key(c),
                (key, c.to_string(), unshifted),
                "{c:?}"
            );
        }
    }
}
