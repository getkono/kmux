//! `RenderMetrics`: cell geometry + resolved font faces from an [`Appearance`].
//!
//! The toolkit-neutral analog of `kmux-gtk`'s `render::Metrics` and the Swift
//! `TerminalMetrics`: it measures a monospace cell from the configured font and
//! is the single cols/rows authority (via [`crate::geometry::CellMetrics`]), so
//! the resize path is shared rather than re-derived per toolkit. It also holds
//! the loaded font bytes per [`FaceStyle`] so the glyph atlas can rasterize.
//!
//! Font discovery uses `fontdb` (system fonts) so the crate needs no
//! fontconfig/CoreText build dependency. When no face resolves (e.g. a headless
//! CI box with no fonts) it falls back to approximate metrics so cols/rows and
//! layout still work — only glyph rasterization needs a real face.

use std::sync::Arc;

use fontdb::{Database, Family, Query, Stretch, Style, Weight};
use swash::FontRef;

use kmux_app::appearance::Appearance;

use crate::geometry::{CellMetrics, FaceStyle};

/// A resolved font face: the file bytes + the face index within the file.
#[derive(Clone)]
pub struct FontFace {
    data: Arc<Vec<u8>>,
    index: u32,
}

impl FontFace {
    /// Borrow this face as a swash [`FontRef`] for measuring / rasterizing.
    pub fn as_ref(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(&self.data, self.index as usize)
    }

    /// Build a face from embedded/static bytes (a single-face file → index 0).
    /// Used for the bundled glyph fallback font ([`crate::fallback`]).
    pub fn from_static(bytes: &'static [u8]) -> Self {
        FontFace {
            data: Arc::new(bytes.to_vec()),
            index: 0,
        }
    }
}

impl std::fmt::Debug for FontFace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontFace")
            .field("bytes", &self.data.len())
            .field("index", &self.index)
            .finish()
    }
}

/// The four faces a cell can use, each `None` when the system has no match.
#[derive(Clone, Debug, Default)]
pub struct Faces {
    /// Regular weight, upright.
    pub regular: Option<FontFace>,
    /// Bold weight, upright.
    pub bold: Option<FontFace>,
    /// Regular weight, italic.
    pub italic: Option<FontFace>,
    /// Bold weight, italic.
    pub bold_italic: Option<FontFace>,
}

impl Faces {
    /// The face for a [`FaceStyle`], falling back to the regular face when the
    /// requested variant is absent (so bold/italic still render *something*).
    pub fn face(&self, style: FaceStyle) -> Option<&FontFace> {
        let exact = match style {
            FaceStyle::Regular => &self.regular,
            FaceStyle::Bold => &self.bold,
            FaceStyle::Italic => &self.italic,
            FaceStyle::BoldItalic => &self.bold_italic,
        };
        exact.as_ref().or(self.regular.as_ref())
    }
}

/// Resolved cell metrics + faces for one appearance at one scale factor.
#[derive(Clone, Debug)]
pub struct RenderMetrics {
    cell: CellMetrics,
    ascent: f32,
    px_size: f32,
    scale: f32,
    faces: Faces,
}

impl RenderMetrics {
    /// Resolve metrics + faces from an [`Appearance`] at `scale` (logical→
    /// physical). `size_pt * scale` is the physical pixel em size.
    pub fn from_appearance(appearance: &Appearance, scale: f32) -> Self {
        let px_size = appearance.size_pt * scale;
        let mut db = Database::new();
        db.load_system_fonts();

        let regular = resolve(&db, &appearance.family, Weight::NORMAL, Style::Normal);
        let bold = resolve(
            &db,
            appearance
                .family_bold
                .as_deref()
                .unwrap_or(&appearance.family),
            Weight::BOLD,
            Style::Normal,
        );
        let italic = resolve(
            &db,
            appearance
                .family_italic
                .as_deref()
                .unwrap_or(&appearance.family),
            Weight::NORMAL,
            Style::Italic,
        );
        let bold_italic = resolve(
            &db,
            appearance
                .family_bold_italic
                .as_deref()
                .unwrap_or(&appearance.family),
            Weight::BOLD,
            Style::Italic,
        );

        let faces = Faces {
            regular,
            bold,
            italic,
            bold_italic,
        };

        let (cell_w_base, cell_h_base, ascent) = measure(faces.regular.as_ref(), px_size);
        let cell_w = appearance.cell_width_adjust.apply(cell_w_base as f64) as f32;
        let cell_h = appearance.cell_height_adjust.apply(cell_h_base as f64) as f32;

        Self {
            cell: CellMetrics::new(cell_w.max(1.0), cell_h.max(1.0)),
            ascent,
            px_size,
            scale,
            faces,
        }
    }

    /// The cell geometry (cell box + rule/cursor positions).
    pub fn cell(&self) -> &CellMetrics {
        &self.cell
    }

    /// Baseline distance from the cell top, in physical px (for glyph placement).
    pub fn ascent(&self) -> f32 {
        self.ascent
    }

    /// Physical pixel em size the faces are rasterized at.
    pub fn px_size(&self) -> f32 {
        self.px_size
    }

    /// The device scale factor these metrics were built for.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The resolved font faces (for the glyph atlas).
    pub fn faces(&self) -> &Faces {
        &self.faces
    }

    /// Map a content-area pixel size to `(cols, rows)`. Delegates to the cell
    /// geometry — the single cols/rows authority. Always at least `1×1`.
    pub fn cols_rows(&self, w_px: i32, h_px: i32) -> (u16, u16) {
        self.cell.cols_rows(w_px, h_px)
    }
}

/// fontdb family for a configured name (`"monospace"` → the generic).
fn family_for(name: &str) -> Family<'_> {
    if name.eq_ignore_ascii_case("monospace") {
        Family::Monospace
    } else {
        Family::Name(name)
    }
}

/// Resolve a face, always falling back to the generic monospace family.
fn resolve(db: &Database, family: &str, weight: Weight, style: Style) -> Option<FontFace> {
    let query = Query {
        families: &[family_for(family), Family::Monospace],
        weight,
        stretch: Stretch::Normal,
        style,
    };
    let id = db.query(&query)?;
    db.with_face_data(id, |bytes, index| FontFace {
        data: Arc::new(bytes.to_vec()),
        index,
    })
}

/// Measure `(cell_w, cell_h, ascent)` in physical px from the regular face, or
/// approximate from `px_size` when no face is available.
fn measure(regular: Option<&FontFace>, px_size: f32) -> (f32, f32, f32) {
    if let Some(face) = regular
        && let Some(font) = face.as_ref()
    {
        let m = font.metrics(&[]);
        let upem = (m.units_per_em as f32).max(1.0);
        let s = px_size / upem;
        let ascent = m.ascent * s;
        let descent = m.descent.abs() * s;
        let leading = m.leading.max(0.0) * s;
        let cell_h = (ascent + descent + leading).max(1.0);

        // Monospace: every glyph shares an advance; measure a stable one.
        let gm = font.glyph_metrics(&[]).scale(px_size);
        let cell_w = ['M', '0', 'x']
            .into_iter()
            .map(|c| font.charmap().map(c))
            .find(|&g| g != 0)
            .map(|g| gm.advance_width(g))
            .filter(|w| *w > 0.0)
            .unwrap_or(px_size * 0.6);

        (cell_w, cell_h, ascent)
    } else {
        // No font available: approximate a typical monospace cell.
        (px_size * 0.6, px_size * 1.2, px_size * 0.95)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_app::appearance::CellAdjust;

    #[test]
    fn from_appearance_yields_positive_cell() {
        let m = RenderMetrics::from_appearance(&Appearance::default(), 1.0);
        assert!(m.cell().cell_w > 0.0);
        assert!(m.cell().cell_h > 0.0);
        assert!(m.ascent() > 0.0);
        assert_eq!(m.px_size(), Appearance::default().size_pt);
    }

    #[test]
    fn cell_width_adjust_is_applied() {
        // The +N px adjust shifts cell_w by exactly N regardless of the resolved
        // font (or the no-font fallback), so this holds on any machine/CI.
        let base = RenderMetrics::from_appearance(&Appearance::default(), 1.0);
        let adjusted = RenderMetrics::from_appearance(
            &Appearance {
                cell_width_adjust: CellAdjust::Pixels(4.0),
                ..Appearance::default()
            },
            1.0,
        );
        assert!((adjusted.cell().cell_w - base.cell().cell_w - 4.0).abs() < 0.01);
    }

    #[test]
    fn scale_grows_the_cell_proportionally() {
        let one = RenderMetrics::from_appearance(&Appearance::default(), 1.0);
        let two = RenderMetrics::from_appearance(&Appearance::default(), 2.0);
        let ratio = two.cell().cell_w / one.cell().cell_w;
        assert!(
            (1.9..=2.1).contains(&ratio),
            "2x scale should ~double the cell width, got {ratio}"
        );
        assert_eq!(two.px_size(), one.px_size() * 2.0);
    }

    #[test]
    fn cols_rows_delegates_to_cell_metrics() {
        let m = RenderMetrics::from_appearance(&Appearance::default(), 1.0);
        let cw = m.cell().cell_w;
        let ch = m.cell().cell_h;
        // Ten cells wide, five tall (plus a partial cell that floors away).
        let (cols, rows) = m.cols_rows((cw * 10.0 + cw * 0.4) as i32, (ch * 5.0 + ch * 0.4) as i32);
        assert_eq!((cols, rows), (10, 5));
    }
}
