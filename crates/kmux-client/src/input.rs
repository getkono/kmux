use crate::key::{Key, Modifiers, NamedKey};

/// Convert a key event to the byte sequence that should be sent to the PTY.
///
/// Returns `None` for keys that don't produce output (e.g., unknown named keys).
/// `app_cursor`: whether the terminal is in application-cursor mode (DECCKM).
pub fn key_to_bytes(
    key: &Key,
    modifiers: Modifiers,
    text: Option<&str>,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    match key {
        Key::Character(c) => {
            let s = c.as_str();
            if modifiers.contains(Modifiers::CTRL)
                && let Some(ch) = s.chars().next()
            {
                let lower = ch.to_ascii_lowercase();
                if lower.is_ascii_alphabetic() {
                    return Some(vec![lower as u8 - b'a' + 1]);
                }
            }
            if let Some(t) = text {
                Some(t.as_bytes().to_vec())
            } else {
                Some(s.as_bytes().to_vec())
            }
        }
        Key::Named(named) => {
            let bytes: &[u8] = match named {
                NamedKey::Space => b" ",
                NamedKey::Enter => b"\r",
                NamedKey::Tab => b"\t",
                NamedKey::Backspace => b"\x7f",
                NamedKey::Escape => b"\x1b",
                NamedKey::Delete => b"\x1b[3~",
                NamedKey::ArrowUp => {
                    if app_cursor {
                        b"\x1bOA"
                    } else {
                        b"\x1b[A"
                    }
                }
                NamedKey::ArrowDown => {
                    if app_cursor {
                        b"\x1bOB"
                    } else {
                        b"\x1b[B"
                    }
                }
                NamedKey::ArrowRight => {
                    if app_cursor {
                        b"\x1bOC"
                    } else {
                        b"\x1b[C"
                    }
                }
                NamedKey::ArrowLeft => {
                    if app_cursor {
                        b"\x1bOD"
                    } else {
                        b"\x1b[D"
                    }
                }
                NamedKey::Home => {
                    if app_cursor {
                        b"\x1bOH"
                    } else {
                        b"\x1b[H"
                    }
                }
                NamedKey::End => {
                    if app_cursor {
                        b"\x1bOF"
                    } else {
                        b"\x1b[F"
                    }
                }
                NamedKey::PageUp => b"\x1b[5~",
                NamedKey::PageDown => b"\x1b[6~",
                NamedKey::Insert => b"\x1b[2~",
                NamedKey::F1 => b"\x1bOP",
                NamedKey::F2 => b"\x1bOQ",
                NamedKey::F3 => b"\x1bOR",
                NamedKey::F4 => b"\x1bOS",
                NamedKey::F5 => b"\x1b[15~",
                NamedKey::F6 => b"\x1b[17~",
                NamedKey::F7 => b"\x1b[18~",
                NamedKey::F8 => b"\x1b[19~",
                NamedKey::F9 => b"\x1b[20~",
                NamedKey::F10 => b"\x1b[21~",
                NamedKey::F11 => b"\x1b[23~",
                NamedKey::F12 => b"\x1b[24~",
                _ => return None,
            };
            Some(bytes.to_vec())
        }
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

    fn no_mods() -> Modifiers {
        Modifiers::empty()
    }

    #[test]
    fn escape_produces_0x1b() {
        let result = key_to_bytes(&Key::Named(NamedKey::Escape), no_mods(), None, false);
        assert_eq!(result, Some(vec![0x1b]));
    }

    #[test]
    fn insert_produces_csi_2_tilde() {
        let result = key_to_bytes(&Key::Named(NamedKey::Insert), no_mods(), None, false);
        assert_eq!(result, Some(b"\x1b[2~".to_vec()));
    }

    #[test]
    fn arrow_up_normal_mode() {
        let result = key_to_bytes(&Key::Named(NamedKey::ArrowUp), no_mods(), None, false);
        assert_eq!(result, Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn arrow_up_app_cursor_mode() {
        let result = key_to_bytes(&Key::Named(NamedKey::ArrowUp), no_mods(), None, true);
        assert_eq!(result, Some(b"\x1bOA".to_vec()));
    }

    #[test]
    fn ctrl_c_produces_etx() {
        let result = key_to_bytes(&Key::Character("c".into()), Modifiers::CTRL, None, false);
        assert_eq!(result, Some(vec![0x03]));
    }

    #[test]
    fn enter_produces_cr() {
        let result = key_to_bytes(&Key::Named(NamedKey::Enter), no_mods(), None, false);
        assert_eq!(result, Some(vec![0x0d]));
    }

    #[test]
    fn backspace_produces_del() {
        let result = key_to_bytes(&Key::Named(NamedKey::Backspace), no_mods(), None, false);
        assert_eq!(result, Some(vec![0x7f]));
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
