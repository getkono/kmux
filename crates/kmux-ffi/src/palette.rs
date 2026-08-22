//! Colours, the theme, and the appearance knobs a preferences pane sets.

use super::*;

/// An RGB palette color.
#[derive(uniffi::Record)]
pub struct FfiColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl From<Rgb> for FfiColor {
    fn from(c: Rgb) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

/// The active toolkit-neutral palette.
#[derive(uniffi::Record)]
pub struct FfiTheme {
    pub bg: FfiColor,
    pub fg: FfiColor,
    pub fg_dim: FfiColor,
    pub accent: FfiColor,
    pub green: FfiColor,
    pub red: FfiColor,
    pub yellow: FfiColor,
    pub purple: FfiColor,
    pub orange: FfiColor,
    pub status_bg: FfiColor,
    pub cursor_bg: FfiColor,
    pub cursor_fg: FfiColor,
}

impl From<&Theme> for FfiTheme {
    fn from(t: &Theme) -> Self {
        Self {
            bg: t.bg.into(),
            fg: t.fg.into(),
            fg_dim: t.fg_dim.into(),
            accent: t.accent.into(),
            green: t.green.into(),
            red: t.red.into(),
            yellow: t.yellow.into(),
            purple: t.purple.into(),
            orange: t.orange.into(),
            status_bg: t.status_bg.into(),
            cursor_bg: t.cursor_bg.into(),
            cursor_fg: t.cursor_fg.into(),
        }
    }
}

/// Whether a [`FfiCellAdjust`] adds pixels or scales by a percentage.
#[derive(uniffi::Enum)]
pub enum FfiCellAdjustKind {
    Pixels,
    Percent,
}

/// A cell-dimension adjustment (mirrors [`CellAdjust`]). `Pixels` adds `value`
/// logical pixels; `Percent` scales by `value`% (e.g. `10.0` → +10%).
#[derive(uniffi::Record)]
pub struct FfiCellAdjust {
    pub kind: FfiCellAdjustKind,
    pub value: f32,
}

impl From<&CellAdjust> for FfiCellAdjust {
    fn from(a: &CellAdjust) -> Self {
        match a {
            CellAdjust::Pixels(v) => Self {
                kind: FfiCellAdjustKind::Pixels,
                value: *v,
            },
            CellAdjust::Percent(v) => Self {
                kind: FfiCellAdjustKind::Percent,
                value: *v,
            },
        }
    }
}

/// The active toolkit-neutral terminal appearance (font + cell geometry). The
/// Swift frontend builds an `NSFont` + CoreText feature settings from this.
#[derive(uniffi::Record)]
pub struct FfiAppearance {
    pub family: String,
    pub family_bold: Option<String>,
    pub family_italic: Option<String>,
    pub family_bold_italic: Option<String>,
    pub size_pt: f32,
    pub style: Option<String>,
    /// OpenType feature settings as harfbuzz tag strings (`"tag=value"`).
    pub features: Vec<String>,
    pub cell_width_adjust: FfiCellAdjust,
    pub cell_height_adjust: FfiCellAdjust,
}

impl From<&Appearance> for FfiAppearance {
    fn from(a: &Appearance) -> Self {
        Self {
            family: a.family.clone(),
            family_bold: a.family_bold.clone(),
            family_italic: a.family_italic.clone(),
            family_bold_italic: a.family_bold_italic.clone(),
            size_pt: a.size_pt,
            style: a.style.clone(),
            features: a
                .features
                .iter()
                .map(kmux_app::appearance::FontFeature::to_setting)
                .collect(),
            cell_width_adjust: (&a.cell_width_adjust).into(),
            cell_height_adjust: (&a.cell_height_adjust).into(),
        }
    }
}
