//! Pure cell→quad geometry: turning a [`Frame`] into draw primitives.
//!
//! This is GPU-free and exhaustively unit-tested — the geometry is computed here
//! and the renderer ([`crate::renderer`]) only uploads it. It mirrors the draw
//! order of the CPU renderers it replaces:
//!
//! 1. **`bg_quads`** — every cell's background (opaque; spacers included so
//!    wide-char halves stay opaque).
//! 2. **glyphs** — each cell's glyph in its foreground (dim → reduced alpha).
//! 3. **`overlay_quads`** — underline/strikethrough rules, selection wash, the
//!    cursor (block fill / bar / underline / hollow outline), focus border, and
//!    the scroll-indicator background — in that emission order, so the cursor
//!    sits above the wash and the border above all.
//! 4. **`overlay_glyphs`** — the block cursor's glyph (in `cursor_fg`, over its
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
            (true, true) => Self::BoldItalic,
            (true, false) => Self::Bold,
            (false, true) => Self::Italic,
            (false, false) => Self::Regular,
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

/// Per-layer primitive counts of a [`SceneGeometry`], for debug HUDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SceneCounts {
    /// Background quads (one per cell).
    pub bg_quads: usize,
    /// Cell glyphs.
    pub glyphs: usize,
    /// Overlay quads (rules, selection wash, cursor, focus border, scroll bg).
    pub overlay_quads: usize,
    /// Overlay glyphs (block-cursor glyph + scroll-indicator text).
    pub overlay_glyphs: usize,
}

impl SceneGeometry {
    /// The per-layer primitive counts, for debug HUDs that report what the
    /// renderer was handed this frame.
    pub fn counts(&self) -> SceneCounts {
        SceneCounts {
            bg_quads: self.bg_quads.len(),
            glyphs: self.glyphs.len(),
            overlay_quads: self.overlay_quads.len(),
            overlay_glyphs: self.overlay_glyphs.len(),
        }
    }
}

/// One solid rectangle of a cursor in physical pixels (the cursor's [`SolidQuad`]
/// geometry without the palette color). A block/bar/underline cursor is a single
/// rect; a hollow-block is four (its outline).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorRect {
    /// Left edge (physical px).
    pub x: f32,
    /// Top edge (physical px).
    pub y: f32,
    /// Width (physical px).
    pub w: f32,
    /// Height (physical px).
    pub h: f32,
}

/// The cursor's pixel geometry for debug tooling: the cell origin plus the solid
/// rectangles the renderer would fill. Built by [`cursor_geometry`], which shares
/// [`cursor_shape_rects`] with [`emit_cursor`] so the debug overlay provably
/// matches what is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorGeometry {
    /// Cursor cell top-left in physical px (computed even when out of range).
    pub cell_origin: (f32, f32),
    /// Whether the cursor falls within the pane grid (else `rects` is empty).
    pub in_range: bool,
    /// The solid rects the cursor occupies (empty for `Hidden`/out-of-range).
    pub rects: Vec<CursorRect>,
}

/// The cursor's solid-rect geometry for `cursor` at `pane_origin` (the pane's
/// top-left in physical px), exactly the rects [`emit_cursor`] would emit.
///
/// Range gating matches the renderer: an out-of-range cursor yields
/// `in_range = false` with no rects. Blink gating is **not** applied — debug
/// tooling wants the geometry even in a blink's off phase (consult
/// [`CursorView::is_drawn`] separately for whether it actually paints).
pub fn cursor_geometry(
    cursor: &CursorView,
    pane_origin: (f32, f32),
    cols: u16,
    rows: u16,
    m: &CellMetrics,
) -> CursorGeometry {
    let (ox, oy) = pane_origin;
    let cell_origin = (
        ox + cursor.col as f32 * m.cell_w,
        oy + cursor.row as f32 * m.cell_h,
    );
    if cursor.col >= cols || cursor.row >= rows {
        return CursorGeometry {
            cell_origin,
            in_range: false,
            rects: Vec::new(),
        };
    }
    CursorGeometry {
        cell_origin,
        in_range: true,
        rects: cursor_shape_rects(cursor.shape, cell_origin.0, cell_origin.1, m),
    }
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

/// One pane row's cached cell-layer geometry (issue #182, §3).
#[derive(Default, Clone)]
struct CachedRow {
    /// `CellGrid::row_generation` the row was built at; a row is reused while
    /// this matches. `u64::MAX` forces a (re)build.
    row_gen: u64,
    bg: Vec<SolidQuad>,
    glyphs: Vec<GlyphQuad>,
    rules: Vec<SolidQuad>,
}

/// Per-pane cached rows plus the identity of the grid they were built for.
#[derive(Default)]
struct PaneRowCache {
    /// Address of the borrowed `CellGrid`; a pane reassigned to a different grid
    /// (tab switch) rebuilds rather than reusing another grid's row stamps.
    grid_id: usize,
    rows: Vec<CachedRow>,
}

/// Frame-level signature: everything that changes the rendered cell layer beyond
/// per-row grid content (palette, metrics, viewport size, pane geometry). Any
/// change invalidates the whole cache — these are not tracked by `row_generation`.
#[derive(PartialEq)]
struct FrameSig {
    palette: Theme,
    metrics: CellMetrics,
    width: u32,
    height: u32,
    panes: Vec<(u16, u16, u16, u16)>,
}

fn frame_sig(frame: &Frame<'_>, m: &CellMetrics) -> FrameSig {
    FrameSig {
        palette: frame.palette.clone(),
        metrics: *m,
        width: frame.width,
        height: frame.height,
        panes: frame
            .panes
            .iter()
            .map(|p| (p.col, p.row, p.cols, p.rows))
            .collect(),
    }
}

/// Cross-frame cache for [`build_scene_cached`].
#[derive(Default)]
pub struct SceneCache {
    sig: Option<FrameSig>,
    panes: Vec<PaneRowCache>,
    /// Rows (re)built during the most recent [`build_scene_cached`] call; the
    /// rest were reused. Surfaced for tests + render-debug accounting.
    rebuilt_rows: usize,
}

impl SceneCache {
    /// Number of rows (re)built in the last `build_scene_cached` — the rest of
    /// the visible rows were served from cache (issue #182, §3).
    pub fn rebuilt_rows(&self) -> usize {
        self.rebuilt_rows
    }
}

/// Like [`build_scene`] but reuses the cell-layer geometry of rows that have not
/// changed since the last frame, re-emitting only dirty rows (issue #182, §3).
///
/// Produces byte-identical output to [`build_scene`]: both share `emit_cell` /
/// `emit_pane_overlays`, and a row is reused only when the frame signature is
/// unchanged and `CellGrid::row_generation` matches — so a content change always
/// rebuilds the affected row. Overlays (selection, cursor, focus, scroll) are
/// rebuilt every frame. The fast path is the steady live case (unscrolled `Grid`
/// panes); scrolled or packed panes fall back to a full emit.
pub fn build_scene_cached(
    frame: &Frame<'_>,
    m: &CellMetrics,
    cache: &mut SceneCache,
) -> SceneGeometry {
    let sig = frame_sig(frame, m);
    let reusable = cache.sig.as_ref() == Some(&sig);
    if cache.panes.len() != frame.panes.len() {
        cache.panes.clear();
        cache
            .panes
            .resize_with(frame.panes.len(), PaneRowCache::default);
    }

    let mut rebuilt = 0usize;
    let mut scene = SceneGeometry::default();
    for (pi, pane) in frame.panes.iter().enumerate() {
        let ox = pane.col as f32 * m.cell_w;
        let oy = pane.row as f32 * m.cell_h;

        let grid = match &pane.cells {
            CellSource::Grid(grid) if grid.scroll_offset() == 0 => Some(*grid),
            _ => None,
        };

        if let Some(grid) = grid {
            let grid_id = std::ptr::from_ref(grid) as usize;
            let pc = &mut cache.panes[pi];
            let nrows = pane.rows as usize;
            // Invalidate this pane's rows if the frame signature changed, the
            // grid identity changed, or the row count changed.
            if !reusable || pc.grid_id != grid_id || pc.rows.len() != nrows {
                pc.grid_id = grid_id;
                pc.rows.clear();
                pc.rows.resize(
                    nrows,
                    CachedRow {
                        row_gen: u64::MAX,
                        ..CachedRow::default()
                    },
                );
            }
            for vr in 0..nrows {
                let row_gen = grid.row_generation(vr);
                let row = &mut pc.rows[vr];
                if row.row_gen != row_gen {
                    let built = build_grid_row(frame, m, ox, oy, grid, vr);
                    row.row_gen = row_gen;
                    row.bg = built.bg_quads;
                    row.glyphs = built.glyphs;
                    row.rules = built.overlay_quads;
                    rebuilt += 1;
                }
                scene.bg_quads.extend_from_slice(&row.bg);
                scene.glyphs.extend_from_slice(&row.glyphs);
                scene.overlay_quads.extend_from_slice(&row.rules);
            }
        } else {
            // Scrolled or packed: no row cache, emit the cell layer directly.
            cache.panes[pi] = PaneRowCache::default();
            emit_pane_cells(&mut scene, frame, pane, m, ox, oy);
        }

        emit_pane_overlays(&mut scene, frame, pane, m, ox, oy);
    }

    cache.sig = Some(sig);
    cache.rebuilt_rows = rebuilt;
    scene
}

/// Build one unscrolled live `Grid` row's cell layer into a throwaway scene (its
/// `bg_quads` / `glyphs` / `overlay_quads` are the row's backgrounds, glyphs, and
/// rules). Mirrors the unscrolled branch of [`for_each_displayed_cell`] exactly.
fn build_grid_row(
    frame: &Frame<'_>,
    m: &CellMetrics,
    ox: f32,
    oy: f32,
    grid: &CellGrid,
    vr: usize,
) -> SceneGeometry {
    let mut row = SceneGeometry::default();
    let cols = grid.cols;
    let cells = grid.cells();
    let blank = CellState::default();
    for vc in 0..cols {
        let cell = cells.get(vr * cols + vc).unwrap_or(&blank);
        let rc = resolve_grid_cell(cell, frame.palette);
        emit_cell(&mut row, m, ox, oy, vr, vc, &rc);
    }
    row
}

fn emit_pane(scene: &mut SceneGeometry, frame: &Frame<'_>, pane: &PaneView<'_>, m: &CellMetrics) {
    let ox = pane.col as f32 * m.cell_w;
    let oy = pane.row as f32 * m.cell_h;
    emit_pane_cells(scene, frame, pane, m, ox, oy);
    emit_pane_overlays(scene, frame, pane, m, ox, oy);
}

/// Emit a pane's whole cell layer — backgrounds, glyphs, rules — for every
/// displayed cell (scrollback-composited when scrolled). Shared by [`build_scene`]
/// and the non-cacheable path of [`build_scene_cached`].
fn emit_pane_cells(
    scene: &mut SceneGeometry,
    frame: &Frame<'_>,
    pane: &PaneView<'_>,
    m: &CellMetrics,
    ox: f32,
    oy: f32,
) {
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
}

/// Emit a pane's view-dependent overlays — selection wash, cursor, focus border,
/// scroll indicator — in draw order, over the already-emitted cell layer. Split
/// out so the dirty-row cache ([`build_scene_cached`]) rebuilds these every frame
/// (they are cheap and depend on transient view state) while reusing the cell
/// layer of unchanged rows.
fn emit_pane_overlays(
    scene: &mut SceneGeometry,
    frame: &Frame<'_>,
    pane: &PaneView<'_>,
    m: &CellMetrics,
    ox: f32,
    oy: f32,
) {
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

    // The solid rects (one for block/bar/underline, four for the hollow outline),
    // shared with `cursor_geometry` so the debug overlay agrees with what we draw.
    for r in cursor_shape_rects(cv.shape, x, y, m) {
        scene.overlay_quads.push(SolidQuad {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
            color: cb,
        });
    }

    // A block cursor also redraws the covered glyph in cursor_fg, over the fill.
    if matches!(cv.shape, CursorShape::Block) {
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
}

/// The solid rectangles a cursor of `shape` occupies, with cell top-left at
/// `(x, y)` in physical px. Block → one full-cell rect; `HollowBlock` → four
/// outline rects (top, bottom, left, right — matching [`emit_outline`]);
/// Underline/Bar → one thin rect; Hidden → none. The single definition shared by
/// [`emit_cursor`] (the renderer) and [`cursor_geometry`] (debug tooling).
fn cursor_shape_rects(shape: CursorShape, x: f32, y: f32, m: &CellMetrics) -> Vec<CursorRect> {
    let (w, h, t) = (m.cell_w, m.cell_h, m.cursor_thickness);
    match shape {
        CursorShape::Block => vec![CursorRect { x, y, w, h }],
        CursorShape::HollowBlock => vec![
            CursorRect { x, y, w, h: t }, // top
            CursorRect {
                x,
                y: y + h - t,
                w,
                h: t,
            }, // bottom
            CursorRect { x, y, w: t, h }, // left
            CursorRect {
                x: x + w - t,
                y,
                w: t,
                h,
            }, // right
        ],
        CursorShape::Underline => vec![CursorRect {
            x,
            y: y + h - t,
            w,
            h: t,
        }],
        CursorShape::Bar => vec![CursorRect { x, y, w: t, h }],
        CursorShape::Hidden => Vec::new(),
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

    /// Issue #182, §3: the dirty-row cache must produce byte-identical geometry
    /// to a full rebuild at every step — a parity a content digest can never give
    /// (a digest is blind to render output). Frozen here against varied content
    /// (every glyph/attr/colour path the `kmux diagnostic` patterns exercise) and
    /// an incremental-update sequence, asserting both parity and that only the
    /// genuinely-dirty rows are rebuilt.
    #[test]
    fn dirty_row_cache_matches_full_rebuild() {
        use kmux_protocol::messages::{DiffOp, TermModes, TerminalDiff};

        let m = CellMetrics::new(8.0, 16.0);
        let rows = 6usize;
        let cols = 10usize;
        let mut initial =
            vec![cell(' ', CellAttrs::DEFAULT_FG | CellAttrs::DEFAULT_BG); rows * cols];
        for r in 0..rows {
            for c in 0..cols {
                let ch = char::from(b'A' + ((r * cols + c) % 26) as u8);
                let attrs = match (r + c) % 4 {
                    0 => 0,
                    1 => CellAttrs::BOLD,
                    2 => CellAttrs::UNDERLINE,
                    _ => CellAttrs::ITALIC | CellAttrs::STRIKETHROUGH,
                };
                initial[r * cols + c] = cell(ch, attrs);
            }
        }
        let mut grid = grid_with(initial, rows, cols);
        let mut cache = SceneCache::default();

        // Parity check + the count of rows actually rebuilt this frame.
        let check = |grid: &CellGrid, cache: &mut SceneCache, sel: &[(u16, u16, u16)]| -> usize {
            let p = pane(grid, sel);
            let frame = Frame::single(400, 400, 1.0, theme(), true, p);
            let cached = build_scene_cached(&frame, &m, cache);
            let full = build_scene(&frame, &m);
            assert_eq!(cached, full, "dirty-row cache diverged from full rebuild");
            cache.rebuilt_rows()
        };

        // Cold cache: every visible row built.
        assert_eq!(check(&grid, &mut cache, &[]), rows);
        // Unchanged frame: every row reused.
        assert_eq!(check(&grid, &mut cache, &[]), 0);
        // A one-cell diff dirties exactly one row.
        grid.apply_diff(TerminalDiff {
            ops: vec![DiffOp::Cell {
                row: 2,
                col: 3,
                cell: cell('Z', CellAttrs::BOLD),
            }],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_reset: None,
        });
        assert_eq!(check(&grid, &mut cache, &[]), 1);
        // A selection is overlay-only: no row rebuilt, still parity.
        assert_eq!(check(&grid, &mut cache, &[(1, 0, 4)]), 0);
        // Resize changes the frame signature → full rebuild, still parity.
        grid.resize(8, 12);
        assert_eq!(check(&grid, &mut cache, &[]), 8);
        // A scrolled pane falls back to a full emit and must still match.
        grid.apply_scrollback_append(
            0,
            vec![vec![cell('q', 0); 12].into(), vec![cell('r', 0); 12].into()],
        );
        grid.scroll_up(2);
        let p = pane(&grid, &[]);
        let frame = Frame::single(400, 400, 1.0, theme(), true, p);
        assert_eq!(
            build_scene_cached(&frame, &m, &mut cache),
            build_scene(&frame, &m),
            "scrolled fallback diverged from full rebuild"
        );
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
    fn cursor_geometry_block_matches_emit_cursor() {
        // The debug helper must report exactly the rect the renderer fills.
        let grid = grid_with(vec![cell('K', 0); 6], 2, 3);
        let mut p = pane(&grid, &[]);
        let cv = CursorView {
            col: 1,
            row: 1,
            shape: CursorShape::Block,
            blink: false,
            visible: true,
        };
        p.cursor = Some(cv);
        let m = CellMetrics::new(8.0, 16.0);
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let scene = build_scene(&frame, &m);

        let geo = cursor_geometry(&cv, (0.0, 0.0), 3, 2, &m);
        assert!(geo.in_range);
        assert_eq!(geo.rects.len(), 1);
        let q = scene.overlay_quads[0]; // the block fill emit_cursor pushed
        let r = geo.rects[0];
        assert_eq!((r.x, r.y, r.w, r.h), (q.x, q.y, q.w, q.h));
        assert_eq!((r.x, r.y), (8.0, 16.0)); // col 1, row 1 → 8×16 cells
    }

    #[test]
    fn cursor_geometry_bar_underline_hollow_hidden() {
        let m = CellMetrics::new(8.0, 16.0);
        let base = CursorView {
            col: 0,
            row: 0,
            shape: CursorShape::Bar,
            blink: false,
            visible: true,
        };

        let bar = cursor_geometry(&base, (0.0, 0.0), 1, 1, &m);
        assert_eq!(bar.rects.len(), 1);
        assert_eq!((bar.rects[0].w, bar.rects[0].h), (m.cursor_thickness, 16.0));

        let underline = cursor_geometry(
            &CursorView {
                shape: CursorShape::Underline,
                ..base
            },
            (0.0, 0.0),
            1,
            1,
            &m,
        );
        assert_eq!(underline.rects.len(), 1);
        assert_eq!(underline.rects[0].y, 16.0 - m.cursor_thickness);
        assert_eq!(
            (underline.rects[0].w, underline.rects[0].h),
            (8.0, m.cursor_thickness)
        );

        let hollow = cursor_geometry(
            &CursorView {
                shape: CursorShape::HollowBlock,
                ..base
            },
            (0.0, 0.0),
            1,
            1,
            &m,
        );
        assert_eq!(hollow.rects.len(), 4); // top, bottom, left, right

        let hidden = cursor_geometry(
            &CursorView {
                shape: CursorShape::Hidden,
                ..base
            },
            (0.0, 0.0),
            1,
            1,
            &m,
        );
        assert!(hidden.in_range);
        assert!(hidden.rects.is_empty());
    }

    #[test]
    fn cursor_geometry_out_of_range_is_empty_but_keeps_origin() {
        let m = CellMetrics::new(8.0, 16.0);
        let cv = CursorView {
            col: 5,
            row: 0,
            shape: CursorShape::Block,
            blink: false,
            visible: true,
        };
        let geo = cursor_geometry(&cv, (0.0, 0.0), 3, 2, &m);
        assert!(!geo.in_range);
        assert!(geo.rects.is_empty());
        assert_eq!(geo.cell_origin, (40.0, 0.0)); // still reported for diagnostics
    }

    #[test]
    fn cursor_geometry_applies_pane_origin() {
        let m = CellMetrics::new(8.0, 16.0);
        let cv = CursorView {
            col: 2,
            row: 1,
            shape: CursorShape::Block,
            blink: false,
            visible: true,
        };
        let geo = cursor_geometry(&cv, (100.0, 200.0), 4, 4, &m);
        assert_eq!(geo.cell_origin, (100.0 + 16.0, 200.0 + 16.0));
        assert_eq!((geo.rects[0].x, geo.rects[0].y), (116.0, 216.0));
    }

    #[test]
    fn scene_counts_match_layer_lengths() {
        let grid = grid_with(vec![cell('a', 0), cell('b', 0)], 1, 2);
        let p = pane(&grid, &[]);
        let frame = Frame::single(100, 100, 1.0, theme(), true, p);
        let scene = build_scene(&frame, &CellMetrics::new(8.0, 16.0));
        let c = scene.counts();
        assert_eq!(c.bg_quads, scene.bg_quads.len());
        assert_eq!(c.glyphs, scene.glyphs.len());
        assert_eq!(c.overlay_quads, scene.overlay_quads.len());
        assert_eq!(c.overlay_glyphs, scene.overlay_glyphs.len());
        assert_eq!(c.bg_quads, 2); // one background per cell
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

    #[test]
    fn grid_and_packed_build_identical_scenes() {
        // The whole geometry pipeline (not just per-cell resolution) must agree
        // between the Grid source and the equivalent packed buffer, so GTK
        // (Grid) and Swift (Packed) render identically.
        let grid = grid_with(
            vec![
                cell('a', 0),
                cell('b', CellAttrs::BOLD | CellAttrs::UNDERLINE),
                cell(' ', CellAttrs::DEFAULT_FG | CellAttrs::DEFAULT_BG),
                cell('世', CellAttrs::WIDE_CHAR),
                cell(' ', CellAttrs::WIDE_CHAR_SPACER),
                cell('z', CellAttrs::DIM | CellAttrs::STRIKETHROUGH),
            ],
            2,
            3,
        );
        let m = CellMetrics::new(8.0, 16.0);
        let cursor = Some(CursorView::from_state(grid.cursor()));

        let grid_pane = PaneView {
            col: 0,
            row: 0,
            cols: 3,
            rows: 2,
            focused: true,
            cells: CellSource::Grid(&grid),
            cursor,
            selection: &[],
            scroll: None,
        };
        let grid_scene = build_scene(&Frame::single(100, 100, 1.0, theme(), true, grid_pane), &m);

        let packed = packed::encode_cells(&grid, theme());
        let packed_pane = PaneView {
            col: 0,
            row: 0,
            cols: 3,
            rows: 2,
            focused: true,
            cells: CellSource::Packed {
                cells: &packed,
                cols: 3,
                rows: 2,
            },
            cursor,
            selection: &[],
            scroll: None,
        };
        let packed_scene = build_scene(
            &Frame::single(100, 100, 1.0, theme(), true, packed_pane),
            &m,
        );

        assert_eq!(grid_scene, packed_scene);
    }
}
