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
    from_env_values(
        std::env::var("TERM").ok(),
        std::env::var("COLORTERM").ok(),
        std::env::var("TERM_PROGRAM").ok(),
        kitty_keyboard_supported,
    )
}

/// Pure core of [`detect`]: takes the environment values as arguments so tests
/// don't have to mutate process-global env vars (which races across the
/// multi-threaded test harness).
fn from_env_values(
    term: Option<String>,
    colorterm: Option<String>,
    term_program: Option<String>,
    kitty_keyboard_supported: bool,
) -> ClientCapabilities {
    let truecolor = matches!(colorterm.as_deref(), Some("truecolor" | "24bit"))
        || term
            .as_deref()
            .is_some_and(|t| t.ends_with("-direct") || t.ends_with("-truecolor"));

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

    fn caps(
        term: Option<&str>,
        colorterm: Option<&str>,
        kitty_keyboard: bool,
    ) -> ClientCapabilities {
        from_env_values(
            term.map(String::from),
            colorterm.map(String::from),
            None,
            kitty_keyboard,
        )
    }

    #[test]
    fn truecolor_from_colorterm() {
        let c = caps(None, Some("truecolor"), false);
        assert!(c.truecolor);
        assert!(!c.kitty_graphics);
        assert!(!c.kitty_keyboard);
    }

    #[test]
    fn truecolor_from_24bit() {
        assert!(caps(None, Some("24bit"), false).truecolor);
    }

    #[test]
    fn truecolor_from_term_direct() {
        assert!(caps(Some("xterm-direct"), None, false).truecolor);
    }

    #[test]
    fn no_truecolor_for_plain_term() {
        assert!(!caps(Some("xterm-256color"), Some(""), false).truecolor);
    }

    #[test]
    fn kitty_keyboard_param_is_honored() {
        assert!(caps(Some("xterm-256color"), Some(""), true).kitty_keyboard);
        assert!(!caps(Some("xterm-256color"), Some(""), false).kitty_keyboard);
    }
}
