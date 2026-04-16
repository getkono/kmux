use std::cell::Cell;
use std::time::Instant;

use iced::{
    Element, Length,
    widget::canvas::{self, Canvas},
};
use kmux_protocol::messages::CursorState;

use kmux_client::event_log::DiagSnapshot;
use kmux_client::grid::{CellGrid, GridPos, ScrollbackBuffer, Selection};
use kmux_client::metrics::MetricsSnapshot;

use crate::app::Message;

mod cell;
mod geometry;
mod hud;
mod rendering;
mod selection;

pub(super) const FONT_SIZE: f32 = 13.0;

/// Borrowed snapshot of the CellGrid used as the canvas `Program`.
///
/// Borrows cells from the grid (no clone) -- valid for the lifetime of a
/// single `view()` call.
pub struct GridView<'a> {
    pub(super) cells: &'a [kmux_protocol::messages::CellState],
    pub(super) cursor: CursorState,
    pub(super) rows: usize,
    pub(super) cols: usize,
    pub(super) cells_generation: u64,
    pub(super) metrics: Option<MetricsSnapshot>,
    pub(super) diag: Option<DiagSnapshot>,
    pub(super) scroll_offset: usize,
    pub(super) scrollback: &'a ScrollbackBuffer,
    pub(super) selection: Option<Selection>,
    pub(super) modes: kmux_protocol::messages::TermModes,
}

impl<'a> GridView<'a> {
    fn from_grid(
        grid: &'a CellGrid,
        metrics: Option<MetricsSnapshot>,
        diag: Option<DiagSnapshot>,
    ) -> Self {
        Self {
            cells: grid.cells(),
            cursor: *grid.cursor(),
            rows: grid.rows,
            cols: grid.cols,
            cells_generation: grid.cells_generation(),
            metrics,
            diag,
            scroll_offset: grid.scroll_offset(),
            scrollback: grid.scrollback(),
            selection: grid.selection().copied(),
            modes: grid.modes(),
        }
    }
}

/// State preserved across canvas redraws.
///
/// The `cells_cache` is rebuilt whenever `cache_generation` falls behind the
/// grid's `cells_generation`, guaranteeing every server-side change is rendered.
/// FPS measurement window in seconds.
const FPS_WINDOW_SECS: f64 = 1.0;

pub struct CanvasState {
    pub(super) rows: u16,
    pub(super) cols: u16,
    pub(super) cells_cache: canvas::Cache,
    pub(super) cursor_cache: canvas::Cache,
    /// Generation of cells data when `cells_cache` was last built.
    pub(super) cache_generation: Cell<u64>,
    pub(super) draw_duration_ms: Cell<f64>,
    /// Circular buffer of draw timestamps for FPS calculation.
    pub(super) draw_timestamps: std::cell::RefCell<std::collections::VecDeque<Instant>>,
    pub(super) last_cache_hit: Cell<bool>,
    // Mouse tracking for selection
    pub(super) mouse_down: bool,
    pub(super) last_click_time: Option<Instant>,
    pub(super) last_click_pos: Option<GridPos>,
    pub(super) click_count: u8,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            rows: 0,
            cols: 0,
            cells_cache: canvas::Cache::default(),
            cursor_cache: canvas::Cache::default(),
            cache_generation: Cell::new(0),
            draw_duration_ms: Cell::new(0.0),
            draw_timestamps: std::cell::RefCell::new(std::collections::VecDeque::with_capacity(
                128,
            )),
            last_cache_hit: Cell::new(true),
            mouse_down: false,
            last_click_time: None,
            last_click_pos: None,
            click_count: 0,
        }
    }
}

impl CanvasState {
    pub(super) fn record_draw(&self, now: Instant) {
        let mut ts = self.draw_timestamps.borrow_mut();
        ts.push_back(now);
        while let Some(&front) = ts.front() {
            if now.duration_since(front).as_secs_f64() > FPS_WINDOW_SECS {
                ts.pop_front();
            } else {
                break;
            }
        }
    }

    pub(super) fn fps(&self) -> f64 {
        let ts = self.draw_timestamps.borrow();
        if ts.len() < 2 {
            return 0.0;
        }
        let first = *ts.front().unwrap();
        let last = *ts.back().unwrap();
        let span = last.duration_since(first).as_secs_f64();
        if span < 0.001 {
            return 0.0;
        }
        (ts.len() - 1) as f64 / span
    }
}

/// Render the cell grid as a fill-parent canvas widget.
pub fn view<'a>(
    grid: &'a CellGrid,
    _session: &'a str,
    metrics: Option<MetricsSnapshot>,
    diag: Option<DiagSnapshot>,
) -> Element<'a, Message> {
    let snapshot = GridView::from_grid(grid, metrics, diag);
    Canvas::new(snapshot)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
