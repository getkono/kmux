//! The grid as Swift sees it: identity for change detection, the cursor,
//! the packed cell buffer, and the cursor rects all three renderers fill.

use super::*;

/// Cheap grid identity for change detection: the frontend re-fetches
/// [`KmuxDriver::grid_snapshot`] only when a generation differs. `generation`
/// changes on *any* update (cursor move or cell change); `cells_generation`
/// changes only when cells change (so the renderer can skip re-packing cells
/// when only the cursor moved).
#[derive(uniffi::Record)]
pub struct GridInfo {
    pub rows: u32,
    pub cols: u32,
    pub generation: u64,
    pub cells_generation: u64,
}

/// Cursor position + appearance. `shape`: 0=block, 1=underline, 2=bar,
/// 3=hollow-block, 4=hidden.
#[derive(uniffi::Record)]
pub struct FfiCursor {
    pub row: u32,
    pub col: u32,
    pub shape: u8,
    pub visible: bool,
    pub blink: bool,
}

/// The active grid as a packed cell buffer (see [`kmux_render::packed`]) plus
/// dimensions and cursor. `cells` is `rows * cols * 16` bytes, row-major.
#[derive(uniffi::Record)]
pub struct GridSnapshot {
    pub rows: u32,
    pub cols: u32,
    pub cursor: FfiCursor,
    pub cells: Vec<u8>,
}

/// One solid rect of the cursor in physical px — exactly what `kmux_render`
/// would fill (block/bar/underline = 1 rect; hollow-block = 4).
#[derive(uniffi::Record)]
pub struct FfiCursorRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// The solid rects a cursor occupies, in physical px relative to the pane's
/// top-left — exactly the rects the GPU renderer fills.
///
/// A CPU frontend needs these to *draw* the cursor, not only to inspect it.
/// Both the wgpu path and the GTK Cairo path read
/// [`kmux_render::cursor_geometry`]; before this existed, Swift's CoreText path
/// had no way to and rasterized its own, with a hardcoded 2px bar/underline
/// against the renderer's scale-aware thickness — so the same cursor looked
/// different depending on the renderer, and too thin on a Retina display.
///
/// Free-standing rather than a `KmuxDriver` method: it is pure geometry over its
/// arguments, so it neither needs nor should take the driver's lock on the draw
/// path.
///
/// Takes the cursor's position and shape rather than an [`FfiCursor`] because
/// `visible` and `blink` deliberately do not enter into it — geometry is what
/// the cursor *would* occupy, and whether to draw it this frame is the caller's
/// question (`FfiCursor::visible`, and the blink phase). A cursor outside the
/// grid, or of a shape this build does not know, yields no rects, so a frontend
/// can fill whatever it is handed without a range check of its own.
#[uniffi::export]
#[must_use]
pub fn kmux_cursor_rects(
    col: u32,
    row: u32,
    shape: u8,
    cols: u32,
    rows: u32,
    cell_w: f32,
    cell_h: f32,
) -> Vec<FfiCursorRect> {
    // Saturating rather than truncating: a coordinate too large for the grid's
    // own type is out of range, and must read as out of range rather than
    // wrapping to somewhere inside it.
    let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    let view = CursorView {
        col: clamp(col),
        row: clamp(row),
        shape: packed::cursor_shape_from_code(shape),
        blink: false,
        visible: true,
    };
    let cell = kmux_render::CellMetrics::new(cell_w, cell_h);
    kmux_render::cursor_geometry(&view, (0.0, 0.0), clamp(cols), clamp(rows), &cell)
        .rects
        .into_iter()
        .map(|r| FfiCursorRect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        })
        .collect()
}

/// What the renderer is handed for the focused pane this frame, for the Swift
/// render-debug overlay. Mirrors [`kmux_app::core::RenderDebugSnapshot`] (a flat
/// record, like [`FfiCursor`]), with the cursor's pixel rects computed here via
/// [`kmux_render::cursor_geometry`] from the cell geometry Swift passes in.
///
/// `has_pane` gates the pane fields; `has_cursor` gates the cursor fields (false
/// when no pane is active or it is scrolled into history). `cursor_shape` uses
/// the same code as [`FfiCursor::shape`] (0=block … 4=hidden).
#[derive(uniffi::Record)]
pub struct FfiRenderDebug {
    pub frame_width: u32,
    pub frame_height: u32,
    pub scale: f32,
    pub renderer: String,
    pub blink_on: bool,
    /// The renderer's scale-aware cursor thickness for the passed cell geometry
    /// (compare against the CoreText path's own constants).
    pub cursor_thickness: f32,
    pub has_pane: bool,
    pub pane_id: String,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub scroll_offset: u64,
    pub has_cursor: bool,
    pub cursor_col: u32,
    pub cursor_row: u32,
    pub cursor_shape: u8,
    pub cursor_blink: bool,
    pub cursor_visible: bool,
    pub cursor_is_drawn: bool,
    /// Whether the cursor falls within the grid (else `cursor_rects` is empty).
    pub cursor_in_range: bool,
    pub cursor_cell_x: f32,
    pub cursor_cell_y: f32,
    pub cursor_rects: Vec<FfiCursorRect>,
}
