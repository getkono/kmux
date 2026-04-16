use iced::{Color as IcedColor, Point as IcedPoint, Rectangle, Size, widget::canvas};
use kmux_client::grid::{CELL_HEIGHT, CELL_WIDTH, Selection};

use super::geometry::abs_row_to_viewport;

/// Selection highlight color (One Dark ACCENT at 30% opacity).
const SELECTION_BG: IcedColor = IcedColor {
    r: 0x61 as f32 / 255.0,
    g: 0xaf as f32 / 255.0,
    b: 0xef as f32 / 255.0,
    a: 0.3,
};

/// Draw the selection overlay as semi-transparent rectangles.
pub fn draw_selection_overlay(
    renderer: &iced::Renderer,
    bounds: Rectangle,
    sel: &Selection,
    cols: usize,
    rows: usize,
    scroll_offset: usize,
    scrollback_len: usize,
) -> canvas::Geometry {
    let mut frame = canvas::Frame::new(renderer, bounds.size());
    let start = sel.start();
    let end = sel.end_pos();

    for abs_row in start.row..=end.row {
        let Some(vr) = abs_row_to_viewport(abs_row, scroll_offset, scrollback_len) else {
            continue;
        };
        if vr >= rows {
            break;
        }

        let col_start = if abs_row == start.row { start.col } else { 0 };
        let col_end = if abs_row == end.row {
            end.col
        } else {
            cols.saturating_sub(1)
        };

        let x = col_start as f32 * CELL_WIDTH;
        let y = vr as f32 * CELL_HEIGHT;
        let w = (col_end - col_start + 1) as f32 * CELL_WIDTH;

        frame.fill_rectangle(
            IcedPoint::new(x, y),
            Size::new(w, CELL_HEIGHT),
            SELECTION_BG,
        );
    }

    frame.into_geometry()
}
