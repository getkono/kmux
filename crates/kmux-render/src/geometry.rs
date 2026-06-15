//! Pure cell→quad geometry: turning a [`Frame`] into draw primitives.
//!
//! This is GPU-free and exhaustively unit-tested — the geometry is computed here
//! and the renderer ([`crate::renderer`]) only uploads it. It mirrors the draw
//! order of the CPU renderers it replaces:
//!
//! 1. **bg_quads** — every cell's background (opaque; spacers included so
//!    wide-char halves stay opaque).
//! 2. **glyphs** — each cell's glyph in its foreground (dim → reduced alpha).
//! 3. **overlay_quads** — underline/strikethrough rules, selection wash, the
//!    cursor (block fill / bar / underline / hollow outline), focus border, and
//!    the scroll-indicator background — in that emission order, so the cursor
//!    sits above the wash and the border above all.
//! 4. **overlay_glyphs** — the block cursor's glyph (in `cursor_fg`, over its
//!    fill) and the scroll-indicator text.
//!
//! The renderer draws the four lists in that order. Glyph quads carry only the
//! cell origin + char + face; the renderer places the rasterized bitmap within
//! the cell using the atlas bearing and the font baseline.
//!
//! The single definition of "which cell shows at (vr, vc)" lives in
//! [`for_each_displayed_cell`] (scrollback composited into the top rows while
//! scrolled) — [`crate::packed::encode_cells`] and the `Grid` render path both
//! go through it, so the two cell sources provably agree.

use kmux_app::theme::Theme;
use kmux_client::grid::{CellGrid, scrollback_display_row_at};
use kmux_protocol::messages::{CellAttrs, CellState, CursorShape};

use crate::color;
use crate::frame::{CellSource, CursorView, Frame, PaneView};
use crate::packed::{self, RenderCell};

/// Foreground alpha applied to dim cells (`CellAttrs::DIM`).
const DIM_ALPHA: f32 = 0.6;
/// Alpha of the translucent selection wash over selected cells.
const SELECTION_WASH_ALPHA: f32 = 0.3;
/// Thickness (physical px) of the focused-pane accent border.
const FOCUS_BORDER_THICKNESS: f32 = 1.5;

/// A solid-color rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolidQuad {
    /// Left edge (physical px).
    pub x: f32,
    /// Top edge (physical px).
    pub y: f32,
    /// Width (physical px).
    pub w: f32,
    /// Height (physical px).
    pub h: f32,
    /// Straight (non-premultiplied) RGBA in surface color space.
    pub color: [f32; 4],
}

/// A request to draw one glyph at a cell origin. The renderer resolves the
/// `(ch, style)` to an atlas entry and places the bitmap using the baseline +
/// the glyph's bearing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphQuad {
    /// Cell origin x (physical px).
    pub cell_x: f32,
    /// Cell origin y (physical px).
    pub cell_y: f32,
    /// The character to draw.
    pub ch: char,
    /// Which font face to use.
    pub style: FaceStyle,
    /// Glyph tint (RGBA; alpha < 1 for dim).
    pub color: [f32; 4],
}

/// The four font faces a cell can request (no synthetic blends here — face
/// selection/synthesis is the metrics/atlas layer's job).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaceStyle {
    /// Regular weight, upright.
    Regular,
    /// Bold weight, upright.
    Bold,
    /// Regular weight, italic/oblique.
    Italic,
    /// Bold weight, italic/oblique.
    BoldItalic,
}

impl FaceStyle {
    /// The face implied by a cell's bold/italic attribute bits.
    pub fn from_attrs(a: CellAttrs) -> Self {
        match (a.contains(CellAttrs::BOLD), a.contains(CellAttrs::ITALIC)) {
            (true, true) => FaceStyle::BoldItalic,
            (true, false) => FaceStyle::Bold,
            (false, true) => FaceStyle::Italic,
            (false, false) => FaceStyle::Regular,
        }
    }
}

/// Pixel geometry the quad math needs, independent of any font library so this
/// module stays in the wgpu-free, swash-free core. [`crate::metrics`] derives a
/// real one from a font; tests build one with [`CellMetrics::new`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    /// Cell advance width (physical px).
    pub cell_w: f32,
    /// Cell height / line height (physical px).
    pub cell_h: f32,
    /// Underline rule top, offset from the cell top (physical px).
    pub underline_y: f32,
    /// Underline rule thickness (physical px).
    pub underline_thickness: f32,
    /// Strikethrough rule top, offset from the cell top (physical px).
    pub strikethrough_y: f32,
    /// Strikethrough rule thickness (physical px).
    pub strikethrough_thickness: f32,
    /// Thickness of bar/underline cursors and the hollow-block outline (px).
    pub cursor_thickness: f32,
}

impl CellMetrics {
    /// Derive sensible rule/cursor geometry from the cell box alone. Real font
    /// metrics refine these in [`crate::metrics`]; this keeps the geometry
    /// testable without a font.
    pub fn new(cell_w: f32, cell_h: f32) -> Self {
        let t = (cell_h * 0.06).max(1.0);
        Self {
            cell_w,
            cell_h,
            underline_thickness: t,
            underline_y: (cell_h - t * 2.0).max(0.0),
            strikethrough_thickness: t,
            strikethrough_y: cell_h * 0.5,
            cursor_thickness: (cell_h * 0.1).max(1.0),
        }
    }

    /// Map a content-area pixel size to `(cols, rows)` — how many whole cells
    /// fit. The single cols/rows authority that feeds the resize path; both
    /// frontends route their geometry through this (the GTK draw/resize branch
    /// and, via the FFI, the Swift one), replacing the per-toolkit Pango /
    /// CoreText measurement. Always at least `1×1`.
    pub fn cols_rows(&self, w_px: i32, h_px: i32) -> (u16, u16) {
        let cols = (w_px.max(0) as f32 / self.cell_w.max(1.0)).floor().max(1.0);
        let rows = (h_px.max(0) as f32 / self.cell_h.max(1.0)).floor().max(1.0);
        (cols as u16, rows as u16)
    }
}

/// A displayed cell with final (palette-resolved) float colors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedCell {
    /// The cell's character.
    pub ch: char,
    /// Foreground RGBA in `[0,1]`.
    pub fg: [f32; 4],
    /// Background RGBA in `[0,1]`.
    pub bg: [f32; 4],
    /// Attribute bits.
    pub attrs: CellAttrs,
    /// Width code: `0` = spacer, `1` = normal, `2` = wide.
    pub width: u8,
}

/// The draw primitives for one frame, in draw order (see the module docs).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SceneGeometry {
    /// Opaque cell backgrounds (drawn first).
    pub bg_quads: Vec<SolidQuad>,
    /// Cell glyphs (drawn over backgrounds).
    pub glyphs: Vec<GlyphQuad>,
    /// Rules, selection wash, cursor, borders (drawn over glyphs).
    pub overlay_quads: Vec<SolidQuad>,
    /// Block-cursor glyph + scroll-indicator text (drawn last).
    pub overlay_glyphs: Vec<GlyphQuad>,
}

/// Visit every *displayed* cell of `grid` row-major, compositing scrollback into
/// the top rows when scrolled (identical to the GTK renderer and
/// [`scrollback_display_row_at`]). The one shared definition consumed by both
/// [`crate::packed::encode_cells`] and [`build_scene`]'s `Grid` path.
pub fn for_each_displayed_cell(grid: &CellGrid, mut f: impl FnMut(usize, usize, &CellState)) {
    let cols = grid.cols;
    let rows = grid.rows;
    let scroll_offset = grid.scroll_offset();
    let scrollback = grid.scrollback();
    let cells = grid.cells();
    let blank = CellState::default();

    for vr in 0..rows {
        let sb_row = if scroll_offset > 0 && vr < scroll_offset {
            scrollback_display_row_at(scrollback, cols, scroll_offset - 1 - vr)
        } else {
            None
        };
        for vc in 0..cols {
            let cell = if let Some((line_idx, col_start)) = sb_row {
                scrollback
                    .get(line_idx)
                    .and_then(|line| line.get(col_start + vc))
            } else if scroll_offset > 0 {
                vr.checked_sub(scroll_offset)
                    .and_then(|grid_row| cells.get(grid_row * cols + vc))
            } else {
                cells.get(vr * cols + vc)
            };
            f(vr, vc, cell.unwrap_or(&blank));
        }
    }
}

/// Resolve a grid [`CellState`] against the palette (handles `DEFAULT_*`).
pub fn resolve_grid_cell(cell: &CellState, palette: &Theme) -> ResolvedCell {
    let fg = if cell.attrs.contains(CellAttrs::DEFAULT_FG) {
        color::rgb(palette.fg)
    } else {
        color::cell_color(cell.fg)
    };
    let bg = if cell.attrs.contains(CellAttrs::DEFAULT_BG) {
        color::rgb(palette.bg)
    } else {
        color::cell_color(cell.bg)
    };
    ResolvedCell {
        ch: cell.c,
        fg,
        bg,
        attrs: cell.attrs,
        width: packed::cell_width(cell.attrs),
    }
}

/// Resolve a decoded packed [`RenderCell`] (colors already final).
pub fn resolve_packed_cell(rc: &RenderCell) -> ResolvedCell {
    ResolvedCell {
        ch: rc.ch,
        fg: color::rgba8(rc.fg),
        bg: color::rgba8(rc.bg),
        attrs: rc.attrs,
        width: rc.width,
    }
}

/// Build the full scene geometry for `frame` at the given cell metrics.
pub fn build_scene(frame: &Frame<'_>, m: &CellMetrics) -> SceneGeometry {
    let mut scene = SceneGeometry::default();
    for pane in &frame.panes {
        emit_pane(&mut scene, frame, pane, m);
    }
    scene
}

fn emit_pane(scene: &mut SceneGeometry, frame: &Frame<'_>, pane: &PaneView<'_>, m: &CellMetrics) {
    let ox = pane.col as f32 * m.cell_w;
    let oy = pane.row as f32 * m.cell_h;

    // Cells: backgrounds, glyphs, rules.
    match &pane.cells {
        CellSource::Grid(grid) => {
            for_each_displayed_cell(grid, |vr, vc, cell| {
                let rc = resolve_grid_cell(cell, frame.palette);
                emit_cell(scene, m, ox, oy, vr, vc, &rc);
            });
        }
        CellSource::Packed { cells, cols, rows } => {
            let cols = *cols as usize;
            for vr in 0..*rows as usize {
                for vc in 0..cols {
                    let rc = resolve_packed_cell(&packed::decode_at(cells, vr * cols + vc));
                    emit_cell(scene, m, ox, oy, vr, vc, &rc);
                }
            }
        }
    }

    // Selection wash (over glyphs, under the cursor).
    let wash = color::with_alpha(color::rgb(frame.palette.accent), SELECTION_WASH_ALPHA);
    for &(vr, c0, c1) in pane.selection {
        let x = ox + c0 as f32 * m.cell_w;
        let w = (c1 - c0 + 1) as f32 * m.cell_w;
        scene.overlay_quads.push(SolidQuad {
            x,
            y: oy + vr as f32 * m.cell_h,
            w,
            h: m.cell_h,
            color: wash,
        });
    }

    // Cursor (over the wash).
    if let Some(cv) = pane.cursor
        && cv.is_drawn(frame.blink_on)
    {
        emit_cursor(scene, frame, pane, m, ox, oy, &cv);
    }

    // Focus border (over everything in the pane).
    if frame.multi && pane.focused {
        emit_focus_border(scene, m, ox, oy, pane.cols, pane.rows, frame.palette.accent);
    }

    // Scroll-into-history indicator.
    if let Some(si) = pane.scroll {
        let (cols, rows) = pane.cells.dims();
        emit_scroll_indicator(scene, frame, m, ox, oy, cols, rows, si.offset, si.total);
    }
}

fn emit_cell(
    scene: &mut SceneGeometry,
    m: &CellMetrics,
    ox: f32,
    oy: f32,
    vr: usize,
    vc: usize,
    rc: &ResolvedCell,
) {
    let x = ox + vc as f32 * m.cell_w;
    let y = oy + vr as f32 * m.cell_h;

    // Background for every cell, spacers included (so wide-char halves are opaque).
    scene.bg_quads.push(SolidQuad {
        x,
        y,
        w: m.cell_w,
        h: m.cell_h,
        color: rc.bg,
    });

    // No glyph for spacers, hidden cells, or non-drawable characters.
    if rc.width == 0 || rc.attrs.contains(CellAttrs::HIDDEN) || !is_drawable(rc.ch) {
        return;
    }

    let mut fg = rc.fg;
    if rc.attrs.contains(CellAttrs::DIM) {
        fg = color::with_alpha(fg, fg[3] * DIM_ALPHA);
    }
    scene.glyphs.push(GlyphQuad {
        cell_x: x,
        cell_y: y,
        ch: rc.ch,
        style: FaceStyle::from_attrs(rc.attrs),
        color: fg,
    });

    // Rules span both halves of a wide char.
    let rule_w = if rc.width == 2 {
        m.cell_w * 2.0
    } else {
        m.cell_w
    };
    if rc.attrs.contains(CellAttrs::UNDERLINE) {
        scene.overlay_quads.push(SolidQuad {
            x,
            y: y + m.underline_y,
            w: rule_w,
            h: m.underline_thickness,
            color: rc.fg,
        });
    }
    if rc.attrs.contains(CellAttrs::STRIKETHROUGH) {
        scene.overlay_quads.push(SolidQuad {
            x,
            y: y + m.strikethrough_y,
            w: rule_w,
            h: m.strikethrough_thickness,
            color: rc.fg,
        });
    }
}

fn emit_cursor(
    scene: &mut SceneGeometry,
    frame: &Frame<'_>,
    pane: &PaneView<'_>,
    m: &CellMetrics,
    ox: f32,
    oy: f32,
    cv: &CursorView,
) {
    let (cols, rows) = pane.cells.dims();
    if cv.col >= cols || cv.row >= rows {
        return;
    }
    let x = ox + cv.col as f32 * m.cell_w;
    let y = oy + cv.row as f32 * m.cell_h;
    let cb = color::rgb(frame.palette.cursor_bg);

    match cv.shape {
        CursorShape::Block => {
            scene.overlay_quads.push(SolidQuad {
                x,
                y,
                w: m.cell_w,
                h: m.cell_h,
                color: cb,
            });
            // Redraw the covered glyph in cursor_fg, over the fill.
            let rc = resolved_cell_at(&pane.cells, frame.palette, cv.row, cv.col);
            if rc.width != 0 && !rc.attrs.contains(CellAttrs::HIDDEN) && is_drawable(rc.ch) {
                scene.overlay_glyphs.push(GlyphQuad {
                    cell_x: x,
                    cell_y: y,
                    ch: rc.ch,
                    style: FaceStyle::from_attrs(rc.attrs),
                    color: color::rgb(frame.palette.cursor_fg),
                });
            }
        }
        CursorShape::HollowBlock => {
            emit_outline(scene, x, y, m.cell_w, m.cell_h, m.cursor_thickness, cb)
        }
        CursorShape::Underline => scene.overlay_quads.push(SolidQuad {
            x,
            y: y + m.cell_h - m.cursor_thickness,
            w: m.cell_w,
            h: m.cursor_thickness,
            color: cb,
        }),
        CursorShape::Bar => scene.overlay_quads.push(SolidQuad {
            x,
            y,
            w: m.cursor_thickness,
            h: m.cell_h,
            color: cb,
        }),
        CursorShape::Hidden => {}
    }
}

fn resolved_cell_at(source: &CellSource<'_>, palette: &Theme, row: u16, col: u16) -> ResolvedCell {
    match source {
        CellSource::Grid(grid) => {
            let idx = row as usize * grid.cols + col as usize;
            let cell = grid.cells().get(idx).copied().unwrap_or_default();
            resolve_grid_cell(&cell, palette)
        }
        CellSource::Packed { cells, cols, .. } => resolve_packed_cell(&packed::decode_at(
            cells,
            row as usize * *cols as usize + col as usize,
        )),
    }
}

fn emit_outline(
    scene: &mut SceneGeometry,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    t: f32,
    color: [f32; 4],
) {
    scene.overlay_quads.push(SolidQuad {
        x,
        y,
        w,
        h: t,
        color,
    }); // top
    scene.overlay_quads.push(SolidQuad {
        x,
        y: y + h - t,
        w,
        h: t,
        color,
    }); // bottom
    scene.overlay_quads.push(SolidQuad {
        x,
        y,
        w: t,
        h,
        color,
    }); // left
    scene.overlay_quads.push(SolidQuad {
        x: x + w - t,
        y,
        w: t,
        h,
        color,
    }); // right
}

fn emit_focus_border(
    scene: &mut SceneGeometry,
    m: &CellMetrics,
    ox: f32,
    oy: f32,
    cols: u16,
    rows: u16,
    accent: kmux_app::theme::Rgb,
) {
    let w = cols as f32 * m.cell_w;
    let h = rows as f32 * m.cell_h;
    emit_outline(
        scene,
        ox,
        oy,
        w,
        h,
        FOCUS_BORDER_THICKNESS,
        color::rgb(accent),
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_scroll_indicator(
    scene: &mut SceneGeometry,
    frame: &Frame<'_>,
    m: &CellMetrics,
    ox: f32,
    oy: f32,
    cols: u16,
    rows: u16,
    offset: usize,
    total: usize,
) {
    if cols == 0 || rows == 0 {
        return;
    }
    let label = format!("[{offset}/{total}]");
    let len = label.chars().count() as u16;
    let start_col = cols.saturating_sub(len);
    let row = rows - 1;
    let x0 = ox + start_col as f32 * m.cell_w;
    let y = oy + row as f32 * m.cell_h;
    let w = len.min(cols) as f32 * m.cell_w;

    scene.overlay_quads.push(SolidQuad {
        x: x0,
        y,
        w,
        h: m.cell_h,
        color: color::rgb(frame.palette.status_bg),
    });
    let fg = color::rgb(frame.palette.fg);
    for (i, ch) in label.chars().enumerate() {
        if start_col as usize + i >= cols as usize {
            break;
        }
        scene.overlay_glyphs.push(GlyphQuad {
            cell_x: x0 + i as f32 * m.cell_w,
            cell_y: y,
            ch,
            style: FaceStyle::Regular,
            color: fg,
        });
    }
}

/// Whether a character has a drawable glyph (skip blanks and control codes).
fn is_drawable(c: char) -> bool {
    c != ' ' && c != '\0' && !c.is_control()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::ScrollIndicator;
    use kmux_app::theme::{Rgb, Theme};
    use kmux_protocol::messages::{CellColor, CursorState};

    fn theme() -> &'static Theme {
        static T: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();
        T.get_or_init(|| {
            let mut t = kmux_app::theme::default_theme();
            t.fg = Rgb::new(0xaa, 0xbb, 0xcc);
            t.bg = Rgb::new(0x11, 0x22, 0x33);
            t.accent = Rgb::new(0x40, 0x80, 0xc0);
            t.cursor_bg = Rgb::new(0xf0, 0xf0, 0xf0);
            t.cursor_fg = Rgb::new(0x01, 0x02, 0x03);
            t
        })
    }

    fn cell(c: char, attrs: u16) -> CellState {
        CellState {
            c,
            fg: CellColor::new(0xaa, 0xbb, 0xcc),
            bg: CellColor::new(0x10, 0x20, 0x30),
            attrs: CellAttrs(attrs),
        }
    }

    fn grid_with(cells: Vec<CellState>, rows: usize, cols: usize) -> CellGrid {
        let mut g = CellGrid::new(rows, cols);
        g.apply_snapshot(kmux_protocol::messages::GridSnapshot {
            rows: rows as u16,
            cols: cols as u16,
            cells,
            cursor: CursorState::default(),
            modes: kmux_protocol::messages::TermModes::EMPTY,
            history_total: 0,
            scrollback_base: 0,
            scrollback_tail: Vec::new(),
        });
        g
    }

    fn pane<'a>(grid: &'a CellGrid, sel: &'a [(u16, u16, u16)]) -> PaneView<'a> {
        PaneView {
            col: 0,
            row: 0,
            cols: grid.cols as u16,
            rows: grid.rows as u16,
            focused: true,
            cells: CellSource::Grid(grid),
            cursor: None,
            selection: sel,
            scroll: None,
        }
    }

    #[test]
    fn for_each_displayed_cell_visits_row_major() {
        let grid = grid_with(
            vec![cell('a', 0), cell('b', 0), cell('c', 0), cell('d', 0)],
            2,
            2,
        );
        let mut seen = Vec::new();
        for_each_displayed_cell(&grid, |vr, vc, c| seen.push((vr, vc, c.c)));
        assert_eq!(
            seen,
            vec![(0, 0, 'a'), (0, 1, 'b'), (1, 0, 'c'), (1, 1, 'd')]
        );
    }

    #[test]
    fn grid_and_packed_resolve_identically() {
        // Explicit colors + default flags both round-trip to the same floats.
        let explicit = cell('X', CellAttrs::BOLD);
        let t = theme();
        let from_grid = resolve_grid_cell(&explicit, t);
        let mut bytes = Vec::new();
        packed::encode_cell(&mut bytes, &explicit, t);
        let from_packed = resolve_packed_cell(&packed::decode_cell(&bytes));
        assert_eq!(from_grid, from_packed);

        let defaulted = cell(' ', CellAttrs::DEFAULT_FG | CellAttrs::DEFAULT_BG);
        let from_grid = resolve_grid_cell(&defaulted, t);
        let mut bytes = Vec::new();
        packed::encode_cell(&mut bytes, &defaulted, t);
        let from_packed = resolve_packed_cell(&packed::decode_cell(&bytes));
        assert_eq!(from_grid, from_packed);
        assert_eq!(from_grid.bg, color::rgb(t.bg));
    }

    #[test]
    fn one_glyph_cell_emits_bg_and_glyph() {
        let grid = grid_with(vec![cell('Z', 0)], 1, 1);
        let p = pane(&grid, &[]);
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let scene = build_scene(&frame, &CellMetrics::new(8.0, 16.0));
        assert_eq!(scene.bg_quads.len(), 1);
        assert_eq!(scene.glyphs.len(), 1);
        assert_eq!(
            scene.bg_quads[0],
            SolidQuad {
                x: 0.0,
                y: 0.0,
                w: 8.0,
                h: 16.0,
                color: color::cell_color(CellColor::new(0x10, 0x20, 0x30))
            }
        );
        assert_eq!(scene.glyphs[0].ch, 'Z');
        assert_eq!(scene.glyphs[0].style, FaceStyle::Regular);
    }

    #[test]
    fn blank_and_control_cells_have_no_glyph() {
        let grid = grid_with(vec![cell(' ', 0), cell('\u{7}', 0)], 1, 2);
        let p = pane(&grid, &[]);
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let scene = build_scene(&frame, &CellMetrics::new(8.0, 16.0));
        assert_eq!(scene.bg_quads.len(), 2, "both cells still get a background");
        assert_eq!(scene.glyphs.len(), 0, "space and control emit no glyph");
    }

    #[test]
    fn wide_char_and_spacer() {
        let grid = grid_with(
            vec![
                cell('世', CellAttrs::WIDE_CHAR),
                cell(' ', CellAttrs::WIDE_CHAR_SPACER),
            ],
            1,
            2,
        );
        let p = pane(&grid, &[]);
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let scene = build_scene(&frame, &CellMetrics::new(8.0, 16.0));
        assert_eq!(scene.bg_quads.len(), 2, "wide char + spacer both fill bg");
        assert_eq!(scene.glyphs.len(), 1, "only the wide char draws a glyph");
        assert_eq!(scene.glyphs[0].ch, '世');
    }

    #[test]
    fn dim_reduces_glyph_alpha() {
        let grid = grid_with(vec![cell('d', CellAttrs::DIM)], 1, 1);
        let p = pane(&grid, &[]);
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let scene = build_scene(&frame, &CellMetrics::new(8.0, 16.0));
        assert_eq!(scene.glyphs[0].color[3], DIM_ALPHA);
    }

    #[test]
    fn underline_and_strikethrough_emit_overlay_rules() {
        let grid = grid_with(
            vec![cell('u', CellAttrs::UNDERLINE | CellAttrs::STRIKETHROUGH)],
            1,
            1,
        );
        let p = pane(&grid, &[]);
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let scene = build_scene(&frame, &CellMetrics::new(8.0, 16.0));
        assert_eq!(scene.overlay_quads.len(), 2);
    }

    #[test]
    fn block_cursor_fills_cell_and_overdraws_glyph() {
        let grid = grid_with(vec![cell('K', 0)], 1, 1);
        let mut p = pane(&grid, &[]);
        p.cursor = Some(CursorView {
            col: 0,
            row: 0,
            shape: CursorShape::Block,
            blink: false,
            visible: true,
        });
        let t = theme();
        let frame = Frame::single(100, 100, 1.0, t, true, p);
        let scene = build_scene(&frame, &CellMetrics::new(8.0, 16.0));
        // One overlay solid (the block fill, cursor_bg) and one overlay glyph
        // (the char in cursor_fg).
        assert_eq!(scene.overlay_quads.len(), 1);
        assert_eq!(scene.overlay_quads[0].color, color::rgb(t.cursor_bg));
        assert_eq!(scene.overlay_glyphs.len(), 1);
        assert_eq!(scene.overlay_glyphs[0].color, color::rgb(t.cursor_fg));
        assert_eq!(scene.overlay_glyphs[0].ch, 'K');
    }

    #[test]
    fn bar_cursor_is_a_thin_left_quad() {
        let grid = grid_with(vec![cell('b', 0)], 1, 1);
        let mut p = pane(&grid, &[]);
        p.cursor = Some(CursorView {
            col: 0,
            row: 0,
            shape: CursorShape::Bar,
            blink: false,
            visible: true,
        });
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let m = CellMetrics::new(8.0, 16.0);
        let scene = build_scene(&frame, &m);
        assert_eq!(scene.overlay_quads.len(), 1);
        assert_eq!(scene.overlay_quads[0].w, m.cursor_thickness);
        assert_eq!(scene.overlay_quads[0].h, 16.0);
        assert!(scene.overlay_glyphs.is_empty());
    }

    #[test]
    fn hollow_cursor_is_four_outline_quads() {
        let grid = grid_with(vec![cell('h', 0)], 1, 1);
        let mut p = pane(&grid, &[]);
        p.cursor = Some(CursorView {
            col: 0,
            row: 0,
            shape: CursorShape::HollowBlock,
            blink: false,
            visible: true,
        });
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let scene = build_scene(&frame, &CellMetrics::new(8.0, 16.0));
        assert_eq!(scene.overlay_quads.len(), 4);
    }

    #[test]
    fn blinking_cursor_off_phase_draws_nothing() {
        let grid = grid_with(vec![cell('x', 0)], 1, 1);
        let mut p = pane(&grid, &[]);
        p.cursor = Some(CursorView {
            col: 0,
            row: 0,
            shape: CursorShape::Block,
            blink: true,
            visible: true,
        });
        let frame = Frame::single(100, 100, 1.0, theme(), false, p); // blink_on = false
        let scene = build_scene(&frame, &CellMetrics::new(8.0, 16.0));
        assert!(scene.overlay_quads.is_empty());
        assert!(scene.overlay_glyphs.is_empty());
    }

    #[test]
    fn selection_wash_spans_columns() {
        let grid = grid_with(vec![cell('a', 0), cell('b', 0), cell('c', 0)], 1, 3);
        let sel = [(0u16, 0u16, 1u16)];
        let p = pane(&grid, &sel);
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let m = CellMetrics::new(8.0, 16.0);
        let scene = build_scene(&frame, &m);
        let wash = scene.overlay_quads.last().unwrap();
        assert_eq!(wash.x, 0.0);
        assert_eq!(wash.w, 2.0 * m.cell_w, "cols 0..=1 inclusive => two cells");
        assert!(wash.color[3] < 1.0, "wash is translucent");
    }

    #[test]
    fn focus_border_only_when_multi() {
        let grid = grid_with(vec![cell('a', 0)], 1, 1);
        // Single pane: focused but not multi => no border.
        let frame = Frame::single(100, 100, 1.0, theme(), true, pane(&grid, &[]));
        assert!(
            build_scene(&frame, &CellMetrics::new(8.0, 16.0))
                .overlay_quads
                .is_empty()
        );

        // Multi + focused => four border quads.
        let mut frame = Frame::single(100, 100, 1.0, theme(), true, pane(&grid, &[]));
        frame.multi = true;
        assert_eq!(
            build_scene(&frame, &CellMetrics::new(8.0, 16.0))
                .overlay_quads
                .len(),
            4
        );
    }

    #[test]
    fn pane_origin_offsets_quads() {
        let grid = grid_with(vec![cell('a', 0)], 1, 1);
        let mut p = pane(&grid, &[]);
        p.col = 3;
        p.row = 2;
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let m = CellMetrics::new(8.0, 16.0);
        let scene = build_scene(&frame, &m);
        assert_eq!(scene.bg_quads[0].x, 3.0 * m.cell_w);
        assert_eq!(scene.bg_quads[0].y, 2.0 * m.cell_h);
    }

    #[test]
    fn scroll_indicator_emits_label() {
        let grid = grid_with(vec![cell('a', 0); 20], 2, 10);
        let mut p = pane(&grid, &[]);
        p.scroll = Some(ScrollIndicator {
            offset: 5,
            total: 99,
        });
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let scene = build_scene(&frame, &CellMetrics::new(8.0, 16.0));
        // "[5/99]" => one bg quad + 6 glyphs, all on the last row.
        assert_eq!(scene.overlay_quads.len(), 1);
        assert_eq!(scene.overlay_glyphs.len(), 6);
    }

    #[test]
    fn cols_rows_floors_and_clamps() {
        let m = CellMetrics::new(8.0, 16.0);
        assert_eq!(m.cols_rows(800, 320), (100, 20));
        // Partial trailing cell is floored away.
        assert_eq!(m.cols_rows(805, 327), (100, 20));
        // Never zero, even for a zero/negative area.
        assert_eq!(m.cols_rows(0, 0), (1, 1));
        assert_eq!(m.cols_rows(-50, -50), (1, 1));
    }
}
