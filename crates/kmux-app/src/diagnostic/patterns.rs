//! Pattern generators for the render diagnostic suite (issue #145).
//!
//! [`pattern_bytes`] is the single source of truth for what a diagnostic test
//! paints. The bytes are plain UTF-8 text plus ANSI/SGR escape sequences; the
//! same bytes are written both by `kmux diagnostic <test> --emit` (to the host
//! terminal) and, via the launched session, into the kmux renderer under test.

use super::DiagnosticTest;

/// ESC, the lead byte of every ANSI/SGR escape sequence.
const ESC: &str = "\x1b";

/// The raw bytes a diagnostic test paints. The renderer-under-test is fed
/// exactly these bytes, so they double as the visual ground truth.
pub fn pattern_bytes(test: DiagnosticTest) -> Vec<u8> {
    let mut out = String::new();
    match test {
        DiagnosticTest::Glyphs => glyphs(&mut out),
        DiagnosticTest::Attrs => attrs(&mut out),
        DiagnosticTest::Colors => colors(&mut out),
        DiagnosticTest::Unicode => unicode(&mut out),
        DiagnosticTest::Boxes => boxes(&mut out),
        DiagnosticTest::Progress => progress(&mut out),
        DiagnosticTest::All => {
            for t in DiagnosticTest::EACH {
                section_header(&mut out, t.name());
                pattern_into(t, &mut out);
                out.push('\n');
            }
        }
    }
    out.into_bytes()
}

/// Append a single (non-`All`) test's body into `out`.
fn pattern_into(test: DiagnosticTest, out: &mut String) {
    match test {
        DiagnosticTest::Glyphs => glyphs(out),
        DiagnosticTest::Attrs => attrs(out),
        DiagnosticTest::Colors => colors(out),
        DiagnosticTest::Unicode => unicode(out),
        DiagnosticTest::Boxes => boxes(out),
        DiagnosticTest::Progress => progress(out),
        DiagnosticTest::All => {}
    }
}

/// A reverse-video banner separating sections in the `all` pattern.
fn section_header(out: &mut String, name: &str) {
    out.push_str(&format!("{ESC}[7m── {name} {ESC}[0m\n"));
}

/// Reset all SGR attributes.
fn reset(out: &mut String) {
    out.push_str(ESC);
    out.push_str("[0m");
}

// ── glyphs ────────────────────────────────────────────────────────────────

/// ASCII + common Unicode glyphs — the primary repro for "glyphs not rendered".
fn glyphs(out: &mut String) {
    out.push_str("Printable ASCII (0x20–0x7E):\n");
    for code in 0x20u8..=0x7e {
        out.push(code as char);
        if (code - 0x20 + 1) % 32 == 0 {
            out.push('\n');
        }
    }
    out.push('\n');

    out.push_str("Latin-1 / accents:  ");
    out.push_str("À Á Â Ã Ä Å Æ Ç È É Ê Ë Ì Í Î Ï Ñ Ò Ó Ô Õ Ö Ø Ù Ú Û Ü ß à é ï ñ ö ü ÿ\n");

    out.push_str("Punctuation / symbols:  ");
    out.push_str("‘ ’ “ ” – — … • · † ‡ § ¶ © ® ™ ° ± × ÷ ≈ ≠ ≤ ≥ ∞ µ √ ∑ ∏ ∫ ← ↑ → ↓ ↔\n");

    out.push_str("Currency:  ");
    out.push_str("$ ¢ £ ¤ ¥ € ₩ ₪ ₫ ₹ ₿\n");

    out.push_str("Block elements:  ");
    out.push_str("░ ▒ ▓ █  ▁▂▃▄▅▆▇█  ▏▎▍▌▋▊▉█\n");

    out.push_str("Braille:  ");
    out.push_str("⠁⠉⠋⠛⠟⠿⡿⣿⣷⣯⣟⡿⢿⣻⣽⣾\n");

    out.push_str("Powerline (may tofu without a patched font):  ");
    out.push_str("\u{e0b0}\u{e0b1}\u{e0b2}\u{e0b3}  \u{e0a0}\u{e0a1}\u{e0a2}\n");
}

// ── attrs ─────────────────────────────────────────────────────────────────

/// Text attributes across the four synthesized font faces, exercising the
/// atlas's `FaceStyle` keys and the SGR attribute renderer.
fn attrs(out: &mut String) {
    let sample = "Sphinx AaBbCc 0123";
    let rows: &[(&str, &str)] = &[
        ("normal", ""),
        ("bold", "1"),
        ("dim", "2"),
        ("italic", "3"),
        ("bold+italic", "1;3"),
        ("underline", "4"),
        ("double-underline", "21"),
        ("strikethrough", "9"),
        ("reverse", "7"),
        ("blink", "5"),
    ];
    for (label, sgr) in rows {
        out.push_str(&format!("{label:>18}  "));
        if sgr.is_empty() {
            out.push_str(sample);
        } else {
            out.push_str(&format!("{ESC}[{sgr}m{sample}"));
            reset(out);
        }
        out.push('\n');
    }
}

// ── colors ────────────────────────────────────────────────────────────────

/// 16-color, 256-color (cube + grayscale), and 24-bit truecolor ramps.
fn colors(out: &mut String) {
    out.push_str("16 ANSI colors (fg / bg):\n");
    for code in 0..16 {
        out.push_str(&format!("{ESC}[38;5;{code}m {code:>3}"));
    }
    reset(out);
    out.push('\n');
    for code in 0..16 {
        out.push_str(&format!("{ESC}[48;5;{code}m    "));
    }
    reset(out);
    out.push_str("\n\n");

    out.push_str("256-color cube:\n");
    for code in 16..232 {
        out.push_str(&format!("{ESC}[48;5;{code}m  "));
        if (code - 16 + 1) % 36 == 0 {
            reset(out);
            out.push('\n');
        }
    }
    reset(out);
    out.push('\n');

    out.push_str("Grayscale ramp:\n");
    for code in 232..256 {
        out.push_str(&format!("{ESC}[48;5;{code}m  "));
    }
    reset(out);
    out.push_str("\n\n");

    out.push_str("Truecolor (24-bit) hue sweep:\n");
    let width = 64;
    for i in 0..width {
        let (r, g, b) = hue(i as f32 / width as f32);
        out.push_str(&format!("{ESC}[48;2;{r};{g};{b}m "));
    }
    reset(out);
    out.push('\n');
}

/// A point on a simple red→green→blue→red hue wheel, for the truecolor sweep.
fn hue(t: f32) -> (u8, u8, u8) {
    let h = t * 6.0;
    let x = (255.0 * (1.0 - (h % 2.0 - 1.0).abs())) as u8;
    match h as u32 {
        0 => (255, x, 0),
        1 => (x, 255, 0),
        2 => (0, 255, x),
        3 => (0, x, 255),
        4 => (x, 0, 255),
        _ => (255, 0, x),
    }
}

// ── unicode ───────────────────────────────────────────────────────────────

/// Wide CJK, emoji, and combining marks — a cell-width stress test. The `|`
/// columns make double-width and zero-width handling visible at a glance.
fn unicode(out: &mut String) {
    out.push_str("Wide CJK (each glyph should span two cells):\n");
    out.push_str("|你|好|世|界|  |こ|ん|に|ち|は|  |한|국|어|\n\n");

    out.push_str("Emoji (typically double-width, may be color):\n");
    out.push_str("|😀|🎉|🚀|❤|🌍|🔥|✨|🐙|\n\n");

    out.push_str("Combining marks (precomposed vs. composed):\n");
    out.push_str("é (U+00E9)   vs   e\u{0301} (e + U+0301)\n");
    out.push_str("ñ (U+00F1)   vs   n\u{0303} (n + U+0303)\n\n");

    out.push_str("ZWJ sequence (one grapheme):  👨\u{200d}👩\u{200d}👧\u{200d}👦\n");
}

// ── boxes ─────────────────────────────────────────────────────────────────

/// Box-drawing and line-drawing alignment grid — surfaces cell-metric and
/// glyph-misalignment bugs (corners and joints must line up).
fn boxes(out: &mut String) {
    out.push_str("Light:        Heavy:        Double:       Rounded:\n");
    out.push_str("┌───┬───┐     ┏━━━┳━━━┓     ╔═══╦═══╗     ╭───┬───╮\n");
    out.push_str("│   │   │     ┃   ┃   ┃     ║   ║   ║     │   │   │\n");
    out.push_str("├───┼───┤     ┣━━━╋━━━┫     ╠═══╬═══╣     ├───┼───┤\n");
    out.push_str("│   │   │     ┃   ┃   ┃     ║   ║   ║     │   │   │\n");
    out.push_str("└───┴───┘     ┗━━━┻━━━┛     ╚═══╩═══╝     ╰───┴───╯\n\n");

    out.push_str("Mixed joints:  ╴ ╵ ╶ ╷ ┄ ┅ ┈ ┉ ╌ ╍ ╎ ╏ ┝ ┥ ┰ ┸ ╪ ╫\n");
    out.push_str("Diagonals:     ╱ ╲ ╳    Half blocks:  ▖ ▗ ▘ ▝ ▙ ▟ ▛ ▜\n");
}

// ── progress ──────────────────────────────────────────────────────────────

/// Static single-frame fallback for the OSC 9;4 progress test (issue #125).
///
/// The progress bar is *window chrome*, not grid content, and the real test is
/// the animated loop in [`super::emit_progress_animated`]. These bytes are what
/// `pattern_bytes(Progress)` yields — a legend plus one concrete frame — so the
/// host-terminal `--emit` and any byte-level test still see a valid OSC 9;4.
fn progress(out: &mut String) {
    out.push_str("OSC 9;4 progress report (ConEmu / Windows-Terminal).\n");
    out.push_str(
        "Run `kmux diagnostic progress` for the animated test; this static frame\n\
         sets a single state.\n\n",
    );
    out.push_str("States: 0=remove  1=set  2=error  3=indeterminate  4=pause   value 0..=100\n");
    // One concrete frame: normal progress at 50%.
    out.push_str(ESC);
    out.push_str("]9;4;1;50\x07");
    out.push_str("\nSet: state=1 (set), 50%\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(test: DiagnosticTest) -> String {
        String::from_utf8(pattern_bytes(test)).expect("pattern bytes are valid UTF-8")
    }

    #[test]
    fn every_pattern_is_nonempty_valid_utf8() {
        for test in [
            DiagnosticTest::Glyphs,
            DiagnosticTest::Attrs,
            DiagnosticTest::Colors,
            DiagnosticTest::Unicode,
            DiagnosticTest::Boxes,
            DiagnosticTest::Progress,
            DiagnosticTest::All,
        ] {
            let s = render(test);
            assert!(!s.is_empty(), "{} pattern is empty", test.name());
        }
    }

    #[test]
    fn progress_fallback_emits_osc_9_4() {
        let s = render(DiagnosticTest::Progress);
        assert!(s.contains("\x1b]9;4;"), "progress fallback missing OSC 9;4");
    }

    #[test]
    fn colors_uses_indexed_and_truecolor_sgr() {
        let s = render(DiagnosticTest::Colors);
        assert!(s.contains("48;5;"), "missing 256-color background SGR");
        assert!(s.contains("48;2;"), "missing truecolor background SGR");
    }

    #[test]
    fn attrs_exercises_the_sgr_codes() {
        let s = render(DiagnosticTest::Attrs);
        for sgr in ["[1m", "[3m", "[4m", "[9m", "[7m"] {
            assert!(s.contains(sgr), "attrs missing SGR {sgr}");
        }
    }

    #[test]
    fn all_includes_every_section_header() {
        let s = render(DiagnosticTest::All);
        for test in DiagnosticTest::EACH {
            assert!(
                s.contains(&format!("── {} ", test.name())),
                "`all` missing the {} section",
                test.name()
            );
        }
    }

    #[test]
    fn glyphs_covers_printable_ascii() {
        let s = render(DiagnosticTest::Glyphs);
        for ch in ['A', 'z', '0', '~', '!', '@'] {
            assert!(s.contains(ch), "glyphs missing ASCII {ch:?}");
        }
    }
}
