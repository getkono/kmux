//! Toolkit-agnostic color palette (theme) model and loading.
//!
//! This is the single source of truth for kmux themes. Colors are stored as a
//! plain [`Rgb`] triple; each frontend converts to its own color type at the
//! render boundary (e.g. `kmux-gtk` maps `Rgb` to `gdk::RGBA`, and the Swift app
//! to an `NSColor` via FFI). Nothing here depends on a UI toolkit.

use serde::Deserialize;

const ONE_DARK_TOML: &str = include_str!("../../../themes/one-dark.toml");
const CATPPUCCIN_LATTE_TOML: &str = include_str!("../../../themes/catppuccin-latte.toml");
const CATPPUCCIN_FRAPPE_TOML: &str = include_str!("../../../themes/catppuccin-frappe.toml");
const CATPPUCCIN_MACCHIATO_TOML: &str = include_str!("../../../themes/catppuccin-macchiato.toml");
const CATPPUCCIN_MOCHA_TOML: &str = include_str!("../../../themes/catppuccin-mocha.toml");
const DRACULA_TOML: &str = include_str!("../../../themes/dracula.toml");

/// A 24-bit RGB color. Toolkit-neutral; frontends convert at the render leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Runtime color palette. Frontend-agnostic.
///
/// `PartialEq`/`Eq` lets frontends detect a `/theme` palette change cheaply
/// (e.g. to reload chrome styling). It compares *every* field — including
/// `cursor_bg`/`cursor_fg` — so a cursor-only theme change is not missed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub bg: Rgb,
    pub fg: Rgb,
    pub fg_dim: Rgb,
    pub accent: Rgb,
    pub green: Rgb,
    pub red: Rgb,
    pub yellow: Rgb,
    pub purple: Rgb,
    pub orange: Rgb,
    pub status_bg: Rgb,
    /// Background of the inner-pane cursor for shapes that fill the cell
    /// (Block, HollowBlock); also used as the foreground glyph color for
    /// Bar/Underline. Defaults to `fg` for high contrast against `bg`.
    pub cursor_bg: Rgb,
    /// Text color drawn on top of the Block cursor's background. Defaults to
    /// `bg` so the underlying glyph stays readable.
    pub cursor_fg: Rgb,
}

/// Deserialisation shape for `themes/*.toml` files.
#[derive(Debug, Deserialize)]
struct ThemeFile {
    #[allow(dead_code)]
    name: String,
    bg: String,
    fg: String,
    fg_dim: String,
    accent: String,
    green: String,
    red: String,
    yellow: String,
    purple: String,
    orange: String,
    status_bg: String,
    /// Optional. Defaults to `fg` if omitted.
    #[serde(default)]
    cursor_bg: Option<String>,
    /// Optional. Defaults to `bg` if omitted.
    #[serde(default)]
    cursor_fg: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("invalid hex color '{value}' in field '{field}'")]
    InvalidColor { field: &'static str, value: String },
    #[error("failed to parse theme TOML: {0}")]
    Toml(#[from] toml::de::Error),
}

fn parse_hex(field: &'static str, s: &str) -> Result<Rgb, ThemeError> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return Err(ThemeError::InvalidColor {
            field,
            value: s.to_string(),
        });
    }
    let r = u8::from_str_radix(&s[0..2], 16).map_err(|_| ThemeError::InvalidColor {
        field,
        value: s.to_string(),
    })?;
    let g = u8::from_str_radix(&s[2..4], 16).map_err(|_| ThemeError::InvalidColor {
        field,
        value: s.to_string(),
    })?;
    let b = u8::from_str_radix(&s[4..6], 16).map_err(|_| ThemeError::InvalidColor {
        field,
        value: s.to_string(),
    })?;
    Ok(Rgb { r, g, b })
}

impl ThemeFile {
    fn into_theme(self) -> Result<Theme, ThemeError> {
        let bg = parse_hex("bg", &self.bg)?;
        let fg = parse_hex("fg", &self.fg)?;
        let cursor_bg = match self.cursor_bg.as_deref() {
            Some(s) => parse_hex("cursor_bg", s)?,
            None => fg,
        };
        let cursor_fg = match self.cursor_fg.as_deref() {
            Some(s) => parse_hex("cursor_fg", s)?,
            None => bg,
        };
        Ok(Theme {
            bg,
            fg,
            fg_dim: parse_hex("fg_dim", &self.fg_dim)?,
            accent: parse_hex("accent", &self.accent)?,
            green: parse_hex("green", &self.green)?,
            red: parse_hex("red", &self.red)?,
            yellow: parse_hex("yellow", &self.yellow)?,
            purple: parse_hex("purple", &self.purple)?,
            orange: parse_hex("orange", &self.orange)?,
            status_bg: parse_hex("status_bg", &self.status_bg)?,
            cursor_bg,
            cursor_fg,
        })
    }
}

fn parse_builtin(toml_str: &str) -> Theme {
    toml::from_str::<ThemeFile>(toml_str)
        .expect("built-in theme TOML is malformed")
        .into_theme()
        .expect("built-in theme has invalid colors")
}

/// The set of built-in theme names, in display order. Used to drive completion.
pub const BUILTIN_THEMES: &[&str] = &[
    "one-dark",
    "catppuccin-latte",
    "catppuccin-frappe",
    "catppuccin-macchiato",
    "catppuccin-mocha",
    "dracula",
];

/// Returns the named built-in theme, or `None` if `name` is not recognised.
pub fn builtin_theme(name: &str) -> Option<Theme> {
    let toml_str = match name {
        "one-dark" => ONE_DARK_TOML,
        "catppuccin-latte" => CATPPUCCIN_LATTE_TOML,
        "catppuccin-frappe" => CATPPUCCIN_FRAPPE_TOML,
        "catppuccin-macchiato" => CATPPUCCIN_MACCHIATO_TOML,
        "catppuccin-mocha" => CATPPUCCIN_MOCHA_TOML,
        "dracula" => DRACULA_TOML,
        _ => return None,
    };
    Some(parse_builtin(toml_str))
}

/// The default theme (`catppuccin-macchiato`).
pub fn default_theme() -> Theme {
    parse_builtin(CATPPUCCIN_MACCHIATO_TOML)
}

/// Parses a `ThemeFile` from a TOML string (used for loading custom themes).
pub fn parse_theme_toml(toml_str: &str) -> Result<Theme, ThemeError> {
    let file: ThemeFile = toml::from_str(toml_str)?;
    file.into_theme()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_builtins_parse() {
        for name in BUILTIN_THEMES {
            assert!(
                builtin_theme(name).is_some(),
                "built-in theme '{name}' failed to parse"
            );
        }
    }

    #[test]
    fn test_unknown_builtin_returns_none() {
        assert!(builtin_theme("nonexistent").is_none());
    }

    #[test]
    fn test_hex_parse_valid() {
        let color = parse_hex("bg", "#24273a").unwrap();
        assert_eq!(color, Rgb::new(0x24, 0x27, 0x3a));
    }

    #[test]
    fn test_hex_parse_valid_no_hash() {
        let color = parse_hex("bg", "24273a").unwrap();
        assert_eq!(color, Rgb::new(0x24, 0x27, 0x3a));
    }

    #[test]
    fn test_hex_parse_invalid_length() {
        assert!(matches!(
            parse_hex("bg", "#abc"),
            Err(ThemeError::InvalidColor { .. })
        ));
    }

    #[test]
    fn test_hex_parse_invalid_chars() {
        assert!(matches!(
            parse_hex("bg", "#zzzzzz"),
            Err(ThemeError::InvalidColor { .. })
        ));
    }

    #[test]
    fn test_default_theme_is_macchiato() {
        let theme = default_theme();
        // bg = #24273a
        assert_eq!(theme.bg, Rgb::new(0x24, 0x27, 0x3a));
    }

    #[test]
    fn cursor_colors_default_to_fg_and_bg() {
        // A theme TOML with no cursor_* keys should fall back to fg/bg so every
        // theme has a high-contrast cursor without per-theme tuning.
        let toml = r##"
name = "test"
bg = "#000000"
fg = "#ffffff"
fg_dim = "#888888"
accent = "#0000ff"
green = "#00ff00"
red = "#ff0000"
yellow = "#ffff00"
purple = "#ff00ff"
orange = "#ff8800"
status_bg = "#222222"
"##;
        let theme = parse_theme_toml(toml).unwrap();
        assert_eq!(theme.cursor_bg, theme.fg);
        assert_eq!(theme.cursor_fg, theme.bg);
    }

    #[test]
    fn cursor_colors_can_be_overridden_in_toml() {
        let toml = r##"
name = "test"
bg = "#000000"
fg = "#ffffff"
fg_dim = "#888888"
accent = "#0000ff"
green = "#00ff00"
red = "#ff0000"
yellow = "#ffff00"
purple = "#ff00ff"
orange = "#ff8800"
status_bg = "#222222"
cursor_bg = "#abcdef"
cursor_fg = "#123456"
"##;
        let theme = parse_theme_toml(toml).unwrap();
        assert_eq!(theme.cursor_bg, Rgb::new(0xab, 0xcd, 0xef));
        assert_eq!(theme.cursor_fg, Rgb::new(0x12, 0x34, 0x56));
    }

    #[test]
    fn themes_differing_only_in_cursor_color_are_unequal() {
        // Regression: the GTK frontend's old hand-written `palette_eq` omitted
        // `cursor_bg`/`cursor_fg`, so a `/theme` change to only the cursor color
        // never triggered a chrome reload. The derived `PartialEq` compares all
        // fields, so two palettes differing only in the cursor must compare
        // unequal (and a frontend will reload on the change).
        let base = default_theme();
        let mut cursor_changed = base.clone();
        cursor_changed.cursor_bg = Rgb::new(1, 2, 3);
        assert_ne!(base, cursor_changed);

        let mut cursor_fg_changed = base.clone();
        cursor_fg_changed.cursor_fg = Rgb::new(4, 5, 6);
        assert_ne!(base, cursor_fg_changed);

        // An identical clone compares equal (no spurious reloads).
        assert_eq!(base, base.clone());
    }

    #[test]
    fn all_builtins_have_cursor_colors() {
        // Built-in themes omit the cursor_* keys, so they should default to
        // fg/bg — never an unset/zeroed value.
        for name in BUILTIN_THEMES {
            let theme = builtin_theme(name).unwrap();
            assert_eq!(theme.cursor_bg, theme.fg, "{name} cursor_bg should be fg");
            assert_eq!(theme.cursor_fg, theme.bg, "{name} cursor_fg should be bg");
        }
    }
}
