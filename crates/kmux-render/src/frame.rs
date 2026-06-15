//! Toolkit-neutral per-frame render input.
//!
//! A [`Frame`] is everything the renderer needs to draw one frame, assembled by
//! the caller from already-resolved state — pane tile rects come from the shared
//! [`kmux_app::layout`] resolver, cursor/selection/scroll from the grid (or the
//! FFI getters). The renderer therefore never touches `AppCore`, the session
//! manager, or layout resolution; the two frontends build identical `Frame`s.
//!
//! Cells reach the renderer two ways via [`CellSource`]: GTK borrows its
//! [`CellGrid`] directly (zero copy), while the Swift app passes the existing
//! pre-packed 16-byte buffer across `kmux-ffi` (zero re-pack). Both produce the
//! same geometry — see the parity test in [`crate::geometry`].

use kmux_app::theme::Theme;
use kmux_client::grid::CellGrid;
use kmux_protocol::messages::{CursorShape, CursorState};

/// Everything required to render one frame into the renderer's target.
///
/// Lifetime `'a` borrows the palette, the per-pane cell sources, and the
/// per-pane selection spans for the duration of the [`render`] call, so a frame
/// is cheap to assemble each tick with no terminal-state copies.
///
/// [`render`]: crate::renderer
pub struct Frame<'a> {
    /// Content-area width in physical pixels.
    pub width: u32,
    /// Content-area height in physical pixels.
    pub height: u32,
    /// Device scale factor (logical→physical), e.g. `2.0` on Retina.
    pub scale: f32,
    /// Active palette. `DEFAULT_*` cells are resolved against this.
    pub palette: &'a Theme,
    /// Current blink phase: when `false`, blinking cursors render hidden.
    pub blink_on: bool,
    /// The visible panes, each with its resolved tile rect (in cells).
    pub panes: Vec<PaneView<'a>>,
    /// Whether more than one pane is visible — gates focus borders / dividers.
    pub multi: bool,
}

impl<'a> Frame<'a> {
    /// Construct a single-pane frame (the common case) covering the whole area.
    pub fn single(
        width: u32,
        height: u32,
        scale: f32,
        palette: &'a Theme,
        blink_on: bool,
        pane: PaneView<'a>,
    ) -> Self {
        Self {
            width,
            height,
            scale,
            palette,
            blink_on,
            panes: vec![pane],
            multi: false,
        }
    }
}

/// One pane's placement and contents within the content area.
pub struct PaneView<'a> {
    /// Tile origin column within the content area (0-based, in cells).
    pub col: u16,
    /// Tile origin row within the content area (0-based, in cells).
    pub row: u16,
    /// Tile width in cells.
    pub cols: u16,
    /// Tile height in cells.
    pub rows: u16,
    /// Whether this is the focused pane (drives the focus border when `multi`).
    pub focused: bool,
    /// The cells to draw, borrowed or pre-packed.
    pub cells: CellSource<'a>,
    /// The cursor, or `None` when scrolled into history (no live cursor shown).
    pub cursor: Option<CursorView>,
    /// Selected column spans per visible row, as `(visible_row, col_start,
    /// col_end)` inclusive — the output of
    /// [`CellGrid::visible_selection_spans`] (GTK) or the FFI `selection_for`
    /// getter (Swift). Empty when there is no selection.
    pub selection: &'a [(u16, u16, u16)],
    /// Scrollback position indicator, `Some` only when scrolled into history.
    pub scroll: Option<ScrollIndicator>,
}

/// Where a pane's cell data comes from.
///
/// Both variants describe the same row-major grid of displayed cells (with
/// scrollback already composited into the top rows when scrolled); the renderer
/// reads whichever is cheapest for the caller.
pub enum CellSource<'a> {
    /// Borrow the client grid directly (GTK / any in-process Rust caller). The
    /// renderer composites scrollback itself via [`crate::geometry`].
    Grid(&'a CellGrid),
    /// A pre-packed, palette-resolved buffer of `cols * rows`
    /// [`crate::packed::PACKED_CELL_LEN`]-byte cells, row-major (the Swift path
    /// over `kmux-ffi`). Already scrollback-composited by the packer.
    Packed {
        /// Row-major packed cell bytes (`cols * rows * PACKED_CELL_LEN`).
        cells: &'a [u8],
        /// Grid width in cells.
        cols: u16,
        /// Grid height in cells.
        rows: u16,
    },
}

impl CellSource<'_> {
    /// The grid dimensions `(cols, rows)` this source describes.
    pub fn dims(&self) -> (u16, u16) {
        match self {
            CellSource::Grid(grid) => (grid.cols as u16, grid.rows as u16),
            CellSource::Packed { cols, rows, .. } => (*cols, *rows),
        }
    }
}

/// The cursor to draw within a pane (position + shape + blink request).
///
/// A toolkit-neutral mirror of [`CursorState`] using the renderer's `(col,
/// row)` order. `blink` carries only the inner program's *request* (DECSCUSR);
/// whether the blink is currently visible is the frame-wide
/// [`Frame::blink_on`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorView {
    /// Cursor column (0-based, viewport).
    pub col: u16,
    /// Cursor row (0-based, viewport).
    pub row: u16,
    /// Cursor shape.
    pub shape: CursorShape,
    /// Whether the program requested a blinking cursor.
    pub blink: bool,
    /// Whether the cursor is visible at all (DECTCEM).
    pub visible: bool,
}

impl CursorView {
    /// Build a [`CursorView`] from the grid's [`CursorState`].
    pub fn from_state(cs: &CursorState) -> Self {
        Self {
            col: cs.col,
            row: cs.row,
            shape: cs.shape,
            blink: cs.blink,
            visible: cs.visible,
        }
    }

    /// Whether the cursor should paint this frame: visible, not a hidden shape,
    /// and either steady or in the on phase of a blink.
    pub fn is_drawn(&self, blink_on: bool) -> bool {
        self.visible && !matches!(self.shape, CursorShape::Hidden) && (!self.blink || blink_on)
    }
}

/// Scrollback position, rendered as a small `[offset/total]` indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollIndicator {
    /// Display rows scrolled back from the live bottom (`> 0`).
    pub offset: usize,
    /// Total scrollback display rows available.
    pub total: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::CursorState;

    #[test]
    fn cell_source_dims_grid_and_packed() {
        let grid = CellGrid::new(24, 80);
        assert_eq!(CellSource::Grid(&grid).dims(), (80, 24));

        let packed = CellSource::Packed {
            cells: &[],
            cols: 10,
            rows: 5,
        };
        assert_eq!(packed.dims(), (10, 5));
    }

    #[test]
    fn cursor_view_maps_state_col_row_order() {
        let cs = CursorState {
            row: 3,
            col: 7,
            shape: CursorShape::Bar,
            visible: true,
            blink: true,
        };
        let cv = CursorView::from_state(&cs);
        assert_eq!((cv.col, cv.row), (7, 3));
        assert_eq!(cv.shape, CursorShape::Bar);
        assert!(cv.blink && cv.visible);
    }

    #[test]
    fn cursor_is_drawn_respects_visibility_shape_and_blink() {
        let steady = CursorView {
            col: 0,
            row: 0,
            shape: CursorShape::Block,
            blink: false,
            visible: true,
        };
        // Steady cursors ignore the blink phase.
        assert!(steady.is_drawn(false));
        assert!(steady.is_drawn(true));

        let blinking = CursorView {
            blink: true,
            ..steady
        };
        assert!(!blinking.is_drawn(false), "blinking + off phase => hidden");
        assert!(blinking.is_drawn(true));

        let hidden_shape = CursorView {
            shape: CursorShape::Hidden,
            ..steady
        };
        assert!(!hidden_shape.is_drawn(true));

        let invisible = CursorView {
            visible: false,
            ..steady
        };
        assert!(!invisible.is_drawn(true));
    }

    #[test]
    fn frame_single_wraps_one_pane() {
        let theme = kmux_app::theme::default_theme();
        let grid = CellGrid::new(2, 4);
        let pane = PaneView {
            col: 0,
            row: 0,
            cols: 4,
            rows: 2,
            focused: true,
            cells: CellSource::Grid(&grid),
            cursor: None,
            selection: &[],
            scroll: None,
        };
        let frame = Frame::single(400, 200, 2.0, &theme, true, pane);
        assert_eq!(frame.panes.len(), 1);
        assert!(!frame.multi);
        assert_eq!((frame.width, frame.height), (400, 200));
    }
}
