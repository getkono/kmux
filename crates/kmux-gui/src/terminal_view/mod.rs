use std::cell::Cell;
use std::time::Instant;

use iced::{
    Color as IcedColor, Element, Font, Length, Pixels, Point as IcedPoint, Rectangle, Size,
    font::{Style, Weight},
    mouse,
    widget::canvas::{self, Canvas},
};
use kmux_protocol::messages::{CellAttrs, CursorShape, CursorState};

use kmux_client::event_log::DiagSnapshot;
use kmux_client::grid::{
    CELL_HEIGHT, CELL_WIDTH, CellGrid, GridPos, MULTI_CLICK_TIMEOUT_MS, ScrollbackBuffer,
    Selection, SelectionMode,
};
use kmux_client::metrics::MetricsSnapshot;

use crate::app::Message;

mod cell;
mod geometry;
mod hud;
mod selection;

use cell::{default_bg, draw_cell};
use geometry::pixel_to_grid_pos;
use hud::{draw_hud, draw_scroll_indicator};
use selection::draw_selection_overlay;

pub(super) const FONT_SIZE: f32 = 13.0;

//  Canvas rendering

/// Borrowed snapshot of the CellGrid used as the canvas `Program`.
///
/// Borrows cells from the grid (no clone) -- valid for the lifetime of a
/// single `view()` call.
pub struct GridView<'a> {
    cells: &'a [kmux_protocol::messages::CellState],
    cursor: CursorState,
    rows: usize,
    cols: usize,
    cells_generation: u64,
    metrics: Option<MetricsSnapshot>,
    diag: Option<DiagSnapshot>,
    scroll_offset: usize,
    scrollback: &'a ScrollbackBuffer,
    selection: Option<Selection>,
    modes: kmux_protocol::messages::TermModes,
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
    rows: u16,
    cols: u16,
    cells_cache: canvas::Cache,
    cursor_cache: canvas::Cache,
    /// Generation of cells data when `cells_cache` was last built.
    cache_generation: Cell<u64>,
    draw_duration_ms: Cell<f64>,
    /// Circular buffer of draw timestamps for FPS calculation.
    draw_timestamps: std::cell::RefCell<std::collections::VecDeque<Instant>>,
    last_cache_hit: Cell<bool>,
    // Mouse tracking for selection
    mouse_down: bool,
    last_click_time: Option<Instant>,
    last_click_pos: Option<GridPos>,
    click_count: u8,
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
    fn record_draw(&self, now: Instant) {
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

    fn fps(&self) -> f64 {
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

impl<'a> canvas::Program<Message> for GridView<'a> {
    type State = CanvasState;

    fn update(
        &self,
        state: &mut CanvasState,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        // Mouse wheel scrolling (3 lines per notch).
        if let canvas::Event::Mouse(mouse::Event::WheelScrolled { delta }) = &event {
            let lines = match delta {
                mouse::ScrollDelta::Lines { y, .. } => (*y * 3.0) as i32,
                mouse::ScrollDelta::Pixels { y, .. } => (*y / CELL_HEIGHT) as i32,
            };
            if lines != 0 {
                if self.modes.mouse_report() {
                    // Mouse reporting active: forward scroll to PTY as escape sequences.
                    let (col, row) = if let Some(pos) = cursor.position_in(bounds) {
                        let c = (pos.x / CELL_WIDTH).floor() as u16;
                        let r = (pos.y / CELL_HEIGHT).floor() as u16;
                        (
                            c.min(self.cols as u16 - 1) + 1,
                            r.min(self.rows as u16 - 1) + 1,
                        )
                    } else {
                        (1, 1)
                    };
                    return (
                        canvas::event::Status::Captured,
                        Some(Message::ForwardMouseScroll { col, row, lines }),
                    );
                } else {
                    // No mouse reporting: local scrollback navigation.
                    return (
                        canvas::event::Status::Captured,
                        Some(Message::ScrollTerminal(lines)),
                    );
                }
            }
        }

        // Mouse button pressed → start selection
        if let canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = &event
            && let Some(pos) = cursor.position_in(bounds)
        {
            let grid_pos = pixel_to_grid_pos(
                pos.x,
                pos.y,
                self.cols,
                self.rows,
                self.scroll_offset,
                self.scrollback.len(),
            );

            // Multi-click detection
            let now = Instant::now();
            let same_pos = state.last_click_pos.is_some_and(|p| p.row == grid_pos.row);
            let quick = state
                .last_click_time
                .is_some_and(|t| now.duration_since(t).as_millis() < MULTI_CLICK_TIMEOUT_MS);
            if quick && same_pos {
                state.click_count = (state.click_count % 3) + 1;
            } else {
                state.click_count = 1;
            }
            state.last_click_time = Some(now);
            state.last_click_pos = Some(grid_pos);
            state.mouse_down = true;

            let mode = match state.click_count {
                2 => SelectionMode::Word,
                3 => SelectionMode::Line,
                _ => SelectionMode::Normal,
            };
            return (
                canvas::event::Status::Captured,
                Some(Message::SelectionStart {
                    pos: grid_pos,
                    mode,
                }),
            );
        }

        // Mouse move during drag → update selection
        if let canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) = &event
            && state.mouse_down
        {
            if let Some(pos) = cursor.position_in(bounds) {
                let grid_pos = pixel_to_grid_pos(
                    pos.x,
                    pos.y,
                    self.cols,
                    self.rows,
                    self.scroll_offset,
                    self.scrollback.len(),
                );
                return (
                    canvas::event::Status::Captured,
                    Some(Message::SelectionUpdate { pos: grid_pos }),
                );
            } else if let Some(pos) = cursor.position() {
                // Cursor outside bounds during drag → auto-scroll
                let rel_y = pos.y - bounds.y;
                let direction = if rel_y < 0.0 {
                    3 // scroll up (into history)
                } else if rel_y > bounds.height {
                    -3 // scroll down (toward live)
                } else {
                    0
                };
                if direction != 0 {
                    return (
                        canvas::event::Status::Captured,
                        Some(Message::SelectionAutoScroll(direction)),
                    );
                }
            }
        }

        // Mouse button released → end selection
        if let canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) = &event
            && state.mouse_down
        {
            state.mouse_down = false;
            return (canvas::event::Status::Captured, Some(Message::SelectionEnd));
        }

        let new_rows = (bounds.height / CELL_HEIGHT).floor().max(1.0) as u16;
        let new_cols = (bounds.width / CELL_WIDTH).floor().max(1.0) as u16;
        if state.rows != new_rows || state.cols != new_cols {
            state.rows = new_rows;
            state.cols = new_cols;
            state.cells_cache.clear();
            return (
                canvas::event::Status::Ignored,
                Some(Message::TerminalResized {
                    rows: new_rows,
                    cols: new_cols,
                }),
            );
        }
        (canvas::event::Status::Ignored, None)
    }

    fn mouse_interaction(
        &self,
        _state: &CanvasState,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if cursor.is_over(bounds) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::default()
        }
    }

    fn draw(
        &self,
        state: &CanvasState,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let draw_start = Instant::now();

        // If cells changed since the cache was built, invalidate and rebuild.
        let cache_hit = self.cells_generation == state.cache_generation.get();
        if !cache_hit {
            state.cells_cache.clear();
            state.cache_generation.set(self.cells_generation);
        }
        state.last_cache_hit.set(cache_hit);

        let cells = self.cells;
        let cols = self.cols;
        let rows = self.rows;
        let cursor = &self.cursor;
        let scroll_offset = self.scroll_offset;
        let scrollback = self.scrollback;

        // Layer 1: All cells (cached until next cells_generation bump)
        let cells_geom = state.cells_cache.draw(renderer, bounds.size(), |frame| {
            frame.fill_rectangle(IcedPoint::ORIGIN, bounds.size(), default_bg());
            if scroll_offset == 0 {
                // Normal mode: render the visible grid directly.
                for (idx, cell) in cells.iter().enumerate() {
                    draw_cell(frame, cell, idx / cols, idx % cols);
                }
            } else {
                // Scroll mode: composite scrollback + visible grid.
                let sb_len = scrollback.len();
                for vr in 0..rows {
                    if vr < scroll_offset {
                        // This viewport row comes from scrollback.
                        let sb_idx = sb_len.saturating_sub(scroll_offset) + vr;
                        if let Some(line) = scrollback.get(sb_idx) {
                            for (col, cell) in line.iter().enumerate().take(cols) {
                                draw_cell(frame, cell, vr, col);
                            }
                        }
                    } else {
                        // This viewport row comes from the visible grid.
                        let grid_row = vr - scroll_offset;
                        let base = grid_row * cols;
                        for col in 0..cols {
                            let idx = base + col;
                            if idx < cells.len() {
                                draw_cell(frame, &cells[idx], vr, col);
                            }
                        }
                    }
                }
            }
        });

        // Layer 2: cursor (redrawn every frame -- always clear to pick up position changes)
        state.cursor_cache.clear();
        let cursor_geom = state.cursor_cache.draw(renderer, bounds.size(), |frame| {
            if scroll_offset == 0 && cursor.visible && cursor.shape != CursorShape::Hidden {
                let cur_row = cursor.row as usize;
                let cur_col = cursor.col as usize;
                if cur_row < rows && cur_col < cols {
                    let x = cur_col as f32 * CELL_WIDTH;
                    let y = cur_row as f32 * CELL_HEIGHT;
                    let idx = cur_row * cols + cur_col;

                    // Wide chars: cursor spans two columns.
                    let cursor_w = if cells
                        .get(idx)
                        .is_some_and(|c| c.attrs.contains(CellAttrs::WIDE_CHAR))
                    {
                        CELL_WIDTH * 2.0
                    } else {
                        CELL_WIDTH
                    };

                    match cursor.shape {
                        CursorShape::Block => {
                            frame.fill_rectangle(
                                IcedPoint::new(x, y),
                                Size::new(cursor_w, CELL_HEIGHT),
                                IcedColor {
                                    r: 1.0,
                                    g: 1.0,
                                    b: 1.0,
                                    a: 0.7,
                                },
                            );
                            if let Some(cell) = cells.get(idx)
                                && !cell.attrs.contains(CellAttrs::HIDDEN)
                                && cell.c != ' '
                            {
                                let font = Font {
                                    family: iced::font::Family::Monospace,
                                    weight: if cell.attrs.contains(CellAttrs::BOLD) {
                                        Weight::Bold
                                    } else {
                                        Weight::Normal
                                    },
                                    style: if cell.attrs.contains(CellAttrs::ITALIC) {
                                        Style::Italic
                                    } else {
                                        Style::Normal
                                    },
                                    ..Font::MONOSPACE
                                };
                                frame.fill_text(canvas::Text {
                                    content: cell.c.to_string(),
                                    position: IcedPoint::new(x, y),
                                    color: IcedColor::BLACK,
                                    size: Pixels(FONT_SIZE),
                                    line_height: iced::widget::text::LineHeight::Absolute(Pixels(
                                        CELL_HEIGHT,
                                    )),
                                    font,
                                    horizontal_alignment: iced::alignment::Horizontal::Left,
                                    vertical_alignment: iced::alignment::Vertical::Top,
                                    shaping: iced::widget::text::Shaping::Advanced,
                                });
                            }
                        }
                        CursorShape::Underline => {
                            frame.fill_rectangle(
                                IcedPoint::new(x, y + CELL_HEIGHT - 2.0),
                                Size::new(cursor_w, 2.0),
                                IcedColor::WHITE,
                            );
                        }
                        CursorShape::Bar => {
                            frame.fill_rectangle(
                                IcedPoint::new(x, y),
                                Size::new(2.0, CELL_HEIGHT),
                                IcedColor::WHITE,
                            );
                        }
                        CursorShape::HollowBlock => {
                            for (ox, oy, w, h) in [
                                (0.0, 0.0, cursor_w, 1.0),
                                (0.0, CELL_HEIGHT - 1.0, cursor_w, 1.0),
                                (0.0, 0.0, 1.0, CELL_HEIGHT),
                                (cursor_w - 1.0, 0.0, 1.0, CELL_HEIGHT),
                            ] {
                                frame.fill_rectangle(
                                    IcedPoint::new(x + ox, y + oy),
                                    Size::new(w, h),
                                    IcedColor::WHITE,
                                );
                            }
                        }
                        CursorShape::Hidden => unreachable!(),
                    }
                }
            }
        });

        // Track draw duration and FPS (previous frame's value is shown in HUD).
        let prev_draw_ms = state.draw_duration_ms.get();
        state
            .draw_duration_ms
            .set(draw_start.elapsed().as_secs_f64() * 1000.0);
        state.record_draw(draw_start);
        let fps = state.fps();

        // Layer 2.5: Selection overlay (uncached, cheap -- just rectangles)
        let selection_geom = self.selection.map(|sel| {
            draw_selection_overlay(
                renderer,
                bounds,
                &sel,
                cols,
                rows,
                scroll_offset,
                scrollback.len(),
            )
        });

        // Assemble layers: cells + selection + cursor + optional scroll indicator + optional HUD
        let mut layers = vec![cells_geom];
        if let Some(sel_geom) = selection_geom {
            layers.push(sel_geom);
        }
        layers.push(cursor_geom);

        // Layer 3: Scroll position indicator (when scrolled)
        if scroll_offset > 0 {
            layers.push(draw_scroll_indicator(
                renderer,
                bounds,
                scroll_offset,
                scrollback.len(),
            ));
        }

        // Layer 4: HUD overlay (never cached -- content changes every frame)
        if let Some(metrics) = self.metrics {
            let cache_hit = state.last_cache_hit.get();
            layers.push(draw_hud(
                renderer,
                bounds,
                &metrics,
                self.diag.as_ref(),
                prev_draw_ms,
                fps,
                cache_hit,
            ));
        }
        layers
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
