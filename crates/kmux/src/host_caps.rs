use kmux_protocol::messages::ClientCapabilities;

/// Detect what capabilities the local terminal supports by probing environment variables.
///
/// Rules:
/// - `truecolor`: true if `$COLORTERM` is "truecolor" or "24bit", or `$TERM` ends with
///   "-direct" or "-truecolor".
/// - `kitty_graphics`: always false — the TUI renderer (`CellGrid`) has no image support.
/// - `kitty_keyboard`: always false — input is read via crossterm, which does not use the
///   kitty keyboard protocol.
/// - `term`/`term_program`: informational, forwarded as-is for server-side logging.
pub fn detect() -> ClientCapabilities {
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
        kitty_keyboard: false,
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
        let caps = detect();
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
        let caps = detect();
        assert!(caps.truecolor);
    }

    #[test]
    fn truecolor_from_term_direct() {
        unsafe {
            std::env::remove_var("COLORTERM");
            std::env::set_var("TERM", "xterm-direct");
        }
        let caps = detect();
        assert!(caps.truecolor);
    }

    #[test]
    fn no_truecolor_for_plain_term() {
        unsafe {
            std::env::set_var("COLORTERM", "");
            std::env::set_var("TERM", "xterm-256color");
        }
        let caps = detect();
        assert!(!caps.truecolor);
    }
}
