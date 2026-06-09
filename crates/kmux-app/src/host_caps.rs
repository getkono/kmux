use kmux_protocol::messages::ClientCapabilities;

/// Detect what capabilities the local terminal supports by probing environment variables.
///
/// Rules:
/// - `truecolor`: true if `$COLORTERM` is "truecolor" or "24bit", or `$TERM` ends with
///   "-direct" or "-truecolor".
/// - `kitty_graphics`: always false — the `CellGrid` renderer has no image support.
/// - `kitty_keyboard`: from `kitty_keyboard_supported` — set by the frontend when
///   its toolkit reports keyboard-enhancement support.
/// - `term`/`term_program`: informational, forwarded as-is for server-side logging.
pub fn detect(kitty_keyboard_supported: bool) -> ClientCapabilities {
    let term = std::env::var("TERM").ok();
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    let term_program = std::env::var("TERM_PROGRAM").ok();

    let truecolor = matches!(colorterm.as_str(), "truecolor" | "24bit")
        || term
            .as_deref()
            .map(|t| t.ends_with("-direct") || t.ends_with("-truecolor"))
            .unwrap_or(false);

    ClientCapabilities {
        truecolor,
        kitty_graphics: false,
        kitty_keyboard: kitty_keyboard_supported,
        term,
        term_program,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truecolor_from_colorterm() {
        // SAFETY: single-threaded test binary; no other threads read these vars.
        unsafe {
            std::env::set_var("COLORTERM", "truecolor");
            std::env::remove_var("TERM");
        }
        let caps = detect(false);
        assert!(caps.truecolor);
        assert!(!caps.kitty_graphics);
        assert!(!caps.kitty_keyboard);
    }

    #[test]
    fn truecolor_from_24bit() {
        unsafe {
            std::env::set_var("COLORTERM", "24bit");
            std::env::remove_var("TERM");
        }
        let caps = detect(false);
        assert!(caps.truecolor);
    }

    #[test]
    fn truecolor_from_term_direct() {
        unsafe {
            std::env::remove_var("COLORTERM");
            std::env::set_var("TERM", "xterm-direct");
        }
        let caps = detect(false);
        assert!(caps.truecolor);
    }

    #[test]
    fn no_truecolor_for_plain_term() {
        unsafe {
            std::env::set_var("COLORTERM", "");
            std::env::set_var("TERM", "xterm-256color");
        }
        let caps = detect(false);
        assert!(!caps.truecolor);
    }

    #[test]
    fn kitty_keyboard_param_is_honored() {
        unsafe {
            std::env::set_var("COLORTERM", "");
            std::env::set_var("TERM", "xterm-256color");
        }
        assert!(detect(true).kitty_keyboard);
        assert!(!detect(false).kitty_keyboard);
    }
}
