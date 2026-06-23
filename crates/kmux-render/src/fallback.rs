//! The bundled universal symbol fallback face.
//!
//! The configured terminal font (e.g. the generic `monospace`) usually lacks
//! Powerline separators (U+E0B0–U+E0D7) and Nerd Font icon glyphs (Private Use
//! Area), so they render as tofu/blank. We embed "Symbols Nerd Font Mono" — an
//! icons-only font with no Latin glyphs — and rasterize from it whenever the
//! primary face has no glyph for a character (see [`crate::atlas`], the GPU/Metal
//! path). The same bytes are registered with the OS font systems by the
//! frontends (fontconfig on GTK, CoreText on macOS) so the CPU render paths
//! resolve the identical glyphs — hence [`symbol_fallback_bytes`] is available
//! even in the wgpu-free build (the CPU renderer is the default).
//!
//! See `assets/NOTICE.md` for the font's provenance and license (Nerd Fonts, MIT).

/// Symbols Nerd Font Mono (icons-only). Embedded so the fallback is deterministic
/// and works out of the box regardless of which fonts are installed.
static FALLBACK_BYTES: &[u8] = include_bytes!("../assets/SymbolsNerdFontMono-Regular.ttf");

/// The raw bytes of the bundled fallback font, for the frontends to register
/// with their OS font systems (fontconfig on GTK, CoreText on macOS) so the CPU
/// render paths resolve the same glyphs the GPU atlas does. Always available
/// (no `text` feature needed) — the CPU paths need it in every build.
pub fn symbol_fallback_bytes() -> &'static [u8] {
    FALLBACK_BYTES
}

/// The process-wide symbol fallback [`FontFace`](crate::metrics::FontFace),
/// loaded once. Used by the glyph atlas to rasterize Powerline/Nerd glyphs
/// missing from the primary face.
#[cfg(feature = "text")]
pub fn symbol_fallback() -> &'static crate::metrics::FontFace {
    use std::sync::OnceLock;
    static FACE: OnceLock<crate::metrics::FontFace> = OnceLock::new();
    FACE.get_or_init(|| crate::metrics::FontFace::from_static(FALLBACK_BYTES))
}

#[cfg(all(test, feature = "text"))]
mod tests {
    use super::*;
    use swash::FontRef;

    #[test]
    fn fallback_face_loads_and_has_powerline_and_icon_glyphs() {
        let face = symbol_fallback();
        let font: FontRef<'_> = face.as_ref().expect("bundled fallback font parses");
        // The whole point of the bundle: these are the glyphs the default
        // monospace font lacks. A non-zero glyph id means the font maps them.
        for ch in ['\u{e0b0}', '\u{e0b1}', '\u{e0a0}', '\u{f015}'] {
            assert_ne!(
                font.charmap().map(ch),
                0,
                "fallback font should map U+{:04X}",
                ch as u32
            );
        }
    }
}
