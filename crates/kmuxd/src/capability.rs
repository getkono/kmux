use std::collections::HashMap;

use kmux_protocol::messages::ClientCapabilities;

/// Compute the kitty feature flags for a pane's backend by ANDing all
/// attached clients' declared capabilities.
///
/// Returns `(kitty_graphics, kitty_keyboard)`.
///
/// When no clients are attached (e.g. at spawn time before the first
/// `Attach` arrives), returns `(false, false)` — the safest defaults since
/// phase A of kmux drops image data anyway.
pub fn intersect_for_atomics<'a>(
    caps: impl IntoIterator<Item = &'a ClientCapabilities>,
) -> (bool, bool) {
    let mut any = false;
    let mut graphics = true;
    let mut keyboard = true;
    for c in caps {
        any = true;
        graphics &= c.kitty_graphics;
        keyboard &= c.kitty_keyboard;
    }
    if any {
        (graphics, keyboard)
    } else {
        (false, false)
    }
}

/// Build the environment variable overrides that should be applied to every
/// shell spawned inside a kmux pane.
///
/// These vars reflect what the server-side VT emulator (wezterm-term) parses,
/// not the launching daemon's own terminal identity:
///
/// - `TERM=xterm-256color` — always, because wezterm-term is an xterm-family
///   parser.  Claiming the client's native TERM would cause the shell to emit
///   sequences the parser might not handle.
/// - `COLORTERM=truecolor` — always, because wezterm-term parses 24-bit SGR
///   unconditionally and kmux forwards RGB cells on the wire.
/// - `TERM_PROGRAM=kmux` / `TERM_PROGRAM_VERSION=<version>` — override
///   launcher leakage so that feature-sniffers (Starship, bat, etc.) see a
///   consistent identity.
///
/// The `_seed_caps` parameter is kept for future use (e.g. per-capability
/// TERM selection if the backend becomes configurable).
pub fn pane_spawn_env(_seed_caps: &ClientCapabilities) -> HashMap<String, String> {
    HashMap::from([
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
        ("TERM_PROGRAM".into(), "kmux".into()),
        (
            "TERM_PROGRAM_VERSION".into(),
            env!("CARGO_PKG_VERSION").into(),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(kitty_graphics: bool, kitty_keyboard: bool, truecolor: bool) -> ClientCapabilities {
        ClientCapabilities {
            kitty_graphics,
            kitty_keyboard,
            truecolor,
            ..Default::default()
        }
    }

    #[test]
    fn intersect_empty_is_false_false() {
        assert_eq!(intersect_for_atomics([].iter()), (false, false));
    }

    #[test]
    fn intersect_single_client_passes_through() {
        let c = caps(true, false, true);
        assert_eq!(intersect_for_atomics([c].iter()), (true, false));
    }

    #[test]
    fn intersect_two_clients_ands_flags() {
        let a = caps(true, true, true);
        let b = caps(true, false, true);
        assert_eq!(intersect_for_atomics([a, b].iter()), (true, false));
    }

    #[test]
    fn intersect_one_without_graphics_disables_it() {
        let a = caps(true, true, true);
        let b = caps(false, true, true);
        assert_eq!(intersect_for_atomics([a, b].iter()), (false, true));
    }

    #[test]
    fn pane_spawn_env_has_required_keys() {
        let c = caps(false, false, false);
        let env = pane_spawn_env(&c);
        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));
        assert_eq!(env.get("TERM_PROGRAM").map(String::as_str), Some("kmux"));
        assert!(env.contains_key("TERM_PROGRAM_VERSION"));
    }
}
