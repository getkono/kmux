use std::cell::Cell;
use std::time::Instant;

use iced::{
    Color as IcedColor, Element, Font, Length, Pixels, Point as IcedPoint, Rectangle, Size,
    alignment,
    font::{Style, Weight},
    mouse,
    widget::canvas::{self, Canvas, Text},
};
use kmux_protocol::messages::{
    CellAttrs, CellColor, CellState, CursorShape, CursorState, TermModes,
};

use kmux_client::event_log::DiagSnapshot;
use kmux_client::grid::{
    CELL_HEIGHT, CELL_WIDTH, CellGrid, DEFAULT_BG, GridPos, MULTI_CLICK_TIMEOUT_MS,
    ScrollbackBuffer, Selection, SelectionMode,
};
use kmux_client::metrics::MetricsSnapshot;

use crate::app::Message;

const FONT_SIZE: f32 = 13.0;

//  Canvas rendering

/// Borrowed snapshot of the CellGrid used as the canvas `Program`.
///
/// Borrows cells from the grid (no clone) -- valid for the lifetime of a
/// single `view()` call.
pub struct GridView<'a> {
    cells: &'a [CellState],
    cursor: CursorState,
    rows: usize,
    cols: usize,
    cells_generation: u64,
    metrics: Option<MetricsSnapshot>,
    diag: Option<DiagSnapshot>,
    scroll_offset: usize,
    scrollback: &'a ScrollbackBuffer,
    selection: Option<Selection>,
    modes: TermModes,
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

// ── Coordinate conversion ──

/// Convert a pixel position to an absolute grid position.
fn pixel_to_grid_pos(
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
fn abs_row_to_viewport(
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

fn cell_color_to_iced(c: CellColor) -> IcedColor {
    IcedColor::from_rgb8(c.r, c.g, c.b)
}

fn default_bg() -> IcedColor {
    IcedColor::from_rgb8(0x28, 0x2c, 0x34)
}

fn draw_cell(frame: &mut canvas::Frame, cell: &CellState, row: usize, col: usize) {
    let x = col as f32 * CELL_WIDTH;
    let y = row as f32 * CELL_HEIGHT;

    // Wide-char spacer: paint background only, skip text/decorations.
    if cell.attrs.contains(CellAttrs::WIDE_CHAR_SPACER) {
        if cell.bg != DEFAULT_BG {
            frame.fill_rectangle(
                IcedPoint::new(x, y),
                Size::new(CELL_WIDTH, CELL_HEIGHT),
                cell_color_to_iced(cell.bg),
            );
        }
        return;
    }

    // Wide chars span two columns.
    let cell_w = if cell.attrs.contains(CellAttrs::WIDE_CHAR) {
        CELL_WIDTH * 2.0
    } else {
        CELL_WIDTH
    };

    if cell.bg != DEFAULT_BG {
        frame.fill_rectangle(
            IcedPoint::new(x, y),
            Size::new(cell_w, CELL_HEIGHT),
            cell_color_to_iced(cell.bg),
        );
    }

    if !cell.attrs.contains(CellAttrs::HIDDEN) && cell.c != ' ' {
        // DIM: blend foreground halfway toward background.
        let fg_color = if cell.attrs.contains(CellAttrs::DIM) {
            let fg = cell_color_to_iced(cell.fg);
            let bg = cell_color_to_iced(cell.bg);
            IcedColor {
                r: (fg.r + bg.r) * 0.5,
                g: (fg.g + bg.g) * 0.5,
                b: (fg.b + bg.b) * 0.5,
                a: 1.0,
            }
        } else {
            cell_color_to_iced(cell.fg)
        };

        // Bold / italic font selection.
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

        frame.fill_text(Text {
            content: cell.c.to_string(),
            position: IcedPoint::new(x, y),
            color: fg_color,
            size: Pixels(FONT_SIZE),
            line_height: iced::widget::text::LineHeight::Absolute(Pixels(CELL_HEIGHT)),
            font,
            horizontal_alignment: alignment::Horizontal::Left,
            vertical_alignment: alignment::Vertical::Top,
            shaping: iced::widget::text::Shaping::Advanced,
        });

        // Underline decoration.
        if cell.attrs.contains(CellAttrs::UNDERLINE) {
            frame.fill_rectangle(
                IcedPoint::new(x, y + CELL_HEIGHT - 1.0),
                Size::new(cell_w, 1.0),
                fg_color,
            );
        }

        // Strikethrough decoration.
        if cell.attrs.contains(CellAttrs::STRIKETHROUGH) {
            frame.fill_rectangle(
                IcedPoint::new(x, y + CELL_HEIGHT * 0.5),
                Size::new(cell_w, 1.0),
                fg_color,
            );
        }
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
                                frame.fill_text(Text {
                                    content: cell.c.to_string(),
                                    position: IcedPoint::new(x, y),
                                    color: IcedColor::BLACK,
                                    size: Pixels(FONT_SIZE),
                                    line_height: iced::widget::text::LineHeight::Absolute(Pixels(
                                        CELL_HEIGHT,
                                    )),
                                    font,
                                    horizontal_alignment: alignment::Horizontal::Left,
                                    vertical_alignment: alignment::Vertical::Top,
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

/// Selection highlight color (One Dark ACCENT at 30% opacity).
const SELECTION_BG: IcedColor = IcedColor {
    r: 0x61 as f32 / 255.0,
    g: 0xaf as f32 / 255.0,
    b: 0xef as f32 / 255.0,
    a: 0.3,
};

/// Draw the selection overlay as semi-transparent rectangles.
fn draw_selection_overlay(
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

/// Draw a scroll position indicator at the top-right corner.
fn draw_scroll_indicator(
    renderer: &iced::Renderer,
    bounds: Rectangle,
    scroll_offset: usize,
    scrollback_len: usize,
) -> canvas::Geometry {
    let mut frame = canvas::Frame::new(renderer, bounds.size());

    let label = format!("[{}/{}]", scroll_offset, scrollback_len);
    let pad = 8.0;
    let font_size = 12.0;
    // Approximate text width: ~7px per character at size 12.
    let text_w = label.len() as f32 * 7.0;
    let x = bounds.width - text_w - pad;
    let y = pad;

    // Semi-transparent background pill.
    frame.fill_rectangle(
        IcedPoint::new(x - 4.0, y - 2.0),
        Size::new(text_w + 8.0, 18.0),
        IcedColor {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.7,
        },
    );

    frame.fill_text(Text {
        content: label,
        position: IcedPoint::new(x, y),
        color: IcedColor::from_rgb8(0xf1, 0xfa, 0x8c), // amber
        size: Pixels(font_size),
        line_height: iced::widget::text::LineHeight::Absolute(Pixels(16.0)),
        font: Font::MONOSPACE,
        horizontal_alignment: alignment::Horizontal::Left,
        vertical_alignment: alignment::Vertical::Top,
        shaping: iced::widget::text::Shaping::Basic,
    });

    frame.into_geometry()
}

/// Draw the HUD overlay as an uncached geometry layer.
fn draw_hud(
    renderer: &iced::Renderer,
    bounds: Rectangle,
    metrics: &MetricsSnapshot,
    diag: Option<&DiagSnapshot>,
    draw_ms: f64,
    fps: f64,
    cache_hit: bool,
) -> canvas::Geometry {
    const HUD_W: f32 = 320.0;
    const HUD_PAD: f32 = 8.0;
    const LINE_H: f32 = 18.0;
    const HUD_FONT_SIZE: f32 = 12.0;
    const MAX_EVENTS: usize = 5;

    let mut frame = canvas::Frame::new(renderer, bounds.size());

    let hud_x = bounds.width - HUD_W - HUD_PAD;
    let hud_y = HUD_PAD;

    let cache_label = if cache_hit {
        "HIT (overlay)"
    } else {
        "MISS (rebuild)"
    };
    let green = IcedColor::from_rgb8(0x50, 0xfa, 0x7b);
    let amber = IcedColor::from_rgb8(0xf1, 0xfa, 0x8c);
    let dim = IcedColor::from_rgb8(0x88, 0x88, 0x88);

    // Collect all HUD lines with their colors
    let c = &metrics.counters;
    let mut lines: Vec<(String, IcedColor)> = vec![
        (
            format!(
                "Net+Apply: {:.1}ms avg / {:.1}ms max",
                metrics.net_apply_avg_ms, metrics.net_apply_max_ms
            ),
            green,
        ),
        (
            format!("Apply:     {:.2}ms avg", metrics.apply_avg_ms),
            green,
        ),
        (format!("Draw:      {:.1}ms (prev frame)", draw_ms), green),
        (
            format!("Batch:     {:.1} msgs avg", metrics.batch_avg),
            green,
        ),
        (format!("FPS:       {fps:.0}"), green),
        (format!("Diff:      {} ops", metrics.last_diff_ops), green),
        (
            format!("LargeDiff: {:.1}ms", metrics.last_large_diff_ms),
            if metrics.last_large_diff_ms > 16.0 {
                amber
            } else {
                green
            },
        ),
        (format!("Cache:     {cache_label}"), green),
        (
            format!(
                "Snapshot:  {}",
                if metrics.snapshot_mode {
                    "FORCED"
                } else {
                    "off"
                }
            ),
            if metrics.snapshot_mode { amber } else { green },
        ),
        ("--- Diagnostics ---".to_owned(), dim),
        (
            format!(
                "Disc:{}  Gap:{}  Lag:{}  Sync:{}",
                c.stale_discards, c.seqno_gaps, c.lag_events, c.resyncs
            ),
            amber,
        ),
    ];

    // Recent events
    if let Some(diag) = diag {
        for (ts, text) in diag.events.iter().rev().take(MAX_EVENTS).rev() {
            let ago = ts.elapsed().as_secs();
            let label = if ago < 60 {
                format!("[{ago}s ago] {text}")
            } else {
                format!("[{}m ago] {text}", ago / 60)
            };
            lines.push((label, amber));
        }
    }

    // Semi-transparent background sized to actual content
    let hud_h = 6.0 + lines.len() as f32 * LINE_H + 6.0;
    frame.fill_rectangle(
        IcedPoint::new(hud_x, hud_y),
        Size::new(HUD_W, hud_h),
        IcedColor {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.75,
        },
    );

    for (i, (content, color)) in lines.iter().enumerate() {
        frame.fill_text(Text {
            content: content.clone(),
            position: IcedPoint::new(hud_x + 8.0, hud_y + 6.0 + i as f32 * LINE_H),
            color: *color,
            size: Pixels(HUD_FONT_SIZE),
            line_height: iced::widget::text::LineHeight::Absolute(Pixels(LINE_H)),
            font: Font::MONOSPACE,
            horizontal_alignment: alignment::Horizontal::Left,
            vertical_alignment: alignment::Vertical::Top,
            shaping: iced::widget::text::Shaping::Basic,
        });
    }

    frame.into_geometry()
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
