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
