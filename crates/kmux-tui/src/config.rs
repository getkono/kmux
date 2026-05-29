use std::path::PathBuf;

use serde::Deserialize;
use tracing::error;

use crate::theme::{self, Theme};

/// Top-level kmux configuration file (`~/.config/kmux/config.toml`).
///
/// The file is optional; its absence is not an error.
#[derive(Debug, Deserialize, Default)]
pub struct KmuxConfig {
    /// Theme ID: a built-in name or a custom theme filename (without `.toml`)
    /// located in `~/.config/kmux/themes/`.
    pub theme: Option<String>,
}

/// Resolve the active theme.
///
/// Priority order:
/// 1. `--theme` CLI flag (`cli_theme`)
/// 2. `theme` key in `~/.config/kmux/config.toml`
/// 3. `default_theme()` (catppuccin-macchiato)
///
/// On any lookup failure the error is logged and the default is returned.
pub fn resolve_theme(cli_theme: Option<&str>) -> Theme {
    if let Some(name) = cli_theme {
        if let Some(t) = load_theme_spec(name) {
            return t;
        }
        // load_theme_spec already logged the error; fall through to default
        return theme::default_theme();
    }

    if let Some(cfg) = load_config_file()
        && let Some(name) = cfg.theme
        && let Some(t) = load_theme_spec(&name)
    {
        return t;
    }
    // load_theme_spec already logged any error; fall through to default

    theme::default_theme()
}

/// Try to load a theme by name.
///
/// Looks up built-in themes first, then `~/.config/kmux/themes/<name>.toml`.
/// Returns `None` (and logs an error) if the theme cannot be found or parsed.
fn load_theme_spec(name: &str) -> Option<Theme> {
    if let Some(t) = theme::builtin_theme(name) {
        return Some(t);
    }

    let path = themes_dir().join(format!("{name}.toml"));
    match std::fs::read_to_string(&path) {
        Ok(contents) => match theme::parse_theme_toml(&contents) {
            Ok(t) => return Some(t),
            Err(e) => {
                error!(
                    "failed to parse theme '{}' at {}: {e}",
                    name,
                    path.display()
                );
            }
        },
        Err(_) => {
            error!(
                "theme '{}' not found (checked built-ins and {}); falling back to default",
                name,
                path.display()
            );
        }
    }

    None
}

/// Load `~/.config/kmux/config.toml` if it exists.
///
/// Logs and returns `None` on parse errors; silently returns `None` if missing.
fn load_config_file() -> Option<KmuxConfig> {
    let path = config_path();
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(contents) => match toml::from_str::<KmuxConfig>(&contents) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                error!("failed to parse config '{}': {e}", path.display());
                None
            }
        },
        Err(e) => {
            error!("failed to read config '{}': {e}", path.display());
            None
        }
    }
}

/// Path to `~/.config/kmux/config.toml`, honouring `XDG_CONFIG_HOME`.
fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Path to `~/.config/kmux/themes/`, honouring `XDG_CONFIG_HOME`.
fn themes_dir() -> PathBuf {
    config_dir().join("themes")
}

/// Base kmux config directory (`~/.config/kmux` or `$XDG_CONFIG_HOME/kmux`).
fn config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .unwrap_or_else(|_| PathBuf::from(".config"))
        });
    base.join("kmux")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ratatui::style::Color;

    use super::*;

    // Serialise all tests that mutate XDG_CONFIG_HOME to avoid races.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_resolve_cli_builtin() {
        let theme = resolve_theme(Some("dracula"));
        // dracula bg = #282a36
        assert_eq!(theme.bg, Color::Rgb(0x28, 0x2a, 0x36));
    }

    #[test]
    fn test_resolve_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        // With XDG_CONFIG_HOME pointing somewhere empty, no config file exists.
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let theme = resolve_theme(None);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        // catppuccin-macchiato bg = #24273a
        assert_eq!(theme.bg, Color::Rgb(0x24, 0x27, 0x3a));
    }

    #[test]
    fn test_resolve_unknown_falls_back_to_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let theme = resolve_theme(Some("does-not-exist"));
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(theme.bg, Color::Rgb(0x24, 0x27, 0x3a));
    }

    #[test]
    fn test_resolve_custom_theme_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let themes_dir = tmp.path().join("kmux").join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();
        let toml = r##"
name      = "my-theme"
bg        = "#ff0000"
fg        = "#00ff00"
fg_dim    = "#0000ff"
accent    = "#ffffff"
green     = "#aaaaaa"
red       = "#bbbbbb"
yellow    = "#cccccc"
purple    = "#dddddd"
orange    = "#eeeeee"
status_bg = "#111111"
"##;
        std::fs::write(themes_dir.join("my-theme.toml"), toml).unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let theme = resolve_theme(Some("my-theme"));
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(theme.bg, Color::Rgb(0xff, 0x00, 0x00));
        assert_eq!(theme.fg, Color::Rgb(0x00, 0xff, 0x00));
    }

    #[test]
    fn test_config_file_theme_id() {
        let cfg: KmuxConfig = toml::from_str(r#"theme = "dracula""#).unwrap();
        assert_eq!(cfg.theme.as_deref(), Some("dracula"));
    }

    #[test]
    fn test_config_file_missing_theme_field() {
        let cfg: KmuxConfig = toml::from_str("").unwrap();
        assert!(cfg.theme.is_none());
    }
}
