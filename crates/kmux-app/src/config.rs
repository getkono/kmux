//! Client configuration file and theme resolution.
//!
//! Frontend-agnostic: [`crate::config::resolve_theme`] returns a toolkit-neutral
//! [`crate::theme::Theme`] which each frontend converts to its own color type.

use kmux_sys::dirs::Dirs;
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::appearance::{self, Appearance, CellAdjust, FontFeature};
use crate::theme::{self, Theme};

/// Terminal renderer backend.
///
/// Selected via the `renderer` key in `~/.config/kmux/config.toml`. It is a
/// configuration key rather than a CLI flag on purpose: a kmux GUI client is
/// effectively a singleton process (one app instance, many windows), so a flag
/// passed to a *second* launch would route to the already-running process and
/// never reach the live renderer. Reading it from config at process start is the
/// only place the choice reliably applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RendererKind {
    /// CPU renderer (Cairo on GTK, CoreText on macOS). The default.
    #[default]
    Cairo,
    /// GPU renderer (wgpu on GTK, Metal on macOS) via the shared `kmux-render`
    /// crate. `wgpu` is accepted as an alias for backward compatibility.
    #[serde(alias = "wgpu")]
    Gpu,
}

impl RendererKind {
    /// The stable lowercase token used at the FFI boundary and in debug output
    /// (`"cairo"` / `"gpu"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cairo => "cairo",
            Self::Gpu => "gpu",
        }
    }

    /// Parse a token (`"cairo"`, `"gpu"`, or the legacy `"wgpu"`), case-insensitive.
    /// Returns `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cairo" => Some(Self::Cairo),
            "gpu" | "wgpu" => Some(Self::Gpu),
            _ => None,
        }
    }
}

/// Top-level kmux configuration file (`~/.config/kmux/config.toml`).
///
/// The file is optional; its absence is not an error.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
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
    /// Whether the performance HUD shows the live network-latency and rendering
    /// FPS counters (issue #61). `None` (the default) shows them; set `false` to
    /// hide them, which also skips their per-frame computation to save power.
    #[serde(skip_serializing_if = "Option::is_none", alias = "perf-counters")]
    pub perf_counters: Option<bool>,
    /// Terminal renderer backend: `"cairo"` (CPU, default) or `"gpu"` (the
    /// `kmux-render` GPU path; `"wgpu"` is accepted as an alias). `None` defaults
    /// to `cairo`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renderer: Option<RendererKind>,
}

/// Load `config.toml`, returning defaults if it is missing or unparseable.
pub fn load() -> KmuxConfig {
    load_config_file().unwrap_or_default()
}

/// Load `config.toml` from an explicit [`Dirs`], returning defaults if it is
/// missing or unparseable.
///
/// The isolation seam for anything that reads config: a test builds a
/// `Dirs::rooted(tmp)` instead of overwriting `XDG_CONFIG_HOME` in the running
/// process (docs/testing.md R3).
pub fn load_from(dirs: &Dirs) -> KmuxConfig {
    read_config_file(dirs).unwrap_or_default()
}

/// Persist `cfg` to `<config_dir>/config.toml`, creating the directory if
/// needed. Used by the GUI preferences window.
pub fn save(cfg: &KmuxConfig) -> anyhow::Result<()> {
    save_to(&Dirs::from_env()?, cfg)
}

/// Persist `cfg` under an explicit [`Dirs`]. See [`load_from`].
pub fn save_to(dirs: &Dirs, cfg: &KmuxConfig) -> anyhow::Result<()> {
    let dir = dirs.config_dir()?;
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
    resolve_theme_from(&load(), cli_theme)
}

/// [`resolve_theme`] against an already-loaded config.
///
/// Pure for a built-in theme name; a *custom* name still reads
/// `<config_dir>/themes/<name>.toml`, which is the one part of theme resolution
/// that cannot be a value.
pub fn resolve_theme_from(cfg: &KmuxConfig, cli_theme: Option<&str>) -> Theme {
    resolve_theme_inner(cfg, cli_theme, load_theme_spec)
}

/// [`resolve_theme_from`] against an explicit [`Dirs`], so a test can exercise
/// the custom-theme-file path without touching the environment.
pub fn resolve_theme_in(dirs: &Dirs, cfg: &KmuxConfig, cli_theme: Option<&str>) -> Theme {
    resolve_theme_inner(cfg, cli_theme, |name| load_theme_spec_in(dirs, name))
}

/// The precedence rule, with theme lookup injected.
///
/// A CLI name is authoritative: if it does not resolve, fall back to the default
/// rather than to the configured theme — otherwise `--theme` with a typo would
/// silently look like it worked.
fn resolve_theme_inner(
    cfg: &KmuxConfig,
    cli_theme: Option<&str>,
    lookup: impl Fn(&str) -> Option<Theme>,
) -> Theme {
    if let Some(name) = cli_theme {
        // `lookup` already logged any error.
        return lookup(name).unwrap_or_else(theme::default_theme);
    }
    if let Some(name) = cfg.theme.as_deref()
        && let Some(t) = lookup(name)
    {
        return t;
    }
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
    resolve_font_from(&load(), cli_font)
}

/// [`resolve_font`] against an already-loaded config. Pure.
pub fn resolve_font_from(cfg: &KmuxConfig, cli_font: Option<&str>) -> String {
    if let Some(font) = cli_font.map(str::trim).filter(|s| !s.is_empty()) {
        return font.to_string();
    }
    if let Some(font) = cfg.font.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
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
    resolve_appearance_from(&load(), cli_font)
}

/// [`resolve_appearance`] against an already-loaded config. Pure.
///
/// Taking the config as a value also removes a double read: the previous form
/// loaded `config.toml` once here and again inside `resolve_font`.
pub fn resolve_appearance_from(cfg: &KmuxConfig, cli_font: Option<&str>) -> Appearance {
    // Legacy font string (CLI `--font` > config `font` > DEFAULT_FONT) seeds the
    // family/size when the structured keys are absent.
    let legacy = resolve_font_from(cfg, cli_font);
    let (legacy_family, legacy_size) = appearance::parse_legacy_font(&legacy);

    let cfg = cfg.clone();

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
    resolve_cursor_blink_from(&load(), cli_value)
}

/// [`resolve_cursor_blink`] against an already-loaded config. Pure.
pub fn resolve_cursor_blink_from(cfg: &KmuxConfig, cli_value: Option<bool>) -> bool {
    cli_value.or(cfg.cursor_blink).unwrap_or(true)
}

/// Resolve the terminal renderer backend.
///
/// Reads the `renderer` key in `~/.config/kmux/config.toml`, defaulting to
/// [`RendererKind::Cairo`]. This is config-only by design (see [`RendererKind`]):
/// a singleton GUI client cannot honor a per-launch flag, so the choice lives in
/// config and is read once at process start.
pub fn resolve_renderer() -> RendererKind {
    resolve_renderer_from(&load())
}

/// [`resolve_renderer`] against an already-loaded config. Pure.
pub fn resolve_renderer_from(cfg: &KmuxConfig) -> RendererKind {
    cfg.renderer.unwrap_or_default()
}

/// Whether the `kmux` entrypoint should warn before opening a GUI inside a
/// kmux-managed shell (issue #73). Defaults to `true`; the user's "start anyway
/// from now on" choice persists `warn_nested = false`.
pub fn warn_when_nested() -> bool {
    warn_when_nested_from(&load())
}

/// [`warn_when_nested`] against an already-loaded config. Pure.
pub fn warn_when_nested_from(cfg: &KmuxConfig) -> bool {
    cfg.warn_nested.unwrap_or(true)
}

/// Persist the nested-warning preference (issue #73), preserving the rest of the
/// config file. Used when the user picks "start anyway from now on".
pub fn set_warn_when_nested(value: bool) -> anyhow::Result<()> {
    let mut cfg = load();
    cfg.warn_nested = Some(value);
    save(&cfg)
}

/// Resolve whether the performance HUD shows the network-latency + FPS counters
/// (issue #61). `perf_counters` key in `~/.config/kmux/config.toml`; defaults to
/// `true`. Hiding them also disables their per-frame calculation.
pub fn resolve_perf_counters() -> bool {
    resolve_perf_counters_from(&load())
}

/// [`resolve_perf_counters`] against an already-loaded config. Pure.
pub fn resolve_perf_counters_from(cfg: &KmuxConfig) -> bool {
    cfg.perf_counters.unwrap_or(true)
}

/// Try to load a theme by name.
///
/// Looks up built-in themes first, then `<config_dir>/themes/<name>.toml`.
/// Returns `None` (and logs an error) if the theme cannot be found or parsed.
fn load_theme_spec(name: &str) -> Option<Theme> {
    // Built-ins resolve without touching the filesystem, so the common case
    // never needs a config dir at all.
    if let Some(t) = theme::builtin_theme(name) {
        return Some(t);
    }
    match Dirs::from_env() {
        Ok(dirs) => load_theme_spec_in(&dirs, name),
        Err(e) => {
            error!("could not resolve config dir for theme '{name}': {e}");
            None
        }
    }
}

/// [`load_theme_spec`] against an explicit [`Dirs`]. See [`load_from`].
fn load_theme_spec_in(dirs: &Dirs, name: &str) -> Option<Theme> {
    if let Some(t) = theme::builtin_theme(name) {
        return Some(t);
    }

    let path = match dirs.config_dir() {
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
    match Dirs::from_env() {
        Ok(dirs) => read_config_file(&dirs),
        Err(e) => {
            error!("could not resolve config dir: {e}");
            None
        }
    }
}

fn read_config_file(dirs: &Dirs) -> Option<KmuxConfig> {
    let path = match dirs.config_dir() {
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
    use super::*;
    use crate::theme::Rgb;

    /// catppuccin-macchiato — the default theme's background.
    const DEFAULT_BG: Rgb = Rgb::new(0x24, 0x27, 0x3a);

    /// A config with nothing set, i.e. every resolver on its default path.
    fn empty_config() -> KmuxConfig {
        KmuxConfig::default()
    }

    fn config_from(toml_src: &str) -> KmuxConfig {
        toml::from_str(toml_src).expect("fixture config parses")
    }

    // ── Theme ────────────────────────────────────────────────────────────────

    #[test]
    fn theme_cli_flag_selects_a_builtin() {
        let theme = resolve_theme_from(&empty_config(), Some("dracula"));
        assert_eq!(theme.bg, Rgb::new(0x28, 0x2a, 0x36));
    }

    #[test]
    fn theme_defaults_when_neither_cli_nor_config_names_one() {
        assert_eq!(resolve_theme_from(&empty_config(), None).bg, DEFAULT_BG);
    }

    #[test]
    fn theme_cli_flag_wins_over_the_config_key() {
        let cfg = config_from(r#"theme = "dracula""#);
        let theme = resolve_theme_from(&cfg, Some("nord"));
        assert_ne!(
            theme.bg,
            Rgb::new(0x28, 0x2a, 0x36),
            "--theme must override the configured theme, not be overridden by it"
        );
    }

    #[test]
    fn theme_comes_from_the_config_key_when_no_cli_flag() {
        let cfg = config_from(r#"theme = "dracula""#);
        assert_eq!(
            resolve_theme_from(&cfg, None).bg,
            Rgb::new(0x28, 0x2a, 0x36)
        );
    }

    #[test]
    fn an_unknown_cli_theme_falls_back_to_the_default_not_to_the_config() {
        // Precedence detail worth pinning: a typo in --theme must not silently
        // hand back the configured theme, or the flag would look like it worked.
        let cfg = config_from(r#"theme = "dracula""#);
        assert_eq!(
            resolve_theme_from(&cfg, Some("does-not-exist")).bg,
            DEFAULT_BG
        );
    }

    #[test]
    fn an_unknown_config_theme_falls_back_to_the_default() {
        let cfg = config_from(r#"theme = "does-not-exist""#);
        assert_eq!(resolve_theme_from(&cfg, None).bg, DEFAULT_BG);
    }

    #[test]
    fn a_custom_theme_file_is_loaded_from_the_config_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dirs = Dirs::rooted(tmp.path());
        let themes = dirs.config_dir().expect("config dir").join("themes");
        std::fs::create_dir_all(&themes).expect("create themes dir");
        std::fs::write(
            themes.join("my-theme.toml"),
            r##"
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
"##,
        )
        .expect("write theme");

        let theme = resolve_theme_in(&dirs, &empty_config(), Some("my-theme"));
        assert_eq!(theme.bg, Rgb::new(0xff, 0x00, 0x00));
        assert_eq!(theme.fg, Rgb::new(0x00, 0xff, 0x00));
    }

    #[test]
    fn a_builtin_name_wins_over_a_same_named_file_and_needs_no_config_dir() {
        // Built-ins short-circuit before any filesystem access, which is why the
        // common case works even when the config dir cannot be resolved.
        let dirs = Dirs::rooted(std::path::Path::new("/nonexistent-kmux-test-root"));
        let theme = resolve_theme_in(&dirs, &empty_config(), Some("dracula"));
        assert_eq!(theme.bg, Rgb::new(0x28, 0x2a, 0x36));
    }

    // ── Font ─────────────────────────────────────────────────────────────────

    #[test]
    fn font_cli_flag_wins() {
        let cfg = config_from(r#"font = "Fira Code 13""#);
        assert_eq!(
            resolve_font_from(&cfg, Some("JetBrains Mono 12")),
            "JetBrains Mono 12"
        );
    }

    #[test]
    fn font_blank_cli_flag_falls_through_to_the_config() {
        let cfg = config_from(r#"font = "Fira Code 13""#);
        assert_eq!(resolve_font_from(&cfg, Some("   ")), "Fira Code 13");
    }

    #[test]
    fn font_comes_from_the_config_when_no_cli_flag() {
        let cfg = config_from(r#"font = "Fira Code 13""#);
        assert_eq!(resolve_font_from(&cfg, None), "Fira Code 13");
    }

    #[test]
    fn font_defaults_when_nothing_is_set() {
        assert_eq!(resolve_font_from(&empty_config(), None), DEFAULT_FONT);
        assert_eq!(resolve_font_from(&empty_config(), Some("  ")), DEFAULT_FONT);
    }

    #[test]
    fn a_blank_config_font_falls_through_to_the_default() {
        let cfg = config_from(r#"font = "   ""#);
        assert_eq!(resolve_font_from(&cfg, None), DEFAULT_FONT);
    }

    // ── Cursor blink ─────────────────────────────────────────────────────────

    #[test]
    fn cursor_blink_cli_flag_wins_over_the_config() {
        let cfg = config_from("cursor_blink = false");
        assert!(resolve_cursor_blink_from(&cfg, Some(true)));
        let cfg = config_from("cursor_blink = true");
        assert!(!resolve_cursor_blink_from(&cfg, Some(false)));
    }

    #[test]
    fn cursor_blink_comes_from_the_config_when_no_cli_flag() {
        let cfg = config_from("cursor_blink = false");
        assert!(!resolve_cursor_blink_from(&cfg, None));
    }

    #[test]
    fn cursor_blink_defaults_to_true() {
        assert!(
            resolve_cursor_blink_from(&empty_config(), None),
            "real terminals blink by default"
        );
    }

    // ── Renderer, nested warning, perf counters ──────────────────────────────

    #[test]
    fn renderer_defaults_to_cairo() {
        assert_eq!(resolve_renderer_from(&empty_config()), RendererKind::Cairo);
    }

    #[test]
    fn renderer_comes_from_the_config() {
        assert_eq!(
            resolve_renderer_from(&config_from(r#"renderer = "gpu""#)),
            RendererKind::Gpu
        );
        assert_eq!(
            resolve_renderer_from(&config_from(r#"renderer = "wgpu""#)),
            RendererKind::Gpu,
            "`wgpu` is kept as a backward-compatible alias"
        );
        assert_eq!(
            resolve_renderer_from(&config_from(r#"renderer = "cairo""#)),
            RendererKind::Cairo
        );
    }

    #[test]
    fn warn_when_nested_defaults_to_true_and_is_silenced_by_the_config() {
        assert!(warn_when_nested_from(&empty_config()));
        assert!(!warn_when_nested_from(&config_from("warn_nested = false")));
    }

    #[test]
    fn perf_counters_default_to_true_and_are_hidden_by_the_config() {
        assert!(resolve_perf_counters_from(&empty_config()));
        assert!(!resolve_perf_counters_from(&config_from(
            "perf_counters = false"
        )));
    }

    // ── Appearance ───────────────────────────────────────────────────────────

    #[test]
    fn appearance_defaults_when_nothing_is_set() {
        let a = resolve_appearance_from(&empty_config(), None);
        assert_eq!(a.family, appearance::DEFAULT_FAMILY);
        assert_eq!(a.size_pt, appearance::DEFAULT_SIZE_PT);
        assert!(a.features.is_empty());
    }

    #[test]
    fn appearance_seeds_family_and_size_from_the_legacy_font_string() {
        let a = resolve_appearance_from(&empty_config(), Some("Fira Code 14"));
        assert_eq!(a.family, "Fira Code");
        assert_eq!(a.size_pt, 14.0);
    }

    #[test]
    fn appearance_structured_keys_win_over_the_legacy_font() {
        let cfg = config_from(
            r#"
font = "Fira Code 14"
font-family = "JetBrains Mono"
font-size = 11.5
"#,
        );
        let a = resolve_appearance_from(&cfg, None);
        assert_eq!(a.family, "JetBrains Mono");
        assert_eq!(a.size_pt, 11.5);
    }

    #[test]
    fn appearance_falls_back_per_field_rather_than_all_or_nothing() {
        // Only the family is set structurally; the size must still come from the
        // legacy string rather than resetting to the default.
        let cfg = config_from(
            r#"
font = "Fira Code 14"
font-family = "JetBrains Mono"
"#,
        );
        let a = resolve_appearance_from(&cfg, None);
        assert_eq!(a.family, "JetBrains Mono");
        assert_eq!(a.size_pt, 14.0, "size should survive from the legacy font");
    }

    #[test]
    fn appearance_ignores_a_non_positive_font_size() {
        let cfg = config_from("font-size = 0.0");
        assert_eq!(
            resolve_appearance_from(&cfg, None).size_pt,
            appearance::DEFAULT_SIZE_PT
        );
    }

    #[test]
    fn appearance_parses_font_features_and_cell_adjustments() {
        let cfg = config_from(
            r#"
font-feature = ["ss01", "-liga"]
adjust-cell-width = "2"
adjust-cell-height = "10%"
"#,
        );
        let a = resolve_appearance_from(&cfg, None);
        assert_eq!(a.features.len(), 2);
        assert_ne!(a.cell_width_adjust, CellAdjust::default());
        assert_ne!(a.cell_height_adjust, CellAdjust::default());
    }

    // ── File format ──────────────────────────────────────────────────────────

    #[test]
    fn config_parses_theme_and_font_fields() {
        let cfg = config_from(r#"theme = "dracula""#);
        assert_eq!(cfg.theme.as_deref(), Some("dracula"));
        let cfg = config_from(r#"font = "Fira Code 13""#);
        assert_eq!(cfg.font.as_deref(), Some("Fira Code 13"));
    }

    #[test]
    fn an_empty_config_leaves_every_field_unset() {
        let cfg = config_from("");
        assert!(cfg.theme.is_none());
        assert!(cfg.font.is_none());
        assert!(cfg.cursor_blink.is_none());
        assert!(cfg.renderer.is_none());
    }

    #[test]
    fn structured_font_keys_accept_both_kebab_and_snake_case() {
        let kebab = config_from(
            r#"
font-family = "JetBrains Mono"
font-size = 12.0
adjust-cell-width = "2"
warn-nested = true
perf-counters = false
"#,
        );
        let snake = config_from(
            r#"
font_family = "JetBrains Mono"
font_size = 12.0
adjust_cell_width = "2"
warn_nested = true
perf_counters = false
"#,
        );
        assert_eq!(kebab.font_family, snake.font_family);
        assert_eq!(kebab.font_size, snake.font_size);
        assert_eq!(kebab.adjust_cell_width, snake.adjust_cell_width);
        assert_eq!(kebab.warn_nested, snake.warn_nested);
        assert_eq!(kebab.perf_counters, snake.perf_counters);
    }

    #[test]
    fn save_then_load_round_trips_through_an_isolated_config_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dirs = Dirs::rooted(tmp.path());

        let cfg = KmuxConfig {
            theme: Some("dracula".into()),
            font: Some("Fira Code 13".into()),
            warn_nested: Some(false),
            perf_counters: Some(false),
            renderer: Some(RendererKind::Gpu),
            ..KmuxConfig::default()
        };
        save_to(&dirs, &cfg).expect("save");

        let read_back = load_from(&dirs);
        assert_eq!(read_back.theme.as_deref(), Some("dracula"));
        assert_eq!(read_back.font.as_deref(), Some("Fira Code 13"));
        assert_eq!(read_back.warn_nested, Some(false));
        assert_eq!(read_back.perf_counters, Some(false));
        assert_eq!(read_back.renderer, Some(RendererKind::Gpu));
    }

    #[test]
    fn loading_a_missing_config_yields_defaults_rather_than_failing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = load_from(&Dirs::rooted(tmp.path()));
        assert!(cfg.theme.is_none(), "a missing config file is not an error");
    }

    #[test]
    fn loading_an_unparseable_config_yields_defaults_rather_than_failing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dirs = Dirs::rooted(tmp.path());
        let dir = dirs.config_dir().expect("config dir");
        std::fs::write(dir.join("config.toml"), "this is not = valid = toml").expect("write");
        let cfg = load_from(&dirs);
        assert!(
            cfg.theme.is_none(),
            "a corrupt config must degrade to defaults, not take down the client"
        );
    }
}
