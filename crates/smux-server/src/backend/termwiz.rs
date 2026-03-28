use smux_protocol::messages::{
    CellAttrs, CellColor, CellState, CursorShape, CursorState, TermModes,
};
use termwiz::cell::Intensity;
use termwiz::color::{ColorAttribute, ColorSpec};
use termwiz::escape::csi::{
    CSI, Cursor, DecPrivateMode, DecPrivateModeCode, Edit, EraseInDisplay, EraseInLine, Mode, Sgr,
};
use termwiz::escape::parser::Parser;
use termwiz::escape::{Action, ControlCode};
use termwiz::surface::change::Change;
use termwiz::surface::{CursorVisibility, Position, Surface};

use super::TerminalBackend;

/// VT emulator backend powered by termwiz `Surface` + `Parser`.
///
/// Parses raw VTE bytes into termwiz `Action`s, translates them into
/// `Change`s, and applies them to an in-memory `Surface`. Covers the
/// most commonly used VTE sequences; exotic sequences are silently ignored.
pub struct TermwizBackend {
    parser: Parser,
    surface: Surface,
    app_cursor: bool,
    rows: u16,
    cols: u16,
}

impl TermwizBackend {
    pub fn new(rows: u16, cols: u16) -> Self {
        let surface = Surface::new(cols as usize, rows as usize);
        Self {
            parser: Parser::new(),
            surface,
            app_cursor: false,
            rows,
            cols,
        }
    }

    fn apply_actions(&mut self, actions: Vec<Action>) {
        for action in actions {
            self.apply_action(action);
        }
    }

    fn apply_action(&mut self, action: Action) {
        match action {
            Action::Print(c) => {
                self.surface.add_change(Change::Text(c.to_string()));
            }
            Action::PrintString(s) => {
                self.surface.add_change(Change::Text(s));
            }
            Action::Control(code) => self.apply_control(code),
            Action::CSI(csi) => self.apply_csi(csi),
            Action::Esc(esc) => self.apply_esc(esc),
            // OSC, DeviceControl, etc. — silently ignored
            _ => {}
        }
    }

    fn apply_control(&mut self, code: ControlCode) {
        match code {
            ControlCode::CarriageReturn => {
                self.surface.add_change(Change::Text("\r".to_string()));
            }
            ControlCode::LineFeed | ControlCode::VerticalTab | ControlCode::FormFeed => {
                self.surface.add_change(Change::Text("\n".to_string()));
            }
            ControlCode::Backspace => {
                let (x, _y) = self.surface.cursor_position();
                if x > 0 {
                    self.surface.add_change(Change::CursorPosition {
                        x: Position::Relative(-1),
                        y: Position::Relative(0),
                    });
                }
            }
            ControlCode::HorizontalTab => {
                let (x, _y) = self.surface.cursor_position();
                let next_tab = ((x / 8) + 1) * 8;
                let spaces = next_tab.saturating_sub(x).min(self.cols as usize - x);
                if spaces > 0 {
                    self.surface.add_change(Change::Text(" ".repeat(spaces)));
                }
            }
            ControlCode::Bell => {} // no visual effect
            _ => {}
        }
    }

    fn apply_csi(&mut self, csi: CSI) {
        match csi {
            CSI::Sgr(sgr) => self.apply_sgr(sgr),
            CSI::Cursor(cursor) => self.apply_cursor(cursor),
            CSI::Edit(edit) => self.apply_edit(edit),
            CSI::Mode(mode) => self.apply_mode(mode),
            _ => {}
        }
    }

    fn apply_sgr(&mut self, sgr: Sgr) {
        use termwiz::cell::AttributeChange;
        match sgr {
            Sgr::Reset => {
                self.surface
                    .add_change(Change::AllAttributes(Default::default()));
            }
            Sgr::Intensity(i) => {
                self.surface
                    .add_change(Change::Attribute(AttributeChange::Intensity(i)));
            }
            Sgr::Underline(u) => {
                self.surface
                    .add_change(Change::Attribute(AttributeChange::Underline(u)));
            }
            Sgr::Blink(b) => {
                self.surface
                    .add_change(Change::Attribute(AttributeChange::Blink(b)));
            }
            Sgr::Italic(on) => {
                self.surface
                    .add_change(Change::Attribute(AttributeChange::Italic(on)));
            }
            Sgr::Inverse(on) => {
                self.surface
                    .add_change(Change::Attribute(AttributeChange::Reverse(on)));
            }
            Sgr::Invisible(on) => {
                self.surface
                    .add_change(Change::Attribute(AttributeChange::Invisible(on)));
            }
            Sgr::StrikeThrough(on) => {
                self.surface
                    .add_change(Change::Attribute(AttributeChange::StrikeThrough(on)));
            }
            Sgr::Foreground(spec) => {
                self.surface
                    .add_change(Change::Attribute(AttributeChange::Foreground(
                        colorspec_to_attr(spec),
                    )));
            }
            Sgr::Background(spec) => {
                self.surface
                    .add_change(Change::Attribute(AttributeChange::Background(
                        colorspec_to_attr(spec),
                    )));
            }
            _ => {}
        }
    }

    fn apply_cursor(&mut self, cursor: Cursor) {
        match cursor {
            Cursor::Position { line, col } => {
                self.surface.add_change(Change::CursorPosition {
                    x: Position::Absolute(col.as_zero_based() as usize),
                    y: Position::Absolute(line.as_zero_based() as usize),
                });
            }
            Cursor::CharacterAndLinePosition { line, col } => {
                self.surface.add_change(Change::CursorPosition {
                    x: Position::Absolute(col.as_zero_based() as usize),
                    y: Position::Absolute(line.as_zero_based() as usize),
                });
            }
            Cursor::Up(n) => {
                self.surface.add_change(Change::CursorPosition {
                    x: Position::Relative(0),
                    y: Position::Relative(-(n as isize)),
                });
            }
            Cursor::Down(n) => {
                self.surface.add_change(Change::CursorPosition {
                    x: Position::Relative(0),
                    y: Position::Relative(n as isize),
                });
            }
            Cursor::Right(n) => {
                self.surface.add_change(Change::CursorPosition {
                    x: Position::Relative(n as isize),
                    y: Position::Relative(0),
                });
            }
            Cursor::Left(n) => {
                self.surface.add_change(Change::CursorPosition {
                    x: Position::Relative(-(n as isize)),
                    y: Position::Relative(0),
                });
            }
            Cursor::CharacterAbsolute(col) => {
                self.surface.add_change(Change::CursorPosition {
                    x: Position::Absolute(col.as_zero_based() as usize),
                    y: Position::Relative(0),
                });
            }
            Cursor::LinePositionAbsolute(line) => {
                self.surface.add_change(Change::CursorPosition {
                    x: Position::Relative(0),
                    y: Position::Absolute(line.saturating_sub(1) as usize),
                });
            }
            Cursor::NextLine(n) => {
                self.surface.add_change(Change::CursorPosition {
                    x: Position::Absolute(0),
                    y: Position::Relative(n as isize),
                });
            }
            Cursor::PrecedingLine(n) => {
                self.surface.add_change(Change::CursorPosition {
                    x: Position::Absolute(0),
                    y: Position::Relative(-(n as isize)),
                });
            }
            Cursor::SetTopAndBottomMargins { .. } => {
                // Scroll region — Surface handles this via ScrollRegion changes
            }
            Cursor::CursorStyle(style) => {
                use termwiz::escape::csi::CursorStyle;
                let shape = match style {
                    CursorStyle::Default
                    | CursorStyle::BlinkingBlock
                    | CursorStyle::SteadyBlock => termwiz::surface::CursorShape::Default,
                    CursorStyle::BlinkingUnderline | CursorStyle::SteadyUnderline => {
                        termwiz::surface::CursorShape::BlinkingUnderline
                    }
                    CursorStyle::BlinkingBar | CursorStyle::SteadyBar => {
                        termwiz::surface::CursorShape::BlinkingBar
                    }
                };
                self.surface.add_change(Change::CursorShape(shape));
            }
            _ => {}
        }
    }

    fn apply_edit(&mut self, edit: Edit) {
        match edit {
            Edit::EraseInDisplay(eid) => match eid {
                EraseInDisplay::EraseDisplay => {
                    self.surface
                        .add_change(Change::ClearScreen(Default::default()));
                }
                EraseInDisplay::EraseToEndOfDisplay => {
                    self.surface
                        .add_change(Change::ClearToEndOfScreen(Default::default()));
                }
                _ => {}
            },
            Edit::EraseInLine(eil) => match eil {
                EraseInLine::EraseToEndOfLine => {
                    self.surface
                        .add_change(Change::ClearToEndOfLine(Default::default()));
                }
                EraseInLine::EraseLine => {
                    // Move to column 0, clear to end of line
                    self.surface.add_change(Change::CursorPosition {
                        x: Position::Absolute(0),
                        y: Position::Relative(0),
                    });
                    self.surface
                        .add_change(Change::ClearToEndOfLine(Default::default()));
                }
                _ => {}
            },
            Edit::ScrollUp(n) => {
                let rows = self.rows as usize;
                self.surface.add_change(Change::ScrollRegionUp {
                    first_row: 0,
                    region_size: rows,
                    scroll_count: n as usize,
                });
            }
            Edit::ScrollDown(n) => {
                let rows = self.rows as usize;
                self.surface.add_change(Change::ScrollRegionDown {
                    first_row: 0,
                    region_size: rows,
                    scroll_count: n as usize,
                });
            }
            Edit::InsertLine(n) => {
                let (_x, y) = self.surface.cursor_position();
                let rows = self.rows as usize;
                let region = rows.saturating_sub(y);
                if region > 0 {
                    self.surface.add_change(Change::ScrollRegionDown {
                        first_row: y,
                        region_size: region,
                        scroll_count: n as usize,
                    });
                }
            }
            Edit::DeleteLine(n) => {
                let (_x, y) = self.surface.cursor_position();
                let rows = self.rows as usize;
                let region = rows.saturating_sub(y);
                if region > 0 {
                    self.surface.add_change(Change::ScrollRegionUp {
                        first_row: y,
                        region_size: region,
                        scroll_count: n as usize,
                    });
                }
            }
            _ => {}
        }
    }

    fn apply_mode(&mut self, mode: Mode) {
        match mode {
            Mode::SetDecPrivateMode(DecPrivateMode::Code(code)) => {
                self.set_dec_mode(code, true);
            }
            Mode::ResetDecPrivateMode(DecPrivateMode::Code(code)) => {
                self.set_dec_mode(code, false);
            }
            _ => {}
        }
    }

    fn set_dec_mode(&mut self, code: DecPrivateModeCode, enable: bool) {
        match code {
            DecPrivateModeCode::ApplicationCursorKeys => {
                self.app_cursor = enable;
            }
            DecPrivateModeCode::ShowCursor => {
                self.surface.add_change(Change::CursorVisibility(if enable {
                    CursorVisibility::Visible
                } else {
                    CursorVisibility::Hidden
                }));
            }
            DecPrivateModeCode::ClearAndEnableAlternateScreen => {
                if enable {
                    self.surface
                        .add_change(Change::ClearScreen(Default::default()));
                }
            }
            _ => {}
        }
    }

    fn apply_esc(&mut self, esc: termwiz::escape::Esc) {
        use termwiz::escape::EscCode;
        if let termwiz::escape::Esc::Code(EscCode::ReverseIndex) = esc {
            // Reverse index: move cursor up one line, scrolling if at top
            let (_x, y) = self.surface.cursor_position();
            if y == 0 {
                let rows = self.rows as usize;
                self.surface.add_change(Change::ScrollRegionDown {
                    first_row: 0,
                    region_size: rows,
                    scroll_count: 1,
                });
            } else {
                self.surface.add_change(Change::CursorPosition {
                    x: Position::Relative(0),
                    y: Position::Relative(-1),
                });
            }
        }
    }
}

impl TerminalBackend for TermwizBackend {
    fn feed(&mut self, data: &[u8]) {
        let actions = self.parser.parse_as_vec(data);
        self.apply_actions(actions);
    }

    fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    fn fill_cells(&self, out: &mut [CellState]) {
        let cols = self.cols as usize;
        let rows = self.rows as usize;

        for (row_idx, line) in self.surface.screen_lines().iter().enumerate() {
            if row_idx >= rows {
                break;
            }
            for cell_ref in line.visible_cells() {
                let col_idx = cell_ref.cell_index();
                if col_idx >= cols {
                    break;
                }
                let attrs = cell_ref.attrs();
                let c = cell_ref.str().chars().next().unwrap_or(' ');

                let (fg, bg) = if attrs.reverse() {
                    (
                        resolve_bg_color(attrs.background()),
                        resolve_fg_color(attrs.foreground()),
                    )
                } else {
                    (
                        resolve_fg_color(attrs.foreground()),
                        resolve_bg_color(attrs.background()),
                    )
                };

                let mut bits: u16 = 0;
                if attrs.intensity() == Intensity::Bold {
                    bits |= CellAttrs::BOLD;
                }
                if attrs.italic() {
                    bits |= CellAttrs::ITALIC;
                }
                if attrs.underline() != termwiz::cell::Underline::None {
                    bits |= CellAttrs::UNDERLINE;
                }
                if attrs.strikethrough() {
                    bits |= CellAttrs::STRIKETHROUGH;
                }
                if attrs.reverse() {
                    bits |= CellAttrs::INVERSE;
                }
                if attrs.invisible() {
                    bits |= CellAttrs::HIDDEN;
                }
                if attrs.intensity() == Intensity::Half {
                    bits |= CellAttrs::DIM;
                }

                out[row_idx * cols + col_idx] = CellState {
                    c,
                    fg,
                    bg,
                    attrs: CellAttrs(bits),
                };
            }
        }
    }

    fn cursor(&self) -> CursorState {
        let (col, row) = self.surface.cursor_position();
        let visible = self.surface.cursor_visibility() == CursorVisibility::Visible;
        let shape = match self.surface.cursor_shape() {
            Some(termwiz::surface::CursorShape::BlinkingUnderline)
            | Some(termwiz::surface::CursorShape::SteadyUnderline) => CursorShape::Underline,
            Some(termwiz::surface::CursorShape::BlinkingBar)
            | Some(termwiz::surface::CursorShape::SteadyBar) => CursorShape::Bar,
            _ => CursorShape::Block,
        };
        CursorState {
            row: row as u16,
            col: col as u16,
            shape,
            visible,
        }
    }

    fn modes(&self) -> TermModes {
        let mut bits: u16 = 0;
        if self.app_cursor {
            bits |= TermModes::APP_CURSOR;
        }
        TermModes(bits)
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.surface.resize(cols as usize, rows as usize);
    }
}

/// Convert termwiz `ColorSpec` (from SGR parsing) to `ColorAttribute` (used by Surface cells).
fn colorspec_to_attr(spec: ColorSpec) -> ColorAttribute {
    match spec {
        ColorSpec::Default => ColorAttribute::Default,
        ColorSpec::PaletteIndex(idx) => ColorAttribute::PaletteIndex(idx),
        ColorSpec::TrueColor(srgba) => ColorAttribute::TrueColorWithDefaultFallback(srgba),
    }
}

/// Resolve a foreground `ColorAttribute` to RGB using One Dark palette.
fn resolve_fg_color(attr: ColorAttribute) -> CellColor {
    resolve_color_attr_with_default(attr, CellColor::new(0xab, 0xb2, 0xbf))
}

/// Resolve a background `ColorAttribute` to RGB using One Dark palette.
fn resolve_bg_color(attr: ColorAttribute) -> CellColor {
    resolve_color_attr_with_default(attr, CellColor::new(0x28, 0x2c, 0x34))
}

fn resolve_color_attr_with_default(attr: ColorAttribute, default: CellColor) -> CellColor {
    match attr {
        ColorAttribute::Default => default,
        ColorAttribute::PaletteIndex(idx) => ansi_indexed_color(idx),
        ColorAttribute::TrueColorWithPaletteFallback(srgba, _)
        | ColorAttribute::TrueColorWithDefaultFallback(srgba) => CellColor::new(
            (srgba.0 * 255.0) as u8,
            (srgba.1 * 255.0) as u8,
            (srgba.2 * 255.0) as u8,
        ),
    }
}

/// Map a 256-color palette index to RGB using the One Dark palette for 0-15.
fn ansi_indexed_color(idx: u8) -> CellColor {
    match idx {
        0 => CellColor::new(0x28, 0x2c, 0x34),  // Black
        1 => CellColor::new(0xe0, 0x6c, 0x75),  // Red
        2 => CellColor::new(0x98, 0xc3, 0x79),  // Green
        3 => CellColor::new(0xe5, 0xc0, 0x7b),  // Yellow
        4 => CellColor::new(0x61, 0xaf, 0xef),  // Blue
        5 => CellColor::new(0xc6, 0x78, 0xdd),  // Magenta
        6 => CellColor::new(0x56, 0xb6, 0xc2),  // Cyan
        7 => CellColor::new(0xab, 0xb2, 0xbf),  // White
        8 => CellColor::new(0x5c, 0x63, 0x70),  // BrightBlack
        9 => CellColor::new(0xe0, 0x6c, 0x75),  // BrightRed
        10 => CellColor::new(0x98, 0xc3, 0x79), // BrightGreen
        11 => CellColor::new(0xe5, 0xc0, 0x7b), // BrightYellow
        12 => CellColor::new(0x61, 0xaf, 0xef), // BrightBlue
        13 => CellColor::new(0xc6, 0x78, 0xdd), // BrightMagenta
        14 => CellColor::new(0x56, 0xb6, 0xc2), // BrightCyan
        15 => CellColor::new(0xff, 0xff, 0xff), // BrightWhite
        16..=231 => {
            let i = idx - 16;
            let r = (i / 36) % 6;
            let g = (i / 6) % 6;
            let b = i % 6;
            let to_byte = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
            CellColor::new(to_byte(r), to_byte(g), to_byte(b))
        }
        232..=255 => {
            let v = (idx - 232) * 10 + 8;
            CellColor::new(v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_engine::DiffEngine;
    use smux_protocol::messages::DiffOp;

    #[test]
    fn feed_hello_produces_cells() {
        let mut ts = DiffEngine::new(TermwizBackend::new(24, 80));
        ts.feed(b"hello");
        let diff = ts.compute_diff().expect("expected Some diff");
        let total_cells: usize = diff
            .ops
            .iter()
            .map(|op| match op {
                DiffOp::Cell { .. } => 1,
                DiffOp::Row { cells, .. } => cells.len(),
                DiffOp::Clear => 0,
            })
            .sum();
        assert!(
            total_cells >= 5,
            "expected at least 5 changed cells, got {total_cells}"
        );
    }

    #[test]
    fn feed_red_text_has_red_fg() {
        let mut ts = DiffEngine::new(TermwizBackend::new(24, 80));
        ts.feed(b"\x1b[31mred");
        let diff = ts.compute_diff().expect("expected Some diff");
        let r_cell = diff
            .ops
            .iter()
            .find_map(|op| match op {
                DiffOp::Cell { cell, .. } if cell.c == 'r' => Some(*cell),
                DiffOp::Row { cells, .. } => cells.iter().find(|c| c.c == 'r').copied(),
                _ => None,
            })
            .expect("should find 'r' cell");
        assert_eq!(r_cell.fg, CellColor::new(0xe0, 0x6c, 0x75));
    }

    #[test]
    fn snapshot_captures_grid() {
        let mut ts = DiffEngine::new(TermwizBackend::new(24, 80));
        ts.feed(b"ABC");
        let snap = ts.snapshot();
        assert_eq!(snap.rows, 24);
        assert_eq!(snap.cols, 80);
        assert_eq!(snap.cells[0].c, 'A');
        assert_eq!(snap.cells[1].c, 'B');
        assert_eq!(snap.cells[2].c, 'C');
    }

    #[test]
    fn cursor_tracks_position() {
        let mut ts = DiffEngine::new(TermwizBackend::new(24, 80));
        ts.feed(b"hello");
        let snap = ts.snapshot();
        assert_eq!(snap.cursor.row, 0);
        assert_eq!(snap.cursor.col, 5);
    }

    #[test]
    fn cursor_movement_via_csi() {
        let mut ts = DiffEngine::new(TermwizBackend::new(24, 80));
        ts.feed(b"hello");
        let _ = ts.compute_diff();

        // CSI H = cursor home (1;1)
        ts.feed(b"\x1b[H");
        let diff = ts.compute_diff().expect("cursor move should produce diff");
        assert_eq!(diff.cursor.row, 0);
        assert_eq!(diff.cursor.col, 0);
    }

    #[test]
    fn app_cursor_mode() {
        let mut ts = DiffEngine::new(TermwizBackend::new(24, 80));
        let _ = ts.compute_diff();

        // DECCKM set
        ts.feed(b"\x1b[?1h");
        let diff = ts.compute_diff().expect("mode change should produce diff");
        assert!(diff.modes.app_cursor());
    }

    #[test]
    fn clear_screen_produces_clear_op() {
        let mut ts = DiffEngine::new(TermwizBackend::new(24, 80));
        // Fill screen
        for _ in 0..24 {
            ts.feed(
                b"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
            );
        }
        let _ = ts.compute_diff();

        // CSI 2J + CSI H
        ts.feed(b"\x1b[2J\x1b[H");
        let diff = ts.compute_diff().expect("expected Some diff");
        assert!(
            matches!(diff.ops.as_slice(), [DiffOp::Clear]),
            "expected DiffOp::Clear, got {:?}",
            diff.ops.len()
        );
    }

    #[test]
    fn hide_cursor() {
        let mut ts = DiffEngine::new(TermwizBackend::new(24, 80));
        ts.feed(b"\x1b[?25l");
        let snap = ts.snapshot();
        assert!(!snap.cursor.visible);
    }
}
