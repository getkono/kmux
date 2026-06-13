//! Client configuration file and theme resolution.
//!
//! Frontend-agnostic: [`resolve_theme`] returns a toolkit-neutral
//! [`crate::theme::Theme`] which each frontend converts to its own color type.

use serde::{Deserialize, Serialize};
use tracing::error;

use crate::appearance::{self, Appearance, CellAdjust, FontFeature};
use crate::theme::{self, Theme};

/// Top-level kmux configuration file (`~/.config/kmux/config.toml`).
///
/// The file is optional; its absence is not an error.
#[derive(Debug, Deserialize, Serialize, Default)]
pub struct KmuxConfig {
    /// Theme ID: a built-in name or a custom theme filename (without `.toml`)
    /// located in `~/.config/kmux/themes/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Legacy GUI font as a Pango font-description string (e.g.
    /// `"JetBrains Mono 12"`). Deprecated in favor of the structured
    /// `font_family`/`font_size` keys below, but still honored: it seeds the
    /// resolved [`Appearance`]'s family + size when the structured keys are
    /// absent. The structured keys win per-field when both are set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    /// Primary (regular) font family, e.g. `"JetBrains Mono"`.
    #[serde(skip_serializing_if = "Option::is_none", alias = "font-family")]
    pub font_family: Option<String>,
    /// Explicit family for bold text (synthesized from `font_family` if unset).
    #[serde(skip_serializing_if = "Option::is_none", alias = "font-family-bold")]
    pub font_family_bold: Option<String>,
    /// Explicit family for italic text (synthesized from `font_family` if unset).
    #[serde(skip_serializing_if = "Option::is_none", alias = "font-family-italic")]
    pub font_family_italic: Option<String>,
    /// Explicit family for bold-italic text (synthesized if unset).
    #[serde(
        skip_serializing_if = "Option::is_none",
        alias = "font-family-bold-italic"
    )]
    pub font_family_bold_italic: Option<String>,
    /// Font size in points.
    #[serde(skip_serializing_if = "Option::is_none", alias = "font-size")]
    pub font_size: Option<f32>,
    /// Optional named style/face for the regular font (e.g. `"Medium"`).
    #[serde(skip_serializing_if = "Option::is_none", alias = "font-style")]
    pub font_style: Option<String>,
    /// OpenType feature settings, e.g. `["ss01", "-liga", "cv01=2"]`. See
    /// [`FontFeature`] for the accepted token forms.
    #[serde(skip_serializing_if = "Option::is_none", alias = "font-feature")]
    pub font_feature: Option<Vec<String>>,
    /// Horizontal cell-size adjustment: a bare number adds pixels, a trailing
    /// `%` scales (e.g. `"2"` or `"10%"`).
    #[serde(skip_serializing_if = "Option::is_none", alias = "adjust-cell-width")]
    pub adjust_cell_width: Option<String>,
    /// Vertical cell-size adjustment (same form as `adjust_cell_width`).
    #[serde(skip_serializing_if = "Option::is_none", alias = "adjust-cell-height")]
    pub adjust_cell_height: Option<String>,
    /// Whether the inner-pane cursor blinks. `None` (the default) means blink;
    /// set `false` to keep the cursor steady regardless of what the program
    /// requests (DECSCUSR `blinking_*` / DEC mode 12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_blink: Option<bool>,
    /// Whether to warn before opening a kmux GUI *inside* a kmux-managed shell
    /// (issue #73). `None`/`true` warns; the "start anyway from now on" choice
    /// persists `false` here to silence it.
    #[serde(skip_serializing_if = "Option::is_none", alias = "warn-nested")]
    pub warn_nested: Option<bool>,
}

/// Load `config.toml`, returning defaults if it is missing or unparseable.
pub fn load() -> KmuxConfig {
    load_config_file().unwrap_or_default()
}

/// Persist `cfg` to `<config_dir>/config.toml`, creating the directory if
/// needed. Used by the GUI preferences window.
pub fn save(cfg: &KmuxConfig) -> anyhow::Result<()> {
    let dir = kmux_protocol::dirs::config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");
    std::fs::write(&path, toml::to_string_pretty(cfg)?)?;
    Ok(())
}

/// Default GUI font (a Pango font-description string) used when neither the
/// `--font` flag nor the config `font` key is set.
pub const DEFAULT_FONT: &str = "monospace 11";

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

/// Resolve the active GUI font (a Pango font-description string).
///
/// Priority order (mirrors [`resolve_theme`]):
/// 1. `--font` CLI flag (`cli_font`)
/// 2. `font` key in `~/.config/kmux/config.toml`
/// 3. [`DEFAULT_FONT`]
///
/// Blank values are ignored so they fall through to the next source.
pub fn resolve_font(cli_font: Option<&str>) -> String {
    if let Some(font) = cli_font.map(str::trim).filter(|s| !s.is_empty()) {
        return font.to_string();
    }
    if let Some(cfg) = load_config_file()
        && let Some(font) = cfg.font.as_deref().map(str::trim).filter(|s| !s.is_empty())
    {
        return font.to_string();
    }
    DEFAULT_FONT.to_string()
}

/// Resolve the active terminal [`Appearance`] (font family/size/style, OpenType
/// features, cell adjustments).
///
/// Each field follows the project's standard precedence:
/// 1. the structured key in `~/.config/kmux/config.toml`
///    (`font-family`, `font-size`, `font-feature`, `adjust-cell-*`, …);
/// 2. otherwise, for family + size, the legacy `font` Pango string (resolved via
///    [`resolve_font`], so `--font` and the config `font` key still apply);
/// 3. otherwise the [`appearance`] defaults.
///
/// `cli_font` is the `--font` flag, forwarded to [`resolve_font`] for the legacy
/// fallback (there are no per-field appearance CLI flags).
pub fn resolve_appearance(cli_font: Option<&str>) -> Appearance {
    // Legacy font string (CLI `--font` > config `font` > DEFAULT_FONT) seeds the
    // family/size when the structured keys are absent.
    let legacy = resolve_font(cli_font);
    let (legacy_family, legacy_size) = appearance::parse_legacy_font(&legacy);

    let cfg = load_config_file().unwrap_or_default();

    let trimmed = |o: Option<String>| o.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    let family = trimmed(cfg.font_family)
        .or(legacy_family)
        .unwrap_or_else(|| appearance::DEFAULT_FAMILY.to_string());
    let size_pt = cfg
        .font_size
        .filter(|s| *s > 0.0)
        .or(legacy_size)
        .unwrap_or(appearance::DEFAULT_SIZE_PT);

    let features = cfg
        .font_feature
        .map(|tokens| {
            tokens
                .iter()
                .filter_map(|t| FontFeature::parse(t))
                .collect()
        })
        .unwrap_or_default();
    let cell_width_adjust = cfg
        .adjust_cell_width
        .as_deref()
        .and_then(CellAdjust::parse)
        .unwrap_or_default();
    let cell_height_adjust = cfg
        .adjust_cell_height
        .as_deref()
        .and_then(CellAdjust::parse)
        .unwrap_or_default();

    Appearance {
        family,
        family_bold: trimmed(cfg.font_family_bold),
        family_italic: trimmed(cfg.font_family_italic),
        family_bold_italic: trimmed(cfg.font_family_bold_italic),
        size_pt,
        style: trimmed(cfg.font_style),
        features,
        cell_width_adjust,
        cell_height_adjust,
    }
}

/// Resolve whether the inner-pane cursor blinks.
///
/// Priority order (mirrors [`resolve_font`]):
/// 1. `--cursor-blink` CLI flag (`cli_value`)
/// 2. `cursor_blink` key in `~/.config/kmux/config.toml`
/// 3. `true` (blink by default, matching real terminals)
pub fn resolve_cursor_blink(cli_value: Option<bool>) -> bool {
    if let Some(value) = cli_value {
        return value;
    }
    if let Some(cfg) = load_config_file()
        && let Some(value) = cfg.cursor_blink
    {
        return value;
    }
    true
}

/// Whether the `kmux` entrypoint should warn before opening a GUI inside a
/// kmux-managed shell (issue #73). Defaults to `true`; the user's "start anyway
/// from now on" choice persists `warn_nested = false`.
pub fn warn_when_nested() -> bool {
    load_config_file()
        .and_then(|cfg| cfg.warn_nested)
        .unwrap_or(true)
}

/// Persist the nested-warning preference (issue #73), preserving the rest of the
/// config file. Used when the user picks "start anyway from now on".
pub fn set_warn_when_nested(value: bool) -> anyhow::Result<()> {
    let mut cfg = load();
    cfg.warn_nested = Some(value);
    save(&cfg)
}

/// Try to load a theme by name.
///
/// Looks up built-in themes first, then `<config_dir>/themes/<name>.toml`.
/// Returns `None` (and logs an error) if the theme cannot be found or parsed.
fn load_theme_spec(name: &str) -> Option<Theme> {
    if let Some(t) = theme::builtin_theme(name) {
        return Some(t);
    }

    let path = match kmux_protocol::dirs::config_dir() {
        Ok(dir) => dir.join("themes").join(format!("{name}.toml")),
        Err(e) => {
            error!("could not resolve config dir for theme '{name}': {e}");
            return None;
        }
    };
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

/// Load `<config_dir>/config.toml` if it exists.
///
/// Logs and returns `None` on parse errors; silently returns `None` if missing.
fn load_config_file() -> Option<KmuxConfig> {
    let path = match kmux_protocol::dirs::config_dir() {
        Ok(dir) => dir.join("config.toml"),
        Err(e) => {
            error!("could not resolve config dir: {e}");
            return None;
        }
    };
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::theme::Rgb;

    // Serialise all tests that mutate XDG_CONFIG_HOME to avoid races.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_resolve_cli_builtin() {
        let theme = resolve_theme(Some("dracula"));
        // dracula bg = #282a36
        assert_eq!(theme.bg, Rgb::new(0x28, 0x2a, 0x36));
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
        assert_eq!(theme.bg, Rgb::new(0x24, 0x27, 0x3a));
    }

    #[test]
    fn test_resolve_unknown_falls_back_to_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let theme = resolve_theme(Some("does-not-exist"));
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(theme.bg, Rgb::new(0x24, 0x27, 0x3a));
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
        assert_eq!(theme.bg, Rgb::new(0xff, 0x00, 0x00));
        assert_eq!(theme.fg, Rgb::new(0x00, 0xff, 0x00));
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

    #[test]
    fn font_cli_flag_wins() {
        assert_eq!(resolve_font(Some("JetBrains Mono 12")), "JetBrains Mono 12");
    }

    #[test]
    fn font_blank_flag_falls_through_to_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let font = resolve_font(Some("   "));
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(font, DEFAULT_FONT);
    }

    #[test]
    fn font_defaults_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let font = resolve_font(None);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(font, DEFAULT_FONT);
    }

    #[test]
    fn font_from_config_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let kmux_dir = tmp.path().join("kmux");
        std::fs::create_dir_all(&kmux_dir).unwrap();
        std::fs::write(kmux_dir.join("config.toml"), "font = \"Fira Code 13\"\n").unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let font = resolve_font(None);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(font, "Fira Code 13");
    }

    #[test]
    fn config_file_parses_font_field() {
        let cfg: KmuxConfig = toml::from_str(r#"font = "Fira Code 13""#).unwrap();
        assert_eq!(cfg.font.as_deref(), Some("Fira Code 13"));
    }

    #[test]
    fn cursor_blink_cli_flag_wins() {
        assert!(!resolve_cursor_blink(Some(false)));
        assert!(resolve_cursor_blink(Some(true)));
    }

    #[test]
    fn cursor_blink_defaults_to_true_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let blink = resolve_cursor_blink(None);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert!(blink, "the default cursor should blink");
    }

    #[test]
    fn cursor_blink_from_config_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let kmux_dir = tmp.path().join("kmux");
        std::fs::create_dir_all(&kmux_dir).unwrap();
        std::fs::write(kmux_dir.join("config.toml"), "cursor_blink = false\n").unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let blink = resolve_cursor_blink(None);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert!(!blink, "config cursor_blink=false should disable blinking");
    }

    #[test]
    fn config_file_parses_cursor_blink_field() {
        let cfg: KmuxConfig = toml::from_str("cursor_blink = false").unwrap();
        assert_eq!(cfg.cursor_blink, Some(false));
    }

    #[test]
    fn config_file_parses_warn_nested_field() {
        // Both snake_case (canonical) and the kebab alias parse (issue #73).
        let snake: KmuxConfig = toml::from_str("warn_nested = false").unwrap();
        assert_eq!(snake.warn_nested, Some(false));
        let kebab: KmuxConfig = toml::from_str("warn-nested = true").unwrap();
        assert_eq!(kebab.warn_nested, Some(true));
    }

    #[test]
    fn warn_nested_round_trips_through_save_format() {
        // The "start anyway from now on" choice (issue #73) must survive a
        // save → load cycle using the canonical key written by `config::save`.
        let cfg = KmuxConfig {
            warn_nested: Some(false),
            ..Default::default()
        };
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        assert!(
            serialized.contains("warn_nested = false"),
            "expected canonical key, got: {serialized}"
        );
        let back: KmuxConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(back.warn_nested, Some(false));
    }

    #[test]
    fn save_then_load_round_trips() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let cfg = KmuxConfig {
            theme: Some("dracula".into()),
            font: Some("Fira Code 13".into()),
            font_family: Some("JetBrains Mono".into()),
            font_size: Some(14.0),
            font_feature: Some(vec!["ss01".into(), "-liga".into()]),
            adjust_cell_height: Some("10%".into()),
            cursor_blink: Some(false),
            ..KmuxConfig::default()
        };
        let saved = save(&cfg);
        let loaded = load();
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        saved.unwrap();
        assert_eq!(loaded.theme.as_deref(), Some("dracula"));
        assert_eq!(loaded.font.as_deref(), Some("Fira Code 13"));
        assert_eq!(loaded.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(loaded.font_size, Some(14.0));
        assert_eq!(
            loaded.font_feature.as_deref(),
            Some(["ss01".to_string(), "-liga".to_string()].as_slice())
        );
        assert_eq!(loaded.adjust_cell_height.as_deref(), Some("10%"));
        assert_eq!(loaded.cursor_blink, Some(false));
    }

    #[test]
    fn config_parses_structured_font_keys() {
        let cfg: KmuxConfig = toml::from_str(
            r#"
font-family = "JetBrains Mono"
font-size = 13.5
font-feature = ["ss01", "-liga"]
adjust-cell-width = "5"
adjust-cell-height = "10%"
"#,
        )
        .unwrap();
        assert_eq!(cfg.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(cfg.font_size, Some(13.5));
        assert_eq!(
            cfg.font_feature.as_deref(),
            Some(["ss01".to_string(), "-liga".to_string()].as_slice())
        );
        assert_eq!(cfg.adjust_cell_width.as_deref(), Some("5"));
        assert_eq!(cfg.adjust_cell_height.as_deref(), Some("10%"));
    }

    #[test]
    fn config_accepts_snake_case_font_keys() {
        // snake_case is the canonical kmux form (matching `cursor_blink`); the
        // kebab-case aliases above are for Ghostty-config compatibility.
        let cfg: KmuxConfig =
            toml::from_str("font_family = \"Fira Code\"\nfont_size = 12\n").unwrap();
        assert_eq!(cfg.font_family.as_deref(), Some("Fira Code"));
        assert_eq!(cfg.font_size, Some(12.0));
    }

    #[test]
    fn appearance_defaults_when_nothing_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let a = resolve_appearance(None);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(a.family, appearance::DEFAULT_FAMILY);
        assert_eq!(a.size_pt, appearance::DEFAULT_SIZE_PT);
        assert!(a.features.is_empty());
        assert_eq!(a.cell_width_adjust, CellAdjust::default());
    }

    #[test]
    fn appearance_seeds_family_size_from_legacy_font() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let kmux_dir = tmp.path().join("kmux");
        std::fs::create_dir_all(&kmux_dir).unwrap();
        std::fs::write(kmux_dir.join("config.toml"), "font = \"Fira Code 13\"\n").unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let a = resolve_appearance(None);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(a.family, "Fira Code");
        assert_eq!(a.size_pt, 13.0);
    }

    #[test]
    fn appearance_structured_keys_win_over_legacy_font() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let kmux_dir = tmp.path().join("kmux");
        std::fs::create_dir_all(&kmux_dir).unwrap();
        std::fs::write(
            kmux_dir.join("config.toml"),
            "font = \"Fira Code 13\"\nfont-family = \"JetBrains Mono\"\nfont-size = 16\nfont-feature = [\"ss01\"]\n",
        )
        .unwrap();
        unsafe { std::env::set_var("XDG_CONFIG_HOME", tmp.path()) };
        let a = resolve_appearance(None);
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
        assert_eq!(a.family, "JetBrains Mono");
        assert_eq!(a.size_pt, 16.0);
        assert_eq!(a.feature_string().as_deref(), Some("ss01=1"));
    }
}
