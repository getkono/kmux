use std::cell::Cell;
use std::collections::VecDeque;
use std::time::Instant;

use iced::{
    Color as IcedColor, Element, Font, Length, Pixels, Point as IcedPoint, Rectangle, Size,
    alignment,
    font::{Style, Weight},
    mouse,
    widget::canvas::{self, Canvas, Text},
};
use kmux_protocol::messages::{
    CellAttrs, CellColor, CellState, CursorShape, CursorState, DiffOp, GridSnapshot, TermModes,
    TerminalDiff,
};

use crate::event_log::DiagSnapshot;
use crate::metrics::MetricsSnapshot;

use crate::app::Message;

pub const CELL_WIDTH: f32 = 8.0;
pub const CELL_HEIGHT: f32 = 16.0;
const FONT_SIZE: f32 = 13.0;

// ── Selection types ──

/// An absolute position in the terminal's combined scrollback + visible grid.
/// Row 0 = oldest scrollback line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPos {
    pub row: usize,
    pub col: usize,
}

impl GridPos {
    fn min(a: GridPos, b: GridPos) -> GridPos {
        if (a.row, a.col) <= (b.row, b.col) {
            a
        } else {
            b
        }
    }
    fn max(a: GridPos, b: GridPos) -> GridPos {
        if (a.row, a.col) >= (b.row, b.col) {
            a
        } else {
            b
        }
    }
}

/// Selection mode based on click count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Normal,
    Word,
    Line,
}

/// A text selection range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: GridPos,
    pub end: GridPos,
    pub mode: SelectionMode,
}

impl Selection {
    /// The earlier (top-left) position.
    pub fn start(&self) -> GridPos {
        GridPos::min(self.anchor, self.end)
    }
    /// The later (bottom-right) position.
    pub fn end_pos(&self) -> GridPos {
        GridPos::max(self.anchor, self.end)
    }
}

/// Double/triple-click detection timeout.
const MULTI_CLICK_TIMEOUT_MS: u128 = 400;

/// Default background color (One Dark). Matches `CellState::default().bg`.
const DEFAULT_BG: CellColor = CellColor::new(0x28, 0x2c, 0x34);

/// Maximum number of scrollback lines per session.
const MAX_SCROLLBACK_LINES: usize = 50_000;

/// Ring buffer of scrollback lines, stored oldest-first.
pub struct ScrollbackBuffer {
    lines: VecDeque<Vec<CellState>>,
    max_lines: usize,
}

impl ScrollbackBuffer {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            max_lines,
        }
    }

    /// Push new scrollback lines (oldest first).
    pub fn push_lines(&mut self, new_lines: Vec<Vec<CellState>>) {
        for line in new_lines {
            if self.lines.len() >= self.max_lines {
                self.lines.pop_front();
            }
            self.lines.push_back(line);
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Clear all scrollback.
    pub fn clear(&mut self) {
        self.lines.clear();
    }
}

impl Default for ScrollbackBuffer {
    fn default() -> Self {
        Self::new(MAX_SCROLLBACK_LINES)
    }
}

/// Client-side grid state -- receives pre-resolved cells from the server.
///
/// Unlike the old `TerminalBuffer` that wrapped `alacritty_terminal::Term`,
/// this is a thin grid of `CellState` values. All VT parsing and color
/// resolution happens on the server.
///
/// Rendering uses generation-based cache invalidation: the canvas cache is
/// rebuilt whenever `cells_generation` changes, guaranteeing that every
/// server-side change is reflected in the render.
pub struct CellGrid {
    cells: Vec<CellState>,
    cursor: CursorState,
    modes: TermModes,
    pub rows: usize,
    pub cols: usize,
    /// Incremented only when cell ops are non-empty, or on snapshot/clear/resize.
    cells_generation: u64,
    /// Incremented on every update (cell, cursor-only, snapshot, clear, resize).
    cursor_generation: u64,
    /// Lines that have scrolled off the top of the visible area.
    scrollback: ScrollbackBuffer,
    /// Scroll offset from the bottom (0 = live view, >0 = scrolled into history).
    scroll_offset: usize,
    /// Current text selection, if any.
    selection: Option<Selection>,
}

impl CellGrid {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            cells: vec![CellState::default(); rows * cols],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            rows,
            cols,
            cells_generation: 0,
            cursor_generation: 0,
            scrollback: ScrollbackBuffer::default(),
            scroll_offset: 0,
            selection: None,
        }
    }

    /// Replace the entire grid from a server snapshot.
    pub fn apply_snapshot(&mut self, snapshot: GridSnapshot) {
        self.rows = snapshot.rows as usize;
        self.cols = snapshot.cols as usize;
        self.cells = snapshot.cells;
        self.cursor = snapshot.cursor;
        self.modes = snapshot.modes;
        self.scrollback.clear();
        self.scroll_offset = 0;
        self.selection = None;
        self.cells_generation += 1;
        self.cursor_generation += 1;
    }

    /// Apply a diff from the server -- only changed cells are updated.
    pub fn apply_diff(&mut self, diff: TerminalDiff) {
        // Push scrollback lines before applying cell changes.
        if !diff.scrollback_lines.is_empty() {
            let new_count = diff.scrollback_lines.len();
            let old_len = self.scrollback.len();
            self.scrollback.push_lines(diff.scrollback_lines);
            let actual_added = self.scrollback.len() - old_len + new_count.min(old_len);
            // Evicted = lines that were dropped from the front of the ring buffer.
            let evicted = new_count.saturating_sub(actual_added);
            if let Some(sel) = &mut self.selection {
                // Shift selection rows up by evicted lines, then down by new lines.
                if evicted > 0 && sel.anchor.row < evicted {
                    self.selection = None;
                } else if let Some(sel) = &mut self.selection {
                    let net = new_count - evicted;
                    sel.anchor.row = sel.anchor.row.saturating_sub(evicted) + net;
                    sel.end.row = sel.end.row.saturating_sub(evicted) + net;
                }
            }
        }

        let has_cell_ops = !diff.ops.is_empty();
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
        if has_cell_ops {
            self.cells_generation += 1;
        }
        self.cursor_generation += 1;
    }

    /// Apply a cursor-only update (no cell changes).
    pub fn apply_cursor_update(&mut self, cursor: CursorState, modes: TermModes) {
        self.cursor = cursor;
        self.modes = modes;
        self.cursor_generation += 1;
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
        self.scrollback.clear();
        self.scroll_offset = 0;
        self.selection = None;
        self.cells_generation += 1;
        self.cursor_generation += 1;
    }

    /// Resize the grid (server will send a fresh snapshot after resize).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows as usize;
        self.cols = cols as usize;
        self.cells = vec![CellState::default(); self.rows * self.cols];
        self.scroll_offset = 0;
        self.selection = None;
        self.cells_generation += 1;
        self.cursor_generation += 1;
    }

    /// Generation counter that changes on every update (used by iced to detect changes).
    pub fn generation(&self) -> u64 {
        self.cursor_generation
    }

    /// Generation counter that changes only when cells change (used for cache invalidation).
    pub fn cells_generation(&self) -> u64 {
        self.cells_generation
    }

    /// Scroll up by `n` lines into history.
    pub fn scroll_up(&mut self, n: usize) {
        let max_offset = self.scrollback.len();
        let new_offset = (self.scroll_offset + n).min(max_offset);
        if new_offset != self.scroll_offset {
            self.scroll_offset = new_offset;
            self.cells_generation += 1;
        }
    }

    /// Scroll down by `n` lines toward live view.
    pub fn scroll_down(&mut self, n: usize) {
        let new_offset = self.scroll_offset.saturating_sub(n);
        if new_offset != self.scroll_offset {
            self.scroll_offset = new_offset;
            self.cells_generation += 1;
        }
    }

    /// Snap to the bottom (live view).
    pub fn scroll_to_bottom(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset = 0;
            self.cells_generation += 1;
        }
    }

    /// Whether the view is scrolled up from live output.
    pub fn is_scrolled(&self) -> bool {
        self.scroll_offset > 0
    }

    /// Current scroll offset (0 = live, >0 = scrolled into history).
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Number of lines in scrollback history.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    // ── Selection ──

    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    pub fn set_selection(&mut self, sel: Option<Selection>) {
        self.selection = sel;
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Read the cell at an absolute grid position (scrollback + visible).
    fn cell_at(&self, pos: GridPos) -> Option<&CellState> {
        let sb_len = self.scrollback.len();
        if pos.row < sb_len {
            self.scrollback
                .lines
                .get(pos.row)
                .and_then(|line| line.get(pos.col))
        } else {
            let grid_row = pos.row - sb_len;
            if grid_row < self.rows && pos.col < self.cols {
                self.cells.get(grid_row * self.cols + pos.col)
            } else {
                None
            }
        }
    }

    /// Find word boundaries around `pos` for double-click selection.
    pub fn find_word_boundaries(&self, pos: GridPos) -> (GridPos, GridPos) {
        let ch = self.cell_at(pos).map(|c| c.c).unwrap_or(' ');
        let is_word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~');

        if is_word(ch) {
            let mut start_col = pos.col;
            while start_col > 0 {
                let prev = GridPos {
                    row: pos.row,
                    col: start_col - 1,
                };
                if self.cell_at(prev).map(|c| is_word(c.c)).unwrap_or(false) {
                    start_col -= 1;
                } else {
                    break;
                }
            }
            let max_col = self.cols.saturating_sub(1);
            let mut end_col = pos.col;
            while end_col < max_col {
                let next = GridPos {
                    row: pos.row,
                    col: end_col + 1,
                };
                if self.cell_at(next).map(|c| is_word(c.c)).unwrap_or(false) {
                    end_col += 1;
                } else {
                    break;
                }
            }
            (
                GridPos {
                    row: pos.row,
                    col: start_col,
                },
                GridPos {
                    row: pos.row,
                    col: end_col,
                },
            )
        } else {
            // Non-word: select just this character
            (pos, pos)
        }
    }

    /// Extract the text covered by the current selection.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection.as_ref()?;
        let start = sel.start();
        let end = sel.end_pos();
        if start == end {
            return None;
        }

        let mut result = String::new();
        for row in start.row..=end.row {
            let col_start = if row == start.row { start.col } else { 0 };
            let col_end = if row == end.row {
                end.col
            } else {
                self.cols.saturating_sub(1)
            };

            let mut line = String::new();
            for col in col_start..=col_end {
                let pos = GridPos { row, col };
                if let Some(cell) = self.cell_at(pos) {
                    if cell.attrs.contains(CellAttrs::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    line.push(cell.c);
                }
            }
            // Trim trailing whitespace per line.
            let trimmed = line.trim_end();
            if row > start.row {
                result.push('\n');
            }
            result.push_str(trimmed);
        }
        Some(result)
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
    cells_generation: u64,
    metrics: Option<MetricsSnapshot>,
    diag: Option<DiagSnapshot>,
    scroll_offset: usize,
    scrollback: &'a ScrollbackBuffer,
    selection: Option<Selection>,
}

impl<'a> GridView<'a> {
    fn from_grid(
        grid: &'a CellGrid,
        metrics: Option<MetricsSnapshot>,
        diag: Option<DiagSnapshot>,
    ) -> Self {
        Self {
            cells: &grid.cells,
            cursor: grid.cursor,
            rows: grid.rows,
            cols: grid.cols,
            cells_generation: grid.cells_generation(),
            metrics,
            diag,
            scroll_offset: grid.scroll_offset,
            scrollback: &grid.scrollback,
            selection: grid.selection,
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

fn cell_color_to_iced(c: kmux_protocol::messages::CellColor) -> IcedColor {
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
                return (
                    canvas::event::Status::Captured,
                    Some(Message::ScrollTerminal(lines)),
                );
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
                        if let Some(line) = scrollback.lines.get(sb_idx) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::{CellColor, GridSnapshot};

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
            scrollback_lines: vec![],
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
            scrollback_lines: vec![],
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
            scrollback_lines: vec![],
        };
        grid.apply_diff(diff);
        assert_eq!(grid.cells[0].c, ' ');
    }

    #[test]
    fn generation_increments() {
        let mut grid = CellGrid::new(2, 3);
        let g0 = grid.generation();
        grid.apply_diff(TerminalDiff {
            ops: vec![DiffOp::Cell {
                row: 0,
                col: 0,
                cell: CellState::default(),
            }],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            scrollback_lines: vec![],
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
            scrollback_lines: vec![],
        });
        assert!(grid.app_cursor());
    }

    #[test]
    fn generation_increments_on_every_diff() {
        let mut grid = CellGrid::new(2, 3);
        let g0 = grid.generation();

        // Cursor-only diff: cursor_generation increments, cells_generation does not
        grid.apply_diff(TerminalDiff {
            ops: vec![],
            cursor: CursorState {
                row: 1,
                col: 1,
                ..CursorState::default()
            },
            modes: TermModes::EMPTY,
            scrollback_lines: vec![],
        });
        assert_eq!(grid.generation(), g0 + 1);
        assert_eq!(grid.cells_generation(), 0);

        // Diff with cell ops: both generations increment
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
            scrollback_lines: vec![],
        });
        assert_eq!(grid.generation(), g0 + 2);
        assert_eq!(grid.cells_generation(), 1);
    }

    #[test]
    fn cursor_update_does_not_bump_cells_generation() {
        let mut grid = CellGrid::new(2, 3);
        let cg0 = grid.cells_generation();
        let g0 = grid.generation();

        grid.apply_cursor_update(
            CursorState {
                row: 1,
                col: 2,
                ..CursorState::default()
            },
            TermModes::EMPTY,
        );

        assert_eq!(
            grid.cells_generation(),
            cg0,
            "cells_generation should not change"
        );
        assert_eq!(
            grid.generation(),
            g0 + 1,
            "cursor_generation should increment"
        );
        assert_eq!(grid.cursor.row, 1);
        assert_eq!(grid.cursor.col, 2);
    }

    #[test]
    fn cell_diff_bumps_both_generations() {
        let mut grid = CellGrid::new(2, 3);
        let cg0 = grid.cells_generation();
        let g0 = grid.generation();

        grid.apply_diff(TerminalDiff {
            ops: vec![DiffOp::Cell {
                row: 0,
                col: 0,
                cell: CellState {
                    c: 'X',
                    ..CellState::default()
                },
            }],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            scrollback_lines: vec![],
        });

        assert_eq!(grid.cells_generation(), cg0 + 1);
        assert_eq!(grid.generation(), g0 + 1);
    }

    #[test]
    fn empty_ops_diff_bumps_only_cursor_generation() {
        let mut grid = CellGrid::new(2, 3);
        let cg0 = grid.cells_generation();
        let g0 = grid.generation();

        grid.apply_diff(TerminalDiff {
            ops: vec![],
            cursor: CursorState {
                row: 1,
                col: 1,
                ..CursorState::default()
            },
            modes: TermModes::EMPTY,
            scrollback_lines: vec![],
        });

        assert_eq!(
            grid.cells_generation(),
            cg0,
            "cells_generation should not change for empty ops"
        );
        assert_eq!(grid.generation(), g0 + 1);
    }

    #[test]
    fn rapid_diffs_all_reflected_in_generation() {
        let mut grid = CellGrid::new(24, 80);
        let g0 = grid.cells_generation();

        // Simulate 10 rapid diffs
        for i in 0..10u8 {
            grid.apply_diff(TerminalDiff {
                ops: vec![DiffOp::Cell {
                    row: (i % 24) as u16,
                    col: 0,
                    cell: CellState {
                        c: (b'A' + i) as char,
                        ..CellState::default()
                    },
                }],
                cursor: CursorState::default(),
                modes: TermModes::EMPTY,
                scrollback_lines: vec![],
            });
        }

        // Every diff bumped the generation
        assert_eq!(grid.cells_generation(), g0 + 10);

        // All cells reflect their latest values
        for i in 0..10u8 {
            let row = (i % 24) as usize;
            assert_eq!(grid.cells[row * 80].c, (b'A' + i) as char);
        }
    }

    #[test]
    fn snapshot_bumps_cells_generation() {
        let mut grid = CellGrid::new(2, 3);
        let cg0 = grid.cells_generation();

        grid.apply_snapshot(GridSnapshot {
            rows: 2,
            cols: 3,
            cells: vec![CellState::default(); 6],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
        });

        assert_eq!(grid.cells_generation(), cg0 + 1);
    }

    #[test]
    fn clear_bumps_cells_generation() {
        let mut grid = CellGrid::new(3, 4);
        let cg0 = grid.cells_generation();
        grid.clear();
        assert_eq!(grid.cells_generation(), cg0 + 1);
    }

    #[test]
    fn resize_bumps_cells_generation() {
        let mut grid = CellGrid::new(2, 3);
        let cg0 = grid.cells_generation();
        grid.resize(4, 5);
        assert_eq!(grid.cells_generation(), cg0 + 1);
        assert_eq!(grid.rows, 4);
        assert_eq!(grid.cols, 5);
    }

    #[test]
    fn wide_char_attrs_round_trip_through_diff() {
        let mut grid = CellGrid::new(1, 4);
        let wide_cell = CellState {
            c: '中',
            attrs: CellAttrs(CellAttrs::WIDE_CHAR),
            ..CellState::default()
        };
        let spacer_cell = CellState {
            c: ' ',
            attrs: CellAttrs(CellAttrs::WIDE_CHAR_SPACER),
            ..CellState::default()
        };
        let diff = TerminalDiff {
            ops: vec![
                DiffOp::Cell {
                    row: 0,
                    col: 0,
                    cell: wide_cell,
                },
                DiffOp::Cell {
                    row: 0,
                    col: 1,
                    cell: spacer_cell,
                },
            ],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            scrollback_lines: vec![],
        };
        grid.apply_diff(diff);
        assert_eq!(grid.cells[0].c, '中');
        assert!(grid.cells[0].attrs.contains(CellAttrs::WIDE_CHAR));
        assert!(grid.cells[1].attrs.contains(CellAttrs::WIDE_CHAR_SPACER));
    }

    #[test]
    fn bold_italic_attrs_preserved_through_diff() {
        let mut grid = CellGrid::new(1, 2);
        let bold_italic = CellState {
            c: 'A',
            attrs: CellAttrs(CellAttrs::BOLD | CellAttrs::ITALIC),
            ..CellState::default()
        };
        let diff = TerminalDiff {
            ops: vec![DiffOp::Cell {
                row: 0,
                col: 0,
                cell: bold_italic,
            }],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            scrollback_lines: vec![],
        };
        grid.apply_diff(diff);
        assert!(grid.cells[0].attrs.contains(CellAttrs::BOLD));
        assert!(grid.cells[0].attrs.contains(CellAttrs::ITALIC));
        assert!(!grid.cells[0].attrs.contains(CellAttrs::UNDERLINE));
    }
}
