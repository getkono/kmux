mod config;
mod convert;

use std::sync::Arc;

use kmux_protocol::messages::{CellAttrs, CellState, CursorState, TermModes};
use tattoy_wezterm_term::{Terminal, TerminalSize, terminal::Alert};

use crate::backend::{BackendConfig, BackendEventSink, BackendSize, TerminalBackend};
use config::KmuxTerminalConfig;
use convert::{cell_state_from_attrs, convert_cursor};

/// VT emulator backend powered by `tattoy-wezterm-term`.
pub struct WezTermBackend {
    pub(super) term: Terminal,
    pub(super) size: BackendSize,
}

/// Bridge from wezterm's `AlertHandler` to our `BackendEventSink`.
///
/// Holds a shared reference to the sink so the alert handler and the backend
/// struct can both reach the same sink.  Calls from wezterm are synchronous
/// inside `advance_bytes`; the sink MUST NOT block.
struct WezTermAlertBridge(Arc<dyn BackendEventSink>);

impl tattoy_wezterm_term::terminal::AlertHandler for WezTermAlertBridge {
    fn alert(&mut self, alert: Alert) {
        match alert {
            Alert::Bell => self.0.on_bell(),
            Alert::WindowTitleChanged(title) => self.0.on_title(&title),
            Alert::IconTitleChanged(Some(title)) => self.0.on_title(&title),
            _ => {}
        }
    }
}

fn make_wezterm_size(size: BackendSize) -> TerminalSize {
    TerminalSize {
        rows: size.rows as usize,
        cols: size.cols as usize,
        pixel_width: size.pixel_width as usize,
        pixel_height: size.pixel_height as usize,
        dpi: 0,
    }
}

impl TerminalBackend for WezTermBackend {
    fn new(cfg: BackendConfig) -> Self {
        let wez_size = make_wezterm_size(cfg.size);
        let mut term = Terminal::new(
            wez_size,
            Arc::new(KmuxTerminalConfig {
                kitty_graphics: cfg.capabilities.kitty_graphics,
                kitty_keyboard: cfg.capabilities.kitty_keyboard,
                scrollback: cfg.scrollback,
            }),
            "kmux",
            env!("CARGO_PKG_VERSION"),
            Box::new(std::io::sink()),
        );
        term.set_notification_handler(Box::new(WezTermAlertBridge(cfg.events)));
        Self {
            term,
            size: cfg.size,
        }
    }

    fn name() -> &'static str
    where
        Self: Sized,
    {
        "wezterm"
    }

    fn feed(&mut self, data: &[u8]) {
        self.term.advance_bytes(data);
    }

    fn size(&self) -> BackendSize {
        self.size
    }

    fn resize(&mut self, size: BackendSize) {
        self.size = size;
        self.term.resize(make_wezterm_size(size));
    }

    fn cursor(&self) -> CursorState {
        convert_cursor(&self.term.cursor_pos())
    }

    fn modes(&self) -> TermModes {
        let mut bits: u16 = 0;
        if self.term.bracketed_paste_enabled() {
            bits |= TermModes::BRACKETED_PASTE;
        }
        // is_mouse_grabbed() returns true when any mouse tracking mode is active
        // (DEC 1000/1002/1003).  MOUSE_REPORT_CLICK is used as a conservative
        // proxy; individual mouse mode bits are not distinguishable in this API.
        //
        // TODO: file upstream PR to expose individual mouse mode bits.
        if self.term.is_mouse_grabbed() {
            bits |= TermModes::MOUSE_REPORT_CLICK;
        }
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
        let cols = self.size.cols as usize;
        let rows = self.size.rows as usize;

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
                    if cr.width() > 1 && col + 1 < cols {
                        out[idx + 1].attrs.0 |= CellAttrs::WIDE_CHAR_SPACER;
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Mutex, OnceLock};

    use super::*;
    use crate::backend::{BackendEventSink, CapabilityHandles, NullEventSink};
    use crate::diff_engine::{DiffEngine, DiffResult};
    use kmux_protocol::messages::{CellColor, DiffOp};

    fn expect_cell_diff(result: DiffResult) -> kmux_protocol::messages::TerminalDiff {
        match result {
            DiffResult::CellDiff(diff) => diff,
            other => panic!("expected CellDiff, got {other:?}"),
        }
    }

    fn test_cfg(rows: u16, cols: u16) -> BackendConfig {
        BackendConfig {
            size: BackendSize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
            capabilities: CapabilityHandles {
                kitty_graphics: Arc::new(AtomicBool::new(false)),
                kitty_keyboard: Arc::new(AtomicBool::new(false)),
            },
            events: Arc::new(NullEventSink),
            scrollback: 1_000,
        }
    }

    fn test_backend(rows: u16, cols: u16) -> WezTermBackend {
        WezTermBackend::new(test_cfg(rows, cols))
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
        ts.resize(BackendSize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        });
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

    #[test]
    fn resize_with_pixel_dims_roundtrip() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        let new_size = BackendSize {
            rows: 40,
            cols: 132,
            pixel_width: 1056,
            pixel_height: 640,
        };
        ts.resize(new_size);
        let reported = ts.backend.size();
        assert_eq!(reported.rows, 40);
        assert_eq!(reported.cols, 132);
        assert_eq!(reported.pixel_width, 1056);
        assert_eq!(reported.pixel_height, 640);
    }

    /// Verifies that the event sink receives title changes from OSC-0 sequences.
    #[test]
    fn event_sink_receives_title() {
        struct TitleCapture(Mutex<Vec<String>>);
        impl BackendEventSink for TitleCapture {
            fn on_title(&self, title: &str) {
                self.0.lock().unwrap().push(title.to_string());
            }
        }

        static SINK: OnceLock<Arc<TitleCapture>> = OnceLock::new();
        let sink = SINK.get_or_init(|| Arc::new(TitleCapture(Mutex::new(vec![]))));

        let cfg = BackendConfig {
            size: BackendSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            capabilities: CapabilityHandles {
                kitty_graphics: Arc::new(AtomicBool::new(false)),
                kitty_keyboard: Arc::new(AtomicBool::new(false)),
            },
            events: Arc::clone(sink) as Arc<dyn BackendEventSink>,
            scrollback: 1_000,
        };

        let mut backend = WezTermBackend::new(cfg);
        backend.feed(b"\x1b]0;My Terminal Title\x07");

        let titles = sink.0.lock().unwrap();
        assert!(
            titles.iter().any(|t| t.contains("My Terminal Title")),
            "expected title event for 'My Terminal Title', got: {titles:?}"
        );
    }
}
