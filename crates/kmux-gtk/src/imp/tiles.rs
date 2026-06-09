//! Tiling glue for the GTK frontend.
//!
//! The terminal grid is drawn by `render::render_tiled`, which lays the active
//! tab's panes out via the shared `kmux_app::layout` resolver. These helpers do
//! the two non-render parts: push each pane's resolved sub-rect size down to the
//! client (so its PTY is sized to the tile, not the whole window), and hit-test
//! a pixel position to a pane id (click-to-focus).

use std::cell::RefCell;
use std::rc::Rc;

use kmux_app::layout::{LayoutConfig, PaneRect, resolve_layout};
use kmux_protocol::messages::TermSize;

use super::Frontend;
use super::render::GUTTER;

fn cfg() -> LayoutConfig {
    LayoutConfig {
        gutter_cols: GUTTER,
        gutter_rows: GUTTER,
        min_cols: 1,
        min_rows: 1,
    }
}

/// Resolve the active tab's layout against the drawing's pixel size and push the
/// per-pane sub-rect sizes to the client. No-op when there's no active tab.
pub fn push_sizes(fe: &Rc<RefCell<Frontend>>, width_px: i32, height_px: i32) {
    let mut f = fe.borrow_mut();
    let (cols, rows) = f.metrics.cols_rows(width_px, height_px);
    let Some(layout) = f.core.mgr.render_layout() else {
        return;
    };
    let Some(word) = f.core.mgr.active_session().map(str::to_string) else {
        return;
    };
    let (cw, ch) = (f.metrics.cell_w, f.metrics.cell_h);
    let rects = resolve_layout(&layout, cols, rows, &cfg());
    let sizes: Vec<(String, TermSize)> = rects
        .iter()
        .map(|r| {
            (
                format!("{word}/{}", r.pane_index),
                TermSize {
                    rows: r.rows,
                    cols: r.cols,
                    pixel_width: (r.cols as f64 * cw) as u16,
                    pixel_height: (r.rows as f64 * ch) as u16,
                },
            )
        })
        .collect();
    f.core.mgr.set_pane_sizes(sizes);
}

/// The pane id and its resolved cell rect at pixel `(x, y)` within the drawing,
/// or `None` in a gutter / when there's no active tab. Pointer handlers use the
/// rect to map a window pixel into the pane's *local* cell grid (selection and
/// scroll are otherwise off by the pane's offset inside a tiled tab).
pub fn pane_hit(
    f: &Frontend,
    x: f64,
    y: f64,
    width_px: i32,
    height_px: i32,
) -> Option<(String, PaneRect)> {
    let (cols, rows) = f.metrics.cols_rows(width_px, height_px);
    let layout = f.core.mgr.render_layout()?;
    let word = f.core.mgr.active_session()?.to_string();
    let (cw, ch) = (f.metrics.cell_w, f.metrics.cell_h);
    for r in resolve_layout(&layout, cols, rows, &cfg()) {
        let (px, py) = (r.col as f64 * cw, r.row as f64 * ch);
        let (pw, ph) = (r.cols as f64 * cw, r.rows as f64 * ch);
        if x >= px && x < px + pw && y >= py && y < py + ph {
            return Some((format!("{word}/{}", r.pane_index), r));
        }
    }
    None
}

/// The pane id at pixel position `(x, y)` within the drawing, if any.
pub fn pane_at(
    fe: &Rc<RefCell<Frontend>>,
    x: f64,
    y: f64,
    width_px: i32,
    height_px: i32,
) -> Option<String> {
    pane_hit(&fe.borrow(), x, y, width_px, height_px).map(|(id, _)| id)
}

/// The currently-focused pane's resolved rect within a `width_px × height_px`
/// area. Lets pointer handlers map a window pixel into the focused pane's local
/// cell grid — selection and auto-scroll act on the focused pane, which
/// click-to-focus has already moved under the pointer on press.
pub fn focused_rect(f: &Frontend, width_px: i32, height_px: i32) -> Option<PaneRect> {
    let (cols, rows) = f.metrics.cols_rows(width_px, height_px);
    let layout = f.core.mgr.render_layout()?;
    let focused = f
        .core
        .mgr
        .active_pane_id()
        .and_then(|p| p.rsplit_once('/'))
        .and_then(|(_, i)| i.parse::<u32>().ok())?;
    resolve_layout(&layout, cols, rows, &cfg())
        .into_iter()
        .find(|r| r.pane_index == focused)
}
