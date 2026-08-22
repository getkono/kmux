//! Toolkit-neutral terminal appearance/font settings.
//!
//! Like [`crate::theme`], this is the single source of truth for the font and
//! cell-geometry settings each frontend applies at its render leaf: `kmux-gtk`
//! builds a `pango::FontDescription` + an `AttrFontFeatures` attribute list, the
//! Swift app builds an `NSFont` + CoreText feature settings — both from the same
//! [`crate::appearance::Appearance`]. Nothing here depends on a UI toolkit.
//!
//! The settings track [Ghostty's config reference][ghostty] key names
//! (`font-family`, `font-size`, `font-feature`, `adjust-cell-*`, …); see
//! [`crate::config`] for how they are read from `config.toml`.
//!
//! [ghostty]: https://ghostty.org/docs/config/reference

/// Default font family used when none is configured. A generic alias that every
/// toolkit resolves to its platform monospace face, so the grid never falls back
/// to a proportional font.
pub const DEFAULT_FAMILY: &str = "monospace";

/// Default font size in points. Matches the legacy `DEFAULT_FONT` (`"monospace 11"`).
pub const DEFAULT_SIZE_PT: f32 = 11.0;

/// A parsed OpenType feature setting, e.g. `ss01` (on), `-liga` (off), or
/// `cv01=2` (alternate). Drives Pango's `AttrFontFeatures` and CoreText's
/// feature settings.
///
/// Note: kmux renders glyph-by-glyph per cell, so multi-glyph features that span
/// cells (ligatures via `liga`/`calt`) do not visibly fire. Per-glyph features
/// (stylistic sets `ss0x`, `zero`, `onum`, …) work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontFeature {
    /// The OpenType feature tag (e.g. `"calt"`, `"ss01"`).
    pub tag: String,
    /// The feature value: `0` disables, `1` enables, higher selects an alternate.
    pub value: u32,
}

impl FontFeature {
    /// Parse one feature token. Accepts `"tag"` (enable), `"+tag"` (enable),
    /// `"-tag"` (disable), and `"tag=N"` / `"tag N"` (explicit value). Returns
    /// `None` for an empty/tagless token.
    pub fn parse(token: &str) -> Option<Self> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        if let Some(tag) = token.strip_prefix('-') {
            let tag = tag.trim();
            return (!tag.is_empty()).then(|| Self {
                tag: tag.to_string(),
                value: 0,
            });
        }
        let token = token.strip_prefix('+').unwrap_or(token).trim();
        // `tag=value` or `tag value`. Try `=` first so `"zero = 1"` (spaces
        // around the equals) splits on the equals, not the leading space.
        let split = token
            .split_once('=')
            .or_else(|| token.split_once(char::is_whitespace));
        if let Some((tag, val)) = split {
            let tag = tag.trim();
            let value: u32 = val.trim().parse().ok()?;
            return (!tag.is_empty()).then(|| Self {
                tag: tag.to_string(),
                value,
            });
        }
        Some(Self {
            tag: token.to_string(),
            value: 1,
        })
    }

    /// Format as a harfbuzz/CSS feature setting (`"tag=value"`), the form Pango's
    /// `AttrFontFeatures` and CoreText both accept.
    pub fn to_setting(&self) -> String {
        format!("{}={}", self.tag, self.value)
    }
}

/// A cell-dimension adjustment, mirroring Ghostty's `adjust-cell-*`
/// percentage-or-absolute form: a bare number adds pixels, a trailing `%` scales.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CellAdjust {
    /// Add a fixed number of (logical) pixels.
    Pixels(f32),
    /// Scale by a percentage delta (e.g. `10.0` → +10%).
    Percent(f32),
}

impl Default for CellAdjust {
    fn default() -> Self {
        Self::Pixels(0.0)
    }
}

impl CellAdjust {
    /// Parse the config form: `"2"` → `Pixels(2)`, `"10%"` → `Percent(10)`.
    /// Returns `None` for an empty/unparseable value.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        if let Some(pct) = s.strip_suffix('%') {
            pct.trim().parse::<f32>().ok().map(CellAdjust::Percent)
        } else {
            s.parse::<f32>().ok().map(CellAdjust::Pixels)
        }
    }

    /// Apply this adjustment to a measured base dimension.
    pub fn apply(self, base: f64) -> f64 {
        match self {
            Self::Pixels(px) => base + px as f64,
            Self::Percent(p) => base * (1.0 + p as f64 / 100.0),
        }
    }
}

/// Resolved, toolkit-neutral terminal appearance. Each frontend converts this to
/// its own font/metrics types at the render leaf.
#[derive(Debug, Clone, PartialEq)]
pub struct Appearance {
    /// Primary (regular) font family.
    pub family: String,
    /// Explicit family for bold text; `None` synthesizes bold from `family`.
    pub family_bold: Option<String>,
    /// Explicit family for italic text; `None` synthesizes italic from `family`.
    pub family_italic: Option<String>,
    /// Explicit family for bold-italic text; `None` synthesizes from `family`.
    pub family_bold_italic: Option<String>,
    /// Font size in points.
    pub size_pt: f32,
    /// Optional named style/face for the regular font (e.g. `"Medium"`).
    pub style: Option<String>,
    /// OpenType feature settings applied to all text.
    pub features: Vec<FontFeature>,
    /// Horizontal cell-size adjustment.
    pub cell_width_adjust: CellAdjust,
    /// Vertical cell-size adjustment.
    pub cell_height_adjust: CellAdjust,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            family: DEFAULT_FAMILY.to_string(),
            family_bold: None,
            family_italic: None,
            family_bold_italic: None,
            size_pt: DEFAULT_SIZE_PT,
            style: None,
            features: Vec::new(),
            cell_width_adjust: CellAdjust::default(),
            cell_height_adjust: CellAdjust::default(),
        }
    }
}

impl Appearance {
    /// Format the feature list as a single harfbuzz/CSS feature string
    /// (`"calt=0,ss01=1"`) for Pango's `AttrFontFeatures`. Returns `None` when no
    /// features are configured.
    pub fn feature_string(&self) -> Option<String> {
        if self.features.is_empty() {
            return None;
        }
        Some(
            self.features
                .iter()
                .map(FontFeature::to_setting)
                .collect::<Vec<_>>()
                .join(","),
        )
    }
}

/// Best-effort parse of a legacy Pango font-description string (e.g.
/// `"JetBrains Mono 12"`) into a `(family, size_pt)` pair, used to seed an
/// [`Appearance`] when the structured `font-family`/`font-size` keys are absent.
///
/// Pango puts the size last, so the trailing numeric token is the size and the
/// remainder is the family (any embedded style words are left on the family:
/// GTK's Pango re-parses them; other frontends fall back to their default face
/// if the resulting name doesn't resolve).
pub fn parse_legacy_font(s: &str) -> (Option<String>, Option<f32>) {
    let s = s.trim();
    if s.is_empty() {
        return (None, None);
    }
    let (family_part, size) = match s.rsplit_once(' ') {
        Some((head, tail)) => match tail.trim().parse::<f32>() {
            Ok(size) => (head.trim(), Some(size)),
            Err(_) => (s, None),
        },
        None => (s, None),
    };
    let family = (!family_part.is_empty()).then(|| family_part.to_string());
    (family, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_parse_enable_disable_value() {
        assert_eq!(
            FontFeature::parse("ss01"),
            Some(FontFeature {
                tag: "ss01".into(),
                value: 1
            })
        );
        assert_eq!(
            FontFeature::parse("-liga"),
            Some(FontFeature {
                tag: "liga".into(),
                value: 0
            })
        );
        assert_eq!(
            FontFeature::parse("+calt"),
            Some(FontFeature {
                tag: "calt".into(),
                value: 1
            })
        );
        assert_eq!(
            FontFeature::parse("cv01=2"),
            Some(FontFeature {
                tag: "cv01".into(),
                value: 2
            })
        );
        assert_eq!(
            FontFeature::parse("zero = 1"),
            Some(FontFeature {
                tag: "zero".into(),
                value: 1
            })
        );
        assert_eq!(FontFeature::parse("   "), None);
        assert_eq!(FontFeature::parse("-"), None);
    }

    #[test]
    fn feature_string_joins_settings() {
        let a = Appearance {
            features: vec![
                FontFeature {
                    tag: "calt".into(),
                    value: 0,
                },
                FontFeature {
                    tag: "ss01".into(),
                    value: 1,
                },
            ],
            ..Appearance::default()
        };
        assert_eq!(a.feature_string().as_deref(), Some("calt=0,ss01=1"));
        assert_eq!(Appearance::default().feature_string(), None);
    }

    #[test]
    fn cell_adjust_parse_pixels_and_percent() {
        assert_eq!(CellAdjust::parse("5"), Some(CellAdjust::Pixels(5.0)));
        assert_eq!(CellAdjust::parse("-2"), Some(CellAdjust::Pixels(-2.0)));
        assert_eq!(CellAdjust::parse("10%"), Some(CellAdjust::Percent(10.0)));
        assert_eq!(CellAdjust::parse(" 12 % "), Some(CellAdjust::Percent(12.0)));
        assert_eq!(CellAdjust::parse("abc"), None);
        assert_eq!(CellAdjust::parse(""), None);
    }

    #[test]
    fn cell_adjust_apply() {
        assert_eq!(CellAdjust::Pixels(2.0).apply(10.0), 12.0);
        assert_eq!(CellAdjust::Percent(10.0).apply(10.0), 11.0);
        assert_eq!(CellAdjust::default().apply(10.0), 10.0);
    }

    #[test]
    fn legacy_font_parse_family_and_size() {
        assert_eq!(
            parse_legacy_font("JetBrains Mono 12"),
            (Some("JetBrains Mono".into()), Some(12.0))
        );
        assert_eq!(
            parse_legacy_font("monospace"),
            (Some("monospace".into()), None)
        );
        assert_eq!(
            parse_legacy_font("Fira Code 13.5"),
            (Some("Fira Code".into()), Some(13.5))
        );
        assert_eq!(parse_legacy_font("   "), (None, None));
    }
}
