use std::cell::Cell;
use std::time::Instant;

use iced::{
    Color as IcedColor, Element, Font, Length, Pixels, Point as IcedPoint, Rectangle, Size,
    alignment, mouse,
    widget::canvas::{self, Canvas, Text},
};
use smux_protocol::messages::{
    CellAttrs, CellState, CursorShape, CursorState, DiffOp, GridSnapshot, TermModes, TerminalDiff,
};

use crate::metrics::MetricsSnapshot;

use crate::app::Message;

const CELL_WIDTH: f32 = 8.0;
const CELL_HEIGHT: f32 = 16.0;
const FONT_SIZE: f32 = 13.0;

/// Client-side grid state -- receives pre-resolved cells from the server.
///
/// Unlike the old `TerminalBuffer` that wrapped `alacritty_terminal::Term`,
/// this is a thin grid of `CellState` values. All VT parsing and color
/// resolution happens on the server.
pub struct CellGrid {
    cells: Vec<CellState>,
    cursor: CursorState,
    modes: TermModes,
    pub rows: usize,
    pub cols: usize,
    generation: u64,
}

impl CellGrid {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            cells: vec![CellState::default(); rows * cols],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            rows,
            cols,
            generation: 0,
        }
    }

    /// Replace the entire grid from a server snapshot.
    pub fn apply_snapshot(&mut self, snapshot: GridSnapshot) {
        self.rows = snapshot.rows as usize;
        self.cols = snapshot.cols as usize;
        self.cells = snapshot.cells;
        self.cursor = snapshot.cursor;
        self.modes = snapshot.modes;
        self.generation += 1;
    }

    /// Apply a diff from the server -- only changed cells are updated.
    pub fn apply_diff(&mut self, diff: TerminalDiff) {
        for op in diff.ops {
            match op {
                DiffOp::Cell { row, col, cell } => {
                    let idx = row as usize * self.cols + col as usize;
                    if idx < self.cells.len() {
                        self.cells[idx] = cell;
                    }
                }
                DiffOp::Row {
                    row,
                    start_col,
                    cells,
                } => {
                    let base = row as usize * self.cols + start_col as usize;
                    for (i, cell) in cells.into_iter().enumerate() {
                        let idx = base + i;
                        if idx < self.cells.len() {
                            self.cells[idx] = cell;
                        }
                    }
                }
                DiffOp::Clear => {
                    self.cells.fill(CellState::default());
                }
            }
        }
        self.cursor = diff.cursor;
        self.modes = diff.modes;
        self.generation += 1;
    }

    /// Whether the terminal is in application-cursor mode.
    pub fn app_cursor(&self) -> bool {
        self.modes.app_cursor()
    }

    /// Reset to blank cells.
    pub fn clear(&mut self) {
        self.cells.fill(CellState::default());
        self.cursor = CursorState::default();
        self.modes = TermModes::EMPTY;
        self.generation += 1;
    }

    /// Resize the grid (server will send a fresh snapshot after resize).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows as usize;
        self.cols = cols as usize;
        self.cells = vec![CellState::default(); self.rows * self.cols];
        self.generation += 1;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl Default for CellGrid {
    fn default() -> Self {
        Self::new(24, 80)
    }
}

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
    generation: u64,
    metrics: Option<MetricsSnapshot>,
}

impl<'a> GridView<'a> {
    fn from_grid(grid: &'a CellGrid, metrics: Option<MetricsSnapshot>) -> Self {
        Self {
            cells: &grid.cells,
            cursor: grid.cursor,
            rows: grid.rows,
            cols: grid.cols,
            generation: grid.generation(),
            metrics,
        }
    }
}

/// State preserved across canvas redraws.
///
/// The `cells_cache` is cleared on every diff (tracked by `last_generation`)
/// so that any server-side change always produces a fresh render.
/// FPS measurement window in seconds.
const FPS_WINDOW_SECS: f64 = 1.0;

pub struct CanvasState {
    rows: u16,
    cols: u16,
    cells_cache: canvas::Cache,
    cursor_cache: canvas::Cache,
    last_generation: Cell<u64>,
    draw_duration_ms: Cell<f64>,
    /// Circular buffer of draw timestamps for FPS calculation.
    draw_timestamps: std::cell::RefCell<std::collections::VecDeque<Instant>>,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            rows: 0,
            cols: 0,
            cells_cache: canvas::Cache::default(),
            cursor_cache: canvas::Cache::default(),
            last_generation: Cell::new(0),
            draw_duration_ms: Cell::new(0.0),
            draw_timestamps: std::cell::RefCell::new(std::collections::VecDeque::with_capacity(
                128,
            )),
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

fn cell_color_to_iced(c: smux_protocol::messages::CellColor) -> IcedColor {
    IcedColor::from_rgb8(c.r, c.g, c.b)
}

fn default_bg() -> IcedColor {
    IcedColor::from_rgb8(0x28, 0x2c, 0x34)
}

impl<'a> canvas::Program<Message> for GridView<'a> {
    type State = CanvasState;

    fn update(
        &self,
        state: &mut CanvasState,
        _event: canvas::Event,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
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

    fn draw(
        &self,
        state: &CanvasState,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let draw_start = Instant::now();

        // Clear the cell cache on every diff so that any server-side change
        // (cell, cursor, or mode) produces a fresh render.
        if self.generation != state.last_generation.get() {
            state.last_generation.set(self.generation);
            state.cells_cache.clear();
        }

        let cells = self.cells;
        let cols = self.cols;
        let rows = self.rows;
        let cursor = &self.cursor;

        // Layer 1: cells (cached -- redrawn only when cells change)
        let cells_geom = state.cells_cache.draw(renderer, bounds.size(), |frame| {
            frame.fill_rectangle(IcedPoint::ORIGIN, bounds.size(), default_bg());

            for (idx, cell) in cells.iter().enumerate() {
                let row = idx / cols;
                let col = idx % cols;
                let x = col as f32 * CELL_WIDTH;
                let y = row as f32 * CELL_HEIGHT;

                let bg = cell_color_to_iced(cell.bg);
                frame.fill_rectangle(IcedPoint::new(x, y), Size::new(CELL_WIDTH, CELL_HEIGHT), bg);

                let hidden = cell.attrs.contains(CellAttrs::HIDDEN);
                if !hidden && cell.c != ' ' {
                    let fg = cell_color_to_iced(cell.fg);
                    frame.fill_text(Text {
                        content: cell.c.to_string(),
                        position: IcedPoint::new(x, y),
                        color: fg,
                        size: Pixels(FONT_SIZE),
                        line_height: iced::widget::text::LineHeight::Absolute(Pixels(CELL_HEIGHT)),
                        font: Font::MONOSPACE,
                        horizontal_alignment: alignment::Horizontal::Left,
                        vertical_alignment: alignment::Vertical::Top,
                        shaping: iced::widget::text::Shaping::Basic,
                    });
                }
            }
        });

        // Layer 2: cursor (redrawn every frame -- always clear to pick up position changes)
        state.cursor_cache.clear();
        let cursor_geom = state.cursor_cache.draw(renderer, bounds.size(), |frame| {
            if cursor.visible && cursor.shape != CursorShape::Hidden {
                let cur_row = cursor.row as usize;
                let cur_col = cursor.col as usize;
                if cur_row < rows && cur_col < cols {
                    let x = cur_col as f32 * CELL_WIDTH;
                    let y = cur_row as f32 * CELL_HEIGHT;
                    let idx = cur_row * cols + cur_col;

                    match cursor.shape {
                        CursorShape::Block => {
                            frame.fill_rectangle(
                                IcedPoint::new(x, y),
                                Size::new(CELL_WIDTH, CELL_HEIGHT),
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
                                frame.fill_text(Text {
                                    content: cell.c.to_string(),
                                    position: IcedPoint::new(x, y),
                                    color: IcedColor::BLACK,
                                    size: Pixels(FONT_SIZE),
                                    line_height: iced::widget::text::LineHeight::Absolute(Pixels(
                                        CELL_HEIGHT,
                                    )),
                                    font: Font::MONOSPACE,
                                    horizontal_alignment: alignment::Horizontal::Left,
                                    vertical_alignment: alignment::Vertical::Top,
                                    shaping: iced::widget::text::Shaping::Basic,
                                });
                            }
                        }
                        CursorShape::Underline => {
                            frame.fill_rectangle(
                                IcedPoint::new(x, y + CELL_HEIGHT - 2.0),
                                Size::new(CELL_WIDTH, 2.0),
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
                                (0.0, 0.0, CELL_WIDTH, 1.0),
                                (0.0, CELL_HEIGHT - 1.0, CELL_WIDTH, 1.0),
                                (0.0, 0.0, 1.0, CELL_HEIGHT),
                                (CELL_WIDTH - 1.0, 0.0, 1.0, CELL_HEIGHT),
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

        // Layer 3: HUD overlay (never cached -- content changes every frame)
        if let Some(metrics) = self.metrics {
            let hud_geom = draw_hud(renderer, bounds, &metrics, prev_draw_ms, fps);
            vec![cells_geom, cursor_geom, hud_geom]
        } else {
            vec![cells_geom, cursor_geom]
        }
    }
}

/// Draw the HUD overlay as an uncached geometry layer.
fn draw_hud(
    renderer: &iced::Renderer,
    bounds: Rectangle,
    metrics: &MetricsSnapshot,
    draw_ms: f64,
    fps: f64,
) -> canvas::Geometry {
    const HUD_W: f32 = 280.0;
    const HUD_H: f32 = 110.0;
    const HUD_PAD: f32 = 8.0;
    const LINE_H: f32 = 18.0;
    const HUD_FONT_SIZE: f32 = 12.0;

    let mut frame = canvas::Frame::new(renderer, bounds.size());

    let hud_x = bounds.width - HUD_W - HUD_PAD;
    let hud_y = HUD_PAD;

    // Semi-transparent background
    frame.fill_rectangle(
        IcedPoint::new(hud_x, hud_y),
        Size::new(HUD_W, HUD_H),
        IcedColor {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.75,
        },
    );

    let green = IcedColor::from_rgb8(0x50, 0xfa, 0x7b);
    let lines = [
        format!(
            "Net+Apply: {:.1}ms avg / {:.1}ms max",
            metrics.net_apply_avg_ms, metrics.net_apply_max_ms
        ),
        format!("Apply:     {:.2}ms avg", metrics.apply_avg_ms),
        format!("Draw:      {:.1}ms (prev frame)", draw_ms),
        format!("Batch:     {:.1} msgs avg", metrics.batch_avg),
        format!("FPS:       {fps:.0}"),
    ];

    for (i, line) in lines.iter().enumerate() {
        frame.fill_text(Text {
            content: line.clone(),
            position: IcedPoint::new(hud_x + 8.0, hud_y + 6.0 + i as f32 * LINE_H),
            color: green,
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
) -> Element<'a, Message> {
    let snapshot = GridView::from_grid(grid, metrics);
    Canvas::new(snapshot)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smux_protocol::messages::{CellColor, GridSnapshot};

    #[test]
    fn apply_snapshot_sets_all_cells() {
        let mut grid = CellGrid::default();
        let cell_a = CellState {
            c: 'A',
            fg: CellColor::new(255, 0, 0),
            bg: CellColor::new(0, 0, 0),
            attrs: CellAttrs::EMPTY,
        };
        let snapshot = GridSnapshot {
            rows: 2,
            cols: 3,
            cells: vec![cell_a; 6],
            cursor: CursorState {
                row: 1,
                col: 2,
                shape: CursorShape::Block,
                visible: true,
            },
            modes: TermModes::EMPTY,
        };
        grid.apply_snapshot(snapshot);
        assert_eq!(grid.rows, 2);
        assert_eq!(grid.cols, 3);
        assert_eq!(grid.cells.len(), 6);
        assert_eq!(grid.cells[0].c, 'A');
        assert_eq!(grid.cursor.row, 1);
        assert_eq!(grid.cursor.col, 2);
    }

    #[test]
    fn apply_diff_cell_op() {
        let mut grid = CellGrid::new(2, 3);
        let cell_x = CellState {
            c: 'X',
            fg: CellColor::new(255, 255, 255),
            bg: CellColor::new(0, 0, 0),
            attrs: CellAttrs::EMPTY,
        };
        let diff = TerminalDiff {
            ops: vec![DiffOp::Cell {
                row: 0,
                col: 1,
                cell: cell_x,
            }],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
        };
        grid.apply_diff(diff);
        assert_eq!(grid.cells[1].c, 'X');
        assert_eq!(grid.cells[0].c, ' '); // unchanged
    }

    #[test]
    fn apply_diff_row_op() {
        let mut grid = CellGrid::new(2, 5);
        let cells = vec![
            CellState {
                c: 'H',
                ..CellState::default()
            },
            CellState {
                c: 'I',
                ..CellState::default()
            },
        ];
        let diff = TerminalDiff {
            ops: vec![DiffOp::Row {
                row: 1,
                start_col: 2,
                cells,
            }],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
        };
        grid.apply_diff(diff);
        assert_eq!(grid.cells[1 * 5 + 2].c, 'H');
        assert_eq!(grid.cells[1 * 5 + 3].c, 'I');
    }

    #[test]
    fn apply_diff_clear_op() {
        let mut grid = CellGrid::new(2, 3);
        grid.cells[0].c = 'Z';
        let diff = TerminalDiff {
            ops: vec![DiffOp::Clear],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
        };
        grid.apply_diff(diff);
        assert_eq!(grid.cells[0].c, ' ');
    }

    #[test]
    fn generation_increments() {
        let mut grid = CellGrid::new(2, 3);
        let g0 = grid.generation();
        grid.apply_diff(TerminalDiff {
            ops: vec![],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
        });
        assert_eq!(grid.generation(), g0 + 1);
        grid.clear();
        assert_eq!(grid.generation(), g0 + 2);
    }

    #[test]
    fn app_cursor_reflects_modes() {
        let mut grid = CellGrid::new(2, 3);
        assert!(!grid.app_cursor());
        grid.apply_diff(TerminalDiff {
            ops: vec![],
            cursor: CursorState::default(),
            modes: TermModes(TermModes::APP_CURSOR),
        });
        assert!(grid.app_cursor());
    }

    #[test]
    fn generation_increments_on_every_diff() {
        let mut grid = CellGrid::new(2, 3);
        let g0 = grid.generation();

        // Cursor-only diff: generation still increments
        grid.apply_diff(TerminalDiff {
            ops: vec![],
            cursor: CursorState {
                row: 1,
                col: 1,
                ..CursorState::default()
            },
            modes: TermModes::EMPTY,
        });
        assert_eq!(grid.generation(), g0 + 1);

        // Diff with cell ops: generation increments
        grid.apply_diff(TerminalDiff {
            ops: vec![DiffOp::Cell {
                row: 0,
                col: 0,
                cell: CellState {
                    c: 'A',
                    ..CellState::default()
                },
            }],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
        });
        assert_eq!(grid.generation(), g0 + 2);
    }
}
