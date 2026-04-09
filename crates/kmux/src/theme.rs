use ratatui::style::Color;
use serde::Deserialize;

const ONE_DARK_TOML: &str = include_str!("../../../themes/one-dark.toml");
const CATPPUCCIN_LATTE_TOML: &str = include_str!("../../../themes/catppuccin-latte.toml");
const CATPPUCCIN_FRAPPE_TOML: &str = include_str!("../../../themes/catppuccin-frappe.toml");
const CATPPUCCIN_MACCHIATO_TOML: &str = include_str!("../../../themes/catppuccin-macchiato.toml");
const CATPPUCCIN_MOCHA_TOML: &str = include_str!("../../../themes/catppuccin-mocha.toml");
const DRACULA_TOML: &str = include_str!("../../../themes/dracula.toml");

/// Runtime color palette used throughout the TUI.
#[derive(Debug, Clone)]
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub fg_dim: Color,
    pub accent: Color,
    pub green: Color,
    pub red: Color,
    pub yellow: Color,
    pub purple: Color,
    pub orange: Color,
    pub status_bg: Color,
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
}

#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("invalid hex color '{value}' in field '{field}'")]
    InvalidColor { field: &'static str, value: String },
    #[error("failed to parse theme TOML: {0}")]
    Toml(#[from] toml::de::Error),
}

fn parse_hex(field: &'static str, s: &str) -> Result<Color, ThemeError> {
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
    Ok(Color::Rgb(r, g, b))
}

impl ThemeFile {
    fn into_theme(self) -> Result<Theme, ThemeError> {
        Ok(Theme {
            bg: parse_hex("bg", &self.bg)?,
            fg: parse_hex("fg", &self.fg)?,
            fg_dim: parse_hex("fg_dim", &self.fg_dim)?,
            accent: parse_hex("accent", &self.accent)?,
            green: parse_hex("green", &self.green)?,
            red: parse_hex("red", &self.red)?,
            yellow: parse_hex("yellow", &self.yellow)?,
            purple: parse_hex("purple", &self.purple)?,
            orange: parse_hex("orange", &self.orange)?,
            status_bg: parse_hex("status_bg", &self.status_bg)?,
        })
    }
}

fn parse_builtin(toml_str: &str) -> Theme {
    toml::from_str::<ThemeFile>(toml_str)
        .expect("built-in theme TOML is malformed")
        .into_theme()
        .expect("built-in theme has invalid colors")
}

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

/// Maps a protocol `CellColor` to a ratatui `Color`.
pub fn cell_color(c: kmux_protocol::messages::CellColor) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_builtins_parse() {
        for name in &[
            "one-dark",
            "catppuccin-latte",
            "catppuccin-frappe",
            "catppuccin-macchiato",
            "catppuccin-mocha",
            "dracula",
        ] {
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
        assert_eq!(color, Color::Rgb(0x24, 0x27, 0x3a));
    }

    #[test]
    fn test_hex_parse_valid_no_hash() {
        let color = parse_hex("bg", "24273a").unwrap();
        assert_eq!(color, Color::Rgb(0x24, 0x27, 0x3a));
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
        assert_eq!(theme.bg, Color::Rgb(0x24, 0x27, 0x3a));
    }
}
