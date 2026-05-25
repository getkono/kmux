//! Mouse-event encoding for the kmux client.
//!
//! Key encoding moved server-side in PROTOCOL_VERSION 18: the daemon owns a
//! per-pane Ghostty key encoder and decides the byte sequence for each
//! keystroke based on what the inner program negotiated (DECCKM, kitty kbd
//! flags, modifyOtherKeys).  The client now sends structured key events via
//! `ClientMessage::PtyKeyBatch`.  See `kmux::key_convert::convert_to_protocol_key`.

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
}
