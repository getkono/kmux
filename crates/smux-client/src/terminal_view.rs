use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    term::{Config, RenderableCursor, Term, TermMode, cell::Flags, color::Colors},
    vte::ansi::{Color, CursorShape, NamedColor, Processor},
};
use iced::{
    Color as IcedColor, Element, Font, Length, Pixels, Point as IcedPoint, Rectangle, Size,
    alignment, mouse,
    widget::canvas::{self, Canvas, Text},
};

use crate::app::Message;

const CELL_WIDTH: f32 = 8.0;
const CELL_HEIGHT: f32 = 16.0;
const FONT_SIZE: f32 = 13.0;

/// Grid dimensions wrapper implementing the alacritty `Dimensions` trait.
struct TermDims {
    rows: usize,
    cols: usize,
}

impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Terminal state backed by `alacritty_terminal::Term`.
///
/// All raw PTY bytes are fed through the VTE `Processor` which handles the
/// full ANSI/VT escape sequence grammar and updates the grid in place.
pub struct TerminalBuffer {
    term: Term<VoidListener>,
    processor: Processor,
    pub rows: usize,
    pub cols: usize,
    generation: u64,
}

impl TerminalBuffer {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows as usize;
        let cols = cols as usize;
        let dims = TermDims { rows, cols };
        let term = Term::new(Config::default(), &dims, VoidListener);
        Self {
            term,
            processor: Processor::new(),
            rows,
            cols,
            generation: 0,
        }
    }

    /// Feed raw PTY bytes into the VTE parser → grid state is updated in place.
    pub fn push_bytes(&mut self, data: &[u8]) {
        self.processor.advance(&mut self.term, data);
        self.generation += 1;
    }

    /// Reset to a fresh terminal of the same dimensions.
    pub fn clear(&mut self) {
        let next_generation = self.generation + 1;
        *self = Self::new(self.rows as u16, self.cols as u16);
        self.generation = next_generation;
    }

    /// Resize the terminal grid (called when the canvas widget size changes).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows as usize;
        self.cols = cols as usize;
        self.term.resize(TermDims {
            rows: self.rows,
            cols: self.cols,
        });
        self.generation += 1;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the terminal is in application-cursor mode (vim arrow keys).
    pub fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }
}

impl Default for TerminalBuffer {
    fn default() -> Self {
        Self::new(24, 80)
    }
}

// ── Canvas rendering ──────────────────────────────────────────────────────────

/// Per-cell snapshot for rendering — fully owned, no lifetime ties to `Term`.
struct SnapshotCell {
    c: char,
    fg: IcedColor,
    bg: IcedColor,
    hidden: bool,
}

/// Snapshot of terminal grid state used as the canvas `Program`.
///
/// Created from `&TerminalBuffer` on every frame by copying cell data.
/// This keeps the iced widget tree free of lifetime complications.
pub struct TerminalSnapshot {
    cells: Vec<SnapshotCell>,
    cursor: RenderableCursor,
    display_offset: i32,
    rows: usize,
    cols: usize,
    generation: u64,
}

impl TerminalSnapshot {
    fn from_buffer(buf: &TerminalBuffer) -> Self {
        let content = buf.term.renderable_content();
        let colors = content.colors;
        let cursor = content.cursor;
        let display_offset = content.display_offset as i32;
        let rows = buf.rows;
        let cols = buf.cols;

        let mut cells: Vec<SnapshotCell> = (0..rows * cols)
            .map(|_| SnapshotCell {
                c: ' ',
                fg: default_fg(),
                bg: default_bg(),
                hidden: false,
            })
            .collect();

        for indexed in content.display_iter {
            // Map the grid line (can be negative for scrollback) to a screen row.
            let row = indexed.point.line.0 + display_offset;
            let col = indexed.point.column.0;
            if row >= 0 && (row as usize) < rows && col < cols {
                let cell = indexed.cell;
                let (fg, bg) = if cell.flags.contains(Flags::INVERSE) {
                    (
                        resolve_color(cell.bg, colors),
                        resolve_color(cell.fg, colors),
                    )
                } else {
                    (
                        resolve_color(cell.fg, colors),
                        resolve_color(cell.bg, colors),
                    )
                };
                cells[row as usize * cols + col] = SnapshotCell {
                    c: cell.c,
                    fg,
                    bg,
                    hidden: cell.flags.contains(Flags::HIDDEN),
                };
            }
        }

        Self {
            cells,
            cursor,
            display_offset,
            rows,
            cols,
            generation: buf.generation(),
        }
    }
}

/// State preserved across canvas redraws — used to detect grid-size changes
/// and cache rendered geometry between frames.
pub struct CanvasState {
    rows: u16,
    cols: u16,
    cache: canvas::Cache,
    last_generation: std::cell::Cell<u64>,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            rows: 0,
            cols: 0,
            cache: canvas::Cache::default(),
            last_generation: std::cell::Cell::new(0),
        }
    }
}

impl canvas::Program<Message> for TerminalSnapshot {
    type State = CanvasState;

    /// Detect resize: compare canvas `bounds` to stored dims on every event.
    ///
    /// When the bounds change we emit `Message::TerminalResized` so `app.rs`
    /// can update the `Term` grid and tell the server the new PTY size.
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
            state.cache.clear();
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
        // Invalidate cached geometry when terminal content has changed.
        if self.generation != state.last_generation.get() {
            state.last_generation.set(self.generation);
            state.cache.clear();
        }

        let cells = &self.cells;
        let cols = self.cols;
        let rows = self.rows;
        let cursor = &self.cursor;
        let display_offset = self.display_offset;

        let geom = state.cache.draw(renderer, bounds.size(), |frame| {
            // Fill background first to avoid transparent gaps.
            frame.fill_rectangle(IcedPoint::ORIGIN, bounds.size(), default_bg());

            // Draw each cell: background rectangle + character glyph.
            for (idx, cell) in cells.iter().enumerate() {
                let row = idx / cols;
                let col = idx % cols;
                let x = col as f32 * CELL_WIDTH;
                let y = row as f32 * CELL_HEIGHT;

                frame.fill_rectangle(
                    IcedPoint::new(x, y),
                    Size::new(CELL_WIDTH, CELL_HEIGHT),
                    cell.bg,
                );

                if !cell.hidden && cell.c != ' ' {
                    frame.fill_text(Text {
                        content: cell.c.to_string(),
                        position: IcedPoint::new(x, y),
                        color: cell.fg,
                        size: Pixels(FONT_SIZE),
                        line_height: iced::widget::text::LineHeight::Absolute(Pixels(CELL_HEIGHT)),
                        font: Font::MONOSPACE,
                        horizontal_alignment: alignment::Horizontal::Left,
                        vertical_alignment: alignment::Vertical::Top,
                        shaping: iced::widget::text::Shaping::Basic,
                    });
                }
            }

            // Draw cursor on top.
            if cursor.shape != CursorShape::Hidden {
                let cur_row = cursor.point.line.0 + display_offset;
                let cur_col = cursor.point.column.0 as i32;
                if cur_row >= 0
                    && (cur_row as usize) < rows
                    && cur_col >= 0
                    && (cur_col as usize) < cols
                {
                    let x = cur_col as f32 * CELL_WIDTH;
                    let y = cur_row as f32 * CELL_HEIGHT;
                    let idx = cur_row as usize * cols + cur_col as usize;

                    match cursor.shape {
                        CursorShape::Block => {
                            // Semi-transparent block so the character remains legible.
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
                            // Re-draw character with inverted foreground color.
                            if let Some(cell) = cells.get(idx)
                                && !cell.hidden
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
                        CursorShape::Beam => {
                            frame.fill_rectangle(
                                IcedPoint::new(x, y),
                                Size::new(2.0, CELL_HEIGHT),
                                IcedColor::WHITE,
                            );
                        }
                        // Hollow block: draw a thin border instead of a filled rectangle.
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

        vec![geom]
    }
}

/// Render the terminal buffer as a fill-parent canvas widget.
pub fn view<'a>(buffer: &'a TerminalBuffer, _session: &'a str) -> Element<'a, Message> {
    let snapshot = TerminalSnapshot::from_buffer(buffer);
    Canvas::new(snapshot)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

// ── Color resolution ──────────────────────────────────────────────────────────

fn resolve_color(color: Color, colors: &Colors) -> IcedColor {
    match color {
        Color::Named(name) => colors[name].map_or_else(
            || default_named_color(name),
            |rgb| IcedColor::from_rgb8(rgb.r, rgb.g, rgb.b),
        ),
        Color::Indexed(idx) => colors[idx as usize].map_or_else(
            || ansi_indexed_color(idx),
            |rgb| IcedColor::from_rgb8(rgb.r, rgb.g, rgb.b),
        ),
        Color::Spec(rgb) => IcedColor::from_rgb8(rgb.r, rgb.g, rgb.b),
    }
}

fn default_fg() -> IcedColor {
    IcedColor::from_rgb8(0xab, 0xb2, 0xbf)
}

fn default_bg() -> IcedColor {
    IcedColor::from_rgb8(0x28, 0x2c, 0x34)
}

/// Dark terminal theme fallback colors (One Dark palette).
fn default_named_color(name: NamedColor) -> IcedColor {
    match name {
        NamedColor::Black => IcedColor::from_rgb8(0x28, 0x2c, 0x34),
        NamedColor::Red => IcedColor::from_rgb8(0xe0, 0x6c, 0x75),
        NamedColor::Green => IcedColor::from_rgb8(0x98, 0xc3, 0x79),
        NamedColor::Yellow => IcedColor::from_rgb8(0xe5, 0xc0, 0x7b),
        NamedColor::Blue => IcedColor::from_rgb8(0x61, 0xaf, 0xef),
        NamedColor::Magenta => IcedColor::from_rgb8(0xc6, 0x78, 0xdd),
        NamedColor::Cyan => IcedColor::from_rgb8(0x56, 0xb6, 0xc2),
        NamedColor::White => IcedColor::from_rgb8(0xab, 0xb2, 0xbf),
        NamedColor::BrightBlack => IcedColor::from_rgb8(0x5c, 0x63, 0x70),
        NamedColor::BrightRed => IcedColor::from_rgb8(0xe0, 0x6c, 0x75),
        NamedColor::BrightGreen => IcedColor::from_rgb8(0x98, 0xc3, 0x79),
        NamedColor::BrightYellow => IcedColor::from_rgb8(0xe5, 0xc0, 0x7b),
        NamedColor::BrightBlue => IcedColor::from_rgb8(0x61, 0xaf, 0xef),
        NamedColor::BrightMagenta => IcedColor::from_rgb8(0xc6, 0x78, 0xdd),
        NamedColor::BrightCyan => IcedColor::from_rgb8(0x56, 0xb6, 0xc2),
        NamedColor::BrightWhite => IcedColor::from_rgb8(0xff, 0xff, 0xff),
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground => {
            default_fg()
        }
        NamedColor::Background => default_bg(),
        NamedColor::Cursor => IcedColor::from_rgb8(0x52, 0x8b, 0xff),
        NamedColor::DimBlack => IcedColor::from_rgb8(0x1e, 0x21, 0x27),
        NamedColor::DimRed => IcedColor::from_rgb8(0xa8, 0x51, 0x58),
        NamedColor::DimGreen => IcedColor::from_rgb8(0x72, 0x94, 0x5a),
        NamedColor::DimYellow => IcedColor::from_rgb8(0xac, 0x90, 0x5c),
        NamedColor::DimBlue => IcedColor::from_rgb8(0x49, 0x83, 0xb3),
        NamedColor::DimMagenta => IcedColor::from_rgb8(0x95, 0x5a, 0xa5),
        NamedColor::DimCyan => IcedColor::from_rgb8(0x40, 0x89, 0x91),
        NamedColor::DimWhite => IcedColor::from_rgb8(0x80, 0x87, 0x8f),
    }
}

/// Compute a color from the standard 256-color palette when the terminal
/// hasn't overridden it in `Colors`.
fn ansi_indexed_color(idx: u8) -> IcedColor {
    match idx {
        // Indices 0–15: map to named ANSI colors.
        0..=15 => {
            let name = match idx {
                0 => NamedColor::Black,
                1 => NamedColor::Red,
                2 => NamedColor::Green,
                3 => NamedColor::Yellow,
                4 => NamedColor::Blue,
                5 => NamedColor::Magenta,
                6 => NamedColor::Cyan,
                7 => NamedColor::White,
                8 => NamedColor::BrightBlack,
                9 => NamedColor::BrightRed,
                10 => NamedColor::BrightGreen,
                11 => NamedColor::BrightYellow,
                12 => NamedColor::BrightBlue,
                13 => NamedColor::BrightMagenta,
                14 => NamedColor::BrightCyan,
                15 => NamedColor::BrightWhite,
                _ => unreachable!(),
            };
            default_named_color(name)
        }
        // Indices 16–231: 6×6×6 RGB color cube.
        16..=231 => {
            let i = idx - 16;
            let r = (i / 36) % 6;
            let g = (i / 6) % 6;
            let b = i % 6;
            let to_byte = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
            IcedColor::from_rgb8(to_byte(r), to_byte(g), to_byte(b))
        }
        // Indices 232–255: 24-step grayscale ramp.
        232..=255 => {
            let v = (idx - 232) * 10 + 8;
            IcedColor::from_rgb8(v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::index::{Column, Line};

    use super::*;

    #[test]
    fn push_bytes_renders_plain_text() {
        let mut buf = TerminalBuffer::new(24, 80);
        buf.push_bytes(b"hello");
        let grid = buf.term.grid();
        assert_eq!(grid[Line(0)][Column(0)].c, 'h');
        assert_eq!(grid[Line(0)][Column(1)].c, 'e');
        assert_eq!(grid[Line(0)][Column(2)].c, 'l');
    }

    #[test]
    fn push_bytes_handles_color_escape() {
        let mut buf = TerminalBuffer::new(24, 80);
        buf.push_bytes(b"\x1b[31mred");
        let cell = &buf.term.grid()[Line(0)][Column(0)];
        assert_eq!(cell.c, 'r');
        assert_eq!(cell.fg, Color::Named(NamedColor::Red));
    }

    #[test]
    fn clear_resets_terminal() {
        let mut buf = TerminalBuffer::new(24, 80);
        buf.push_bytes(b"hello");
        buf.clear();
        let cell = &buf.term.grid()[Line(0)][Column(0)];
        assert_eq!(cell.c, ' ');
    }

    #[test]
    fn resize_updates_dimensions() {
        let mut buf = TerminalBuffer::new(24, 80);
        buf.resize(40, 120);
        assert_eq!(buf.rows, 40);
        assert_eq!(buf.cols, 120);
    }
}
