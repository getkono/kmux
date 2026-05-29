//! TUI color palette: the ratatui-typed view of a `kmux_app` theme.
//!
//! kmux-app owns the toolkit-neutral palette ([`kmux_app::theme`], colors as
//! `Rgb`). This module mirrors it as `ratatui::style::Color`s for rendering and
//! owns the protocol-color → ratatui mapping. The conversion happens at this
//! boundary so the rest of `ui/` keeps working with `ratatui` colors directly.

use kmux_app::theme as app_theme;
use ratatui::style::Color;

/// Runtime color palette used throughout the TUI (ratatui colors).
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

fn rgb(c: app_theme::Rgb) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

impl From<app_theme::Theme> for Theme {
    fn from(t: app_theme::Theme) -> Self {
        Self {
            bg: rgb(t.bg),
            fg: rgb(t.fg),
            fg_dim: rgb(t.fg_dim),
            accent: rgb(t.accent),
            green: rgb(t.green),
            red: rgb(t.red),
            yellow: rgb(t.yellow),
            purple: rgb(t.purple),
            orange: rgb(t.orange),
            status_bg: rgb(t.status_bg),
        }
    }
}

/// The default theme (`catppuccin-macchiato`), as ratatui colors.
///
/// Test-only: real default resolution lives in `kmux_app::config::resolve_theme`
/// (which converts at the `main.rs` boundary). This binary crate otherwise has
/// no non-test caller, so it's gated to avoid a dead-code warning. Ungate once
/// kmux-tui gains a library target (P7) or a non-test caller appears.
#[cfg(test)]
pub fn default_theme() -> Theme {
    app_theme::default_theme().into()
}

/// Returns the named built-in theme as ratatui colors, or `None`.
pub fn builtin_theme(name: &str) -> Option<Theme> {
    app_theme::builtin_theme(name).map(Into::into)
}

/// Maps a protocol `CellColor` to a ratatui `Color`.
pub fn cell_color(c: kmux_protocol::messages::CellColor) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_bg_is_macchiato() {
        // catppuccin-macchiato bg = #24273a, converted to a ratatui Color::Rgb.
        assert_eq!(default_theme().bg, Color::Rgb(0x24, 0x27, 0x3a));
    }

    #[test]
    fn builtin_dracula_bg_converts() {
        // dracula bg = #282a36
        assert_eq!(
            builtin_theme("dracula").unwrap().bg,
            Color::Rgb(0x28, 0x2a, 0x36)
        );
    }

    #[test]
    fn builtin_unknown_is_none() {
        assert!(builtin_theme("nope").is_none());
    }
}
