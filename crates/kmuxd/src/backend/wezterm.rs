use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kmux_protocol::messages::{
    CellAttrs, CellColor, CellState, CursorShape, CursorState, TermModes,
};
use tattoy_wezterm_surface::{CursorShape as WezCursorShape, CursorVisibility};
use tattoy_wezterm_term::{
    Blink, CursorPosition, Intensity, Terminal, TerminalConfiguration, TerminalSize, Underline,
    color::{ColorAttribute, ColorPalette, SrgbaTuple},
};

use super::TerminalBackend;

const SCROLLBACK_LINES: usize = 50_000;

/// Terminal configuration for the kmux server-side emulator.
///
/// The kitty feature flags are backed by shared atomics so that the daemon
/// can update them at any time (e.g. when a client attaches or detaches)
/// without rebuilding the backend.  wezterm-term queries these flags on every
/// relevant escape-sequence handler, so changes take effect immediately.
#[derive(Debug)]
struct KmuxTerminalConfig {
    /// Whether to accept kitty graphics protocol sequences.
    /// Defaults to `false` until an attached client declares support.
    kitty_graphics: Arc<AtomicBool>,
    /// Whether to accept kitty keyboard enhancement sequences.
    /// Defaults to `false` until an attached client declares support.
    kitty_keyboard: Arc<AtomicBool>,
}

impl TerminalConfiguration for KmuxTerminalConfig {
    fn scrollback_size(&self) -> usize {
        SCROLLBACK_LINES
    }

    fn color_palette(&self) -> ColorPalette {
        // Use the default (xterm-256) palette. The server resolves all named /
        // indexed colours to RGB before sending to clients.  Clients then
        // substitute their own theme for cells that carried DEFAULT_FG/DEFAULT_BG.
        ColorPalette::default()
    }

    fn enable_kitty_graphics(&self) -> bool {
        // Queried per escape-sequence by wezterm-term, so changes to the
        // underlying atomic take effect on the very next advance_bytes call.
        // Phase A: image data is dropped in fill_cells_inner (TODO(images)).
        // We default this to false so we don't silently consume sequences that
        // no currently-attached client can render.
        self.kitty_graphics.load(Ordering::Relaxed)
    }

    fn enable_kitty_keyboard(&self) -> bool {
        self.kitty_keyboard.load(Ordering::Relaxed)
    }
}

/// VT emulator backend powered by `tattoy-wezterm-term`.
pub struct WezTermBackend {
    term: Terminal,
    rows: u16,
    cols: u16,
}

impl WezTermBackend {
    /// Create a new backend.
    ///
    /// `kitty_graphics` and `kitty_keyboard` are live atomics shared with the
    /// [`PaneRelay`][crate::app::PaneRelay].  The daemon updates them whenever
    /// the set of attached clients changes, and wezterm-term reads them on
    /// every relevant escape-sequence handler, so no rebuild is needed.
    pub fn new(
        rows: u16,
        cols: u16,
        kitty_graphics: Arc<AtomicBool>,
        kitty_keyboard: Arc<AtomicBool>,
    ) -> Self {
        let size = TerminalSize {
            rows: rows as usize,
            cols: cols as usize,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        };
        let term = Terminal::new(
            size,
            Arc::new(KmuxTerminalConfig {
                kitty_graphics,
                kitty_keyboard,
            }),
            "kmux",
            env!("CARGO_PKG_VERSION"),
            Box::new(std::io::sink()),
        );
        Self { term, rows, cols }
    }
}

impl TerminalBackend for WezTermBackend {
    fn feed(&mut self, data: &[u8]) {
        self.term.advance_bytes(data);
    }

    fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.term.resize(TerminalSize {
            rows: rows as usize,
            cols: cols as usize,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        });
    }

    fn cursor(&self) -> CursorState {
        convert_cursor(&self.term.cursor_pos())
    }

    fn modes(&self) -> TermModes {
        let mut bits: u16 = 0;
        // APP_CURSOR (DEC 1): wezterm exposes this via TerminalState directly.
        // bracketed_paste_enabled() covers DEC 2004.
        if self.term.bracketed_paste_enabled() {
            bits |= TermModes::BRACKETED_PASTE;
        }
        // is_mouse_grabbed() returns true when any mouse tracking mode is active
        // (DEC 1000/1002/1003).  We set MOUSE_REPORT_CLICK as a conservative proxy;
        // MOUSE_DRAG and MOUSE_MOTION are not individually distinguishable via the
        // public API in this version.
        //
        // TODO: file upstream PR to expose individual mouse mode bits.
        if self.term.is_mouse_grabbed() {
            bits |= TermModes::MOUSE_REPORT_CLICK;
        }
        // SGR mouse (DEC 1006) is also not separately accessible; the combined
        // is_mouse_grabbed() flag covers it at the reporting level clients care about.
        TermModes(bits)
    }

    fn is_alt_screen(&self) -> bool {
        self.term.is_alt_screen_active()
    }

    fn fill_cells(&self, out: &mut [CellState]) {
        self.fill_cells_inner(out);
    }

    fn fill_cells_and_cursor(&self, out: &mut [CellState]) -> (CursorState, TermModes) {
        self.fill_cells_inner(out);
        (self.cursor(), self.modes())
    }

    fn history_size(&self) -> usize {
        let screen = self.term.screen();
        screen
            .scrollback_rows()
            .saturating_sub(screen.physical_rows)
    }

    fn read_history_lines(&self, start: usize, count: usize, cols: usize) -> Vec<Vec<CellState>> {
        let screen = self.term.screen();
        let palette = self.term.palette();
        let hist_size = screen
            .scrollback_rows()
            .saturating_sub(screen.physical_rows);
        let end = (start + count).min(hist_size);
        if start >= end {
            return vec![];
        }
        let mut result = Vec::with_capacity(end - start);
        screen.with_phys_lines(start..end, |lines| {
            for line in lines {
                let mut row = vec![CellState::default(); cols];
                for cr in line.visible_cells() {
                    let col = cr.cell_index();
                    if col >= cols {
                        break;
                    }
                    let attrs = cr.attrs();
                    let cell_state = cell_state_from_attrs(
                        cr.str().chars().next().unwrap_or(' '),
                        cr.width(),
                        attrs,
                        &palette,
                    );
                    row[col] = cell_state;
                    if cr.width() > 1 && col + 1 < cols {
                        row[col + 1].attrs.0 |= CellAttrs::WIDE_CHAR_SPACER;
                    }
                }
                result.push(row);
            }
        });
        result
    }
}

impl WezTermBackend {
    fn fill_cells_inner(&self, out: &mut [CellState]) {
        let screen = self.term.screen();
        let palette = self.term.palette();
        let cols = self.cols as usize;
        let rows = self.rows as usize;

        for cell in out.iter_mut() {
            *cell = CellState::default();
        }

        let start = screen.phys_row(0);
        screen.with_phys_lines(start..start + rows, |lines| {
            for (row, line) in lines.iter().enumerate() {
                for cr in line.visible_cells() {
                    let col = cr.cell_index();
                    if col >= cols {
                        break;
                    }
                    let idx = row * cols + col;
                    if idx >= out.len() {
                        break;
                    }
                    let attrs = cr.attrs();
                    // TODO(images): attrs.images() carries kitty/sixel/iTerm2 image data.
                    // Phase A drops it silently; Phase B will extract and forward via wire
                    // protocol (requires extending kmux-protocol::messages::CellState).
                    out[idx] = cell_state_from_attrs(
                        cr.str().chars().next().unwrap_or(' '),
                        cr.width(),
                        attrs,
                        &palette,
                    );
                    // Mark the trailing half of a wide character.
                    if cr.width() > 1 && col + 1 < cols {
                        out[idx + 1].attrs.0 |= CellAttrs::WIDE_CHAR_SPACER;
                    }
                }
            }
        });
    }
}

// ─── Conversion helpers ───────────────────────────────────────────────────────

fn convert_cursor(pos: &CursorPosition) -> CursorState {
    let visible = pos.visibility == CursorVisibility::Visible;
    CursorState {
        row: pos.y.max(0) as u16,
        col: pos.x as u16,
        shape: if !visible {
            CursorShape::Hidden
        } else {
            convert_cursor_shape(pos.shape)
        },
        visible,
    }
}

fn convert_cursor_shape(shape: WezCursorShape) -> CursorShape {
    match shape {
        WezCursorShape::Default | WezCursorShape::BlinkingBlock | WezCursorShape::SteadyBlock => {
            CursorShape::Block
        }
        WezCursorShape::BlinkingUnderline | WezCursorShape::SteadyUnderline => {
            CursorShape::Underline
        }
        WezCursorShape::BlinkingBar | WezCursorShape::SteadyBar => CursorShape::Bar,
    }
}

fn cell_state_from_attrs(
    c: char,
    width: usize,
    attrs: &tattoy_wezterm_term::CellAttributes,
    palette: &ColorPalette,
) -> CellState {
    let is_inverse = attrs.reverse();

    // Resolve fg and bg, swapping if INVERSE is set.
    let (fg_attr, bg_attr) = if is_inverse {
        (attrs.background(), attrs.foreground())
    } else {
        (attrs.foreground(), attrs.background())
    };

    let fg = resolve_color(fg_attr, palette);
    let bg = resolve_color(bg_attr, palette);

    // Determine DEFAULT_FG / DEFAULT_BG after accounting for the swap.
    let orig_fg_is_default = attrs.foreground() == ColorAttribute::Default;
    let orig_bg_is_default = attrs.background() == ColorAttribute::Default;
    let (displayed_fg_is_default, displayed_bg_is_default) = if is_inverse {
        (orig_bg_is_default, orig_fg_is_default)
    } else {
        (orig_fg_is_default, orig_bg_is_default)
    };

    let mut bits: u16 = 0;
    // Intensity
    match attrs.intensity() {
        Intensity::Bold => bits |= CellAttrs::BOLD,
        Intensity::Half => bits |= CellAttrs::DIM,
        Intensity::Normal => {}
    }
    if attrs.italic() {
        bits |= CellAttrs::ITALIC;
    }
    if attrs.underline() != Underline::None {
        bits |= CellAttrs::UNDERLINE;
    }
    if attrs.strikethrough() {
        bits |= CellAttrs::STRIKETHROUGH;
    }
    if is_inverse {
        bits |= CellAttrs::INVERSE;
    }
    if attrs.invisible() {
        bits |= CellAttrs::HIDDEN;
    }
    if attrs.blink() != Blink::None {
        bits |= CellAttrs::BLINK;
    }
    if width > 1 {
        bits |= CellAttrs::WIDE_CHAR;
    }
    if displayed_fg_is_default {
        bits |= CellAttrs::DEFAULT_FG;
    }
    if displayed_bg_is_default {
        bits |= CellAttrs::DEFAULT_BG;
    }

    CellState {
        c,
        fg,
        bg,
        attrs: CellAttrs(bits),
    }
}

fn resolve_color(attr: ColorAttribute, palette: &ColorPalette) -> CellColor {
    let srgba: SrgbaTuple = palette.resolve_fg(attr);
    let (r, g, b, _) = srgba.as_rgba_u8();
    CellColor::new(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_engine::{DiffEngine, DiffResult};
    use kmux_protocol::messages::DiffOp;

    fn expect_cell_diff(result: DiffResult) -> kmux_protocol::messages::TerminalDiff {
        match result {
            DiffResult::CellDiff(diff) => diff,
            other => panic!("expected CellDiff, got {other:?}"),
        }
    }

    /// Construct a [`WezTermBackend`] for tests with both kitty flags off.
    fn test_backend(rows: u16, cols: u16) -> WezTermBackend {
        WezTermBackend::new(
            rows,
            cols,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn feed_hello_produces_5_cell_diff() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"hello");
        let diff = expect_cell_diff(ts.compute_diff());
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
    fn feed_red_text_has_correct_red_fg() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        // ANSI red (colour index 1) — resolved via the default palette.
        ts.feed(b"\x1b[31mred");
        let diff = expect_cell_diff(ts.compute_diff());
        let r_cell = diff
            .ops
            .iter()
            .find_map(|op| match op {
                DiffOp::Cell { cell, .. } if cell.c == 'r' => Some(*cell),
                DiffOp::Row { cells, .. } => cells.iter().find(|c| c.c == 'r').copied(),
                _ => None,
            })
            .expect("should find 'r' cell");
        // The default palette maps colour 1 to ANSI red.
        // We only assert it is non-zero and not full white.
        assert_ne!(
            r_cell.fg,
            CellColor::new(0xff, 0xff, 0xff),
            "'r' cell should not be white"
        );
        assert_ne!(
            r_cell.fg,
            CellColor::new(0, 0, 0),
            "'r' cell should not be black"
        );
    }

    #[test]
    fn feed_truecolor_text() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        // CSI 38;2;255;128;0 m — truecolor orange
        ts.feed(b"\x1b[38;2;255;128;0mX");
        let diff = expect_cell_diff(ts.compute_diff());
        let x_cell = diff
            .ops
            .iter()
            .find_map(|op| match op {
                DiffOp::Cell { cell, .. } if cell.c == 'X' => Some(*cell),
                DiffOp::Row { cells, .. } => cells.iter().find(|c| c.c == 'X').copied(),
                _ => None,
            })
            .expect("should find 'X' cell");
        assert_eq!(
            x_cell.fg,
            CellColor::new(255, 128, 0),
            "truecolor should pass through"
        );
    }

    #[test]
    fn snapshot_captures_full_grid() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"ABC");
        let snap = ts.snapshot();
        assert_eq!(snap.rows, 24);
        assert_eq!(snap.cols, 80);
        assert_eq!(snap.cells.len(), 24 * 80);
        assert_eq!(snap.cells[0].c, 'A');
        assert_eq!(snap.cells[1].c, 'B');
        assert_eq!(snap.cells[2].c, 'C');
    }

    #[test]
    fn cursor_tracks_position() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"hello");
        let snap = ts.snapshot();
        assert_eq!(snap.cursor.row, 0);
        assert_eq!(snap.cursor.col, 5);
    }

    #[test]
    fn second_feed_only_diffs_new_chars() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"hello");
        let _ = ts.compute_diff();
        ts.feed(b" world");
        let diff = expect_cell_diff(ts.compute_diff());
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
    fn fzf_highlight_move_produces_cell_diff() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"\x1b[?1049h\x1b[?1h\x1b[?25l");
        ts.feed(b"  item1\r\n");
        ts.feed(b"\x1b[7m> item2\x1b[27m\r\n");
        ts.feed(b"  item3\r\n");
        let _ = ts.compute_diff();

        ts.feed(b"\x1b[2;1H  item2");
        ts.feed(b"\x1b[1;1H\x1b[7m> item1\x1b[27m");
        let diff = expect_cell_diff(ts.compute_diff());
        assert!(!diff.ops.is_empty(), "highlight move must have cell ops");
    }

    #[test]
    fn hello_cursor_move_world_diffs_correctly() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"hello");
        let diff1 = expect_cell_diff(ts.compute_diff());
        let cells1: usize = diff1
            .ops
            .iter()
            .map(|op| match op {
                DiffOp::Cell { .. } => 1,
                DiffOp::Row { cells, .. } => cells.len(),
                DiffOp::Clear => 0,
            })
            .sum();
        assert!(cells1 >= 5);

        ts.feed(b"\x1b[3;1H world");
        let diff2 = expect_cell_diff(ts.compute_diff());
        let cells2: usize = diff2
            .ops
            .iter()
            .map(|op| match op {
                DiffOp::Cell { .. } => 1,
                DiffOp::Row { cells, .. } => cells.len(),
                DiffOp::Clear => 0,
            })
            .sum();
        assert!(
            cells2 >= 5,
            "expected at least 5 changed cells on second diff, got {cells2}"
        );
    }

    #[test]
    fn fzf_cursor_hidden_state() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"\x1b[?25l");
        let snap = ts.snapshot();
        assert!(
            !snap.cursor.visible,
            "cursor should be hidden after DECTCEM reset"
        );
    }

    #[test]
    fn bracketed_paste_mode_enable_disable() {
        let mut ts = DiffEngine::new(test_backend(24, 80));

        assert!(
            !ts.modes().bracketed_paste(),
            "bracketed paste should be off by default"
        );

        ts.feed(b"\x1b[?2004h");
        assert!(
            ts.modes().bracketed_paste(),
            "bracketed paste should be on after \\e[?2004h"
        );

        ts.feed(b"\x1b[?2004l");
        assert!(
            !ts.modes().bracketed_paste(),
            "bracketed paste should be off after \\e[?2004l"
        );
    }

    #[test]
    fn mouse_report_click_mode_enable_disable() {
        let mut ts = DiffEngine::new(test_backend(24, 80));

        assert!(
            !ts.modes().mouse_report(),
            "mouse reporting should be off by default"
        );

        ts.feed(b"\x1b[?1000h");
        assert!(
            ts.modes().mouse_report(),
            "mouse reporting should be on after \\e[?1000h"
        );

        ts.feed(b"\x1b[?1000l");
        assert!(
            !ts.modes().mouse_report(),
            "mouse reporting should be off after \\e[?1000l"
        );
    }

    #[test]
    fn fzf_rapid_navigation_cycle() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"\x1b[?1049h\x1b[?25l");
        ts.feed(b"\x1b[7m> item1\x1b[27m\r\n");
        ts.feed(b"  item2\r\n");
        ts.feed(b"  item3\r\n");
        ts.feed(b"  item4\r\n");
        ts.feed(b"  item5\r\n");
        let _ = ts.compute_diff();

        let moves = [
            &b"\x1b[1;1H  item1\x1b[2;1H\x1b[7m> item2\x1b[27m"[..],
            &b"\x1b[2;1H  item2\x1b[3;1H\x1b[7m> item3\x1b[27m"[..],
            &b"\x1b[3;1H  item3\x1b[4;1H\x1b[7m> item4\x1b[27m"[..],
            &b"\x1b[4;1H  item4\x1b[3;1H\x1b[7m> item3\x1b[27m"[..],
            &b"\x1b[3;1H  item3\x1b[2;1H\x1b[7m> item2\x1b[27m"[..],
        ];
        for (i, data) in moves.iter().enumerate() {
            ts.feed(data);
            let diff = expect_cell_diff(ts.compute_diff());
            assert!(
                !diff.ops.is_empty(),
                "navigation step {i} should produce cell ops"
            );
        }
    }

    #[test]
    fn alt_screen_no_scrollback_duplication() {
        let mut ts = DiffEngine::new(test_backend(4, 20));

        for i in 0..8 {
            ts.feed(format!("line {i}\r\n").as_bytes());
        }
        let diff = expect_cell_diff(ts.compute_diff());
        assert!(
            !diff.scrollback_lines.is_empty(),
            "should have generated scrollback"
        );

        ts.feed(b"\x1b[?1049h");
        ts.feed(b"fzf content");
        let diff = expect_cell_diff(ts.compute_diff());
        assert!(
            diff.scrollback_lines.is_empty(),
            "no scrollback on alt screen"
        );

        ts.feed(b"\x1b[?1049l");
        let diff = ts.compute_diff();
        if let DiffResult::CellDiff(d) = diff {
            assert!(
                d.scrollback_lines.is_empty(),
                "exiting alt screen should not re-send {} scrollback lines",
                d.scrollback_lines.len()
            );
        }
    }

    #[test]
    fn default_fg_bg_flags_on_plain_text() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"A");
        let diff = expect_cell_diff(ts.compute_diff());
        let a_cell = diff
            .ops
            .iter()
            .find_map(|op| match op {
                DiffOp::Cell { cell, .. } if cell.c == 'A' => Some(*cell),
                DiffOp::Row { cells, .. } => cells.iter().find(|c| c.c == 'A').copied(),
                _ => None,
            })
            .expect("should find 'A' cell");
        assert!(
            a_cell.attrs.0 & CellAttrs::DEFAULT_FG != 0,
            "plain text should have DEFAULT_FG set"
        );
        assert!(
            a_cell.attrs.0 & CellAttrs::DEFAULT_BG != 0,
            "plain text should have DEFAULT_BG set"
        );
    }

    #[test]
    fn bold_text_sets_bold_flag() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"\x1b[1mB");
        let diff = expect_cell_diff(ts.compute_diff());
        let b_cell = diff
            .ops
            .iter()
            .find_map(|op| match op {
                DiffOp::Cell { cell, .. } if cell.c == 'B' => Some(*cell),
                DiffOp::Row { cells, .. } => cells.iter().find(|c| c.c == 'B').copied(),
                _ => None,
            })
            .expect("should find 'B' cell");
        assert!(
            b_cell.attrs.0 & CellAttrs::BOLD != 0,
            "bold escape should set BOLD flag"
        );
    }

    #[test]
    fn wide_char_marks_spacer() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        // U+4e2d (中) is a CJK wide character
        ts.feed("中".as_bytes());
        let snap = ts.snapshot();
        assert_eq!(snap.cells[0].c, '中');
        assert!(
            snap.cells[0].attrs.0 & CellAttrs::WIDE_CHAR != 0,
            "wide char should have WIDE_CHAR flag"
        );
        assert!(
            snap.cells[1].attrs.0 & CellAttrs::WIDE_CHAR_SPACER != 0,
            "cell after wide char should have WIDE_CHAR_SPACER flag"
        );
    }

    #[test]
    fn resize_changes_grid_dimensions() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"hello");
        ts.resize(30, 120);
        let snap = ts.snapshot();
        assert_eq!(snap.rows, 30);
        assert_eq!(snap.cols, 120);
    }

    #[test]
    fn scrollback_lines_accumulated() {
        let mut ts = DiffEngine::new(test_backend(4, 20));
        for i in 0..8 {
            ts.feed(format!("line{i}\r\n").as_bytes());
        }
        let diff = expect_cell_diff(ts.compute_diff());
        assert!(
            !diff.scrollback_lines.is_empty(),
            "should have scrollback after printing past screen height"
        );
    }
}
