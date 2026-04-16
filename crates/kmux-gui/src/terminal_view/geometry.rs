use kmux_client::grid::{CELL_HEIGHT, CELL_WIDTH, GridPos};

/// Convert a pixel position to an absolute grid position.
pub fn pixel_to_grid_pos(
    x: f32,
    y: f32,
    cols: usize,
    rows: usize,
    scroll_offset: usize,
    scrollback_len: usize,
) -> GridPos {
    let col = (x / CELL_WIDTH).floor().max(0.0) as usize;
    let col = col.min(cols.saturating_sub(1));
    let viewport_row = (y / CELL_HEIGHT).floor().max(0.0) as usize;
    let viewport_row = viewport_row.min(rows.saturating_sub(1));
    let abs_row = scrollback_len.saturating_sub(scroll_offset) + viewport_row;
    GridPos { row: abs_row, col }
}

/// Convert an absolute row to a viewport row. Returns None if off-screen.
pub(super) fn abs_row_to_viewport(
    abs_row: usize,
    scroll_offset: usize,
    scrollback_len: usize,
) -> Option<usize> {
    let viewport_start = scrollback_len.saturating_sub(scroll_offset);
    if abs_row >= viewport_start {
        Some(abs_row - viewport_start)
    } else {
        None
    }
}
