//! Terminal backend powered by libghostty-vt (via `kmux-ghostty`).
//!
//! Wraps [`kmux_ghostty::GhosttyTerm`] in the [`TerminalBackend`] trait used
//! by [`DiffEngine`](crate::diff_engine::DiffEngine). Cell/cursor/modes data
//! is copied once per FFI crossing into caller-owned buffers — no heap churn
//! on the hot path beyond the scratch `Vec` inside [`GhosttyTerm::fill_cells`].

use std::sync::Arc;

use kmux_ghostty::{EventSink, GhosttyError, GhosttyTerm, TermSize};
use kmux_protocol::messages::{CellState, CursorState, KeyEvent as ProtoKeyEvent, TermModes};

use crate::backend::{BackendConfig, BackendEventSink, BackendSize, TerminalBackend};

/// Safe adapter: presents the kmuxd-level [`BackendEventSink`] to
/// `kmux-ghostty` as its crate-local [`EventSink`]. Both traits share the same
/// synchronous, non-blocking contract; this type exists only to bridge the
/// crate boundary without making `kmux-ghostty` depend on kmuxd.
struct EventSinkAdapter(Arc<dyn BackendEventSink>);

impl EventSink for EventSinkAdapter {
    fn on_title(&self, title: &str) {
        self.0.on_title(title);
    }

    fn on_bell(&self) {
        self.0.on_bell();
    }

    fn on_osc52(&self, selection: u8, base64: &[u8]) {
        // OSC 52 selection targets are ASCII letters (`c`/`p`/`s`/...);
        // fall back to `"c"` if the terminal emitted garbage.
        let sel = match selection {
            b'c' => "c",
            b'p' => "p",
            b's' => "s",
            b'0'..=b'7' => match selection {
                b'0' => "0",
                b'1' => "1",
                b'2' => "2",
                b'3' => "3",
                b'4' => "4",
                b'5' => "5",
                b'6' => "6",
                _ => "7",
            },
            _ => "c",
        };
        // `base64` is the still-encoded OSC 52 payload. Decoding is done
        // downstream (client-side) so the kmuxd just needs a printable copy.
        if let Ok(s) = std::str::from_utf8(base64) {
            self.0.on_osc52_copy(sel, s);
        }
    }

    fn on_hyperlink(&self, id: Option<&str>, uri: &str) {
        self.0.on_hyperlink(id, uri);
    }
}

/// VT emulator backend powered by libghostty-vt v1.3.1.
///
/// Thread-safety: `GhosttyTerm` is `Send` but not `Sync`; the `DiffEngine`
/// wrapping it in kmuxd is already protected by `Arc<Mutex<…>>` so only one
/// thread holds `&mut self` at a time.
pub struct GhosttyBackend {
    term: GhosttyTerm,
    size: BackendSize,
    events: Arc<dyn BackendEventSink>,
    last_title: String,
}

impl std::fmt::Debug for GhosttyBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhosttyBackend")
            .field("size", &self.size)
            .finish()
    }
}

fn to_term_size(s: BackendSize) -> TermSize {
    TermSize {
        rows: s.rows,
        cols: s.cols,
        pixel_width: s.pixel_width,
        pixel_height: s.pixel_height,
    }
}

impl TerminalBackend for GhosttyBackend {
    fn new(cfg: BackendConfig) -> Self {
        let scrollback = u32::try_from(cfg.scrollback).unwrap_or(u32::MAX);
        let events = Arc::clone(&cfg.events);
        let sink: Arc<dyn EventSink> = Arc::new(EventSinkAdapter(cfg.events));
        let term = GhosttyTerm::new(to_term_size(cfg.size), scrollback, sink)
            .expect("ghostty: failed to construct Terminal (invalid size?)");
        Self {
            term,
            size: cfg.size,
            events,
            last_title: String::new(),
        }
    }

    fn name() -> &'static str
    where
        Self: Sized,
    {
        "ghostty"
    }

    fn feed(&mut self, data: &[u8]) {
        // Per `kmux_ghostty::GhosttyTerm::feed`, `Err(Feed)` is only returned
        // when the VT parser encounters an internal error — in practice this
        // means our Zig handler returned an error. Treat it as a soft failure:
        // log and drop so one bad sequence never tears down a pane.
        if let Err(e) = self.term.feed(data) {
            tracing::warn!(error = %e, "ghostty: feed failed");
        }
        // Pull the current title from the VT state after every feed. This
        // ensures the title is captured even when the push callback fires
        // before a client subscribes to the broadcast channel. PaneEventSink
        // checks for change before broadcasting, so double-firing is a no-op.
        if let Some(title) = self.term.title()
            && title != self.last_title
        {
            self.last_title = title.clone();
            self.events.on_title(&title);
        }
    }

    fn size(&self) -> BackendSize {
        self.size
    }

    fn resize(&mut self, size: BackendSize) {
        self.size = size;
        if let Err(e) = self.term.resize(to_term_size(size)) {
            tracing::warn!(error = %e, "ghostty: resize failed");
        }
    }

    fn cursor(&self) -> CursorState {
        self.term.cursor()
    }

    fn modes(&self) -> TermModes {
        self.term.modes()
    }

    fn is_alt_screen(&self) -> bool {
        self.term.is_alt_screen()
    }

    fn fill_cells(&self, out: &mut [CellState]) {
        // Pre-fill; the wrapper blanks unvisited cells itself, but the trait
        // guarantees the pre-fill invariant for callers that read `out` on
        // partial writes.
        for cell in out.iter_mut() {
            *cell = CellState::default();
        }
        if let Err(e) = self.term.fill_cells(out) {
            tracing::warn!(error = %e, "ghostty: fill_cells failed");
        }
    }

    fn fill_cells_and_cursor(&self, out: &mut [CellState]) -> (CursorState, TermModes) {
        for cell in out.iter_mut() {
            *cell = CellState::default();
        }
        match self.term.fill_cells_and_cursor(out) {
            Ok((cursor, modes)) => (cursor, modes),
            Err(e) => {
                tracing::warn!(error = %e, "ghostty: fill_cells_and_cursor failed");
                (self.term.cursor(), self.term.modes())
            }
        }
    }

    fn history_size(&self) -> usize {
        self.term.history_size()
    }

    fn read_history_lines(&self, start: usize, count: usize, cols: usize) -> Vec<Vec<CellState>> {
        match self.term.read_history(start, count, cols) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "ghostty: read_history failed");
                Vec::new()
            }
        }
    }

    fn encode_key_event(&self, event: &ProtoKeyEvent) -> Vec<u8> {
        let opts = self.term.encoder_options();
        let kg_event = proto_key_to_ghostty(event);
        match kmux_ghostty::encode_key(&opts, &kg_event) {
            Ok(bytes) => bytes,
            Err(e) => {
                // KeyEncodeError::InvalidEnum is impossible at runtime — both
                // sides use Rust enums with explicit u16 ordinals checked at
                // compile time by the `key_ordinals_match_zig` tests on each
                // side.  If it does fire, drop the keystroke loudly.
                tracing::warn!(error = %e, "ghostty: encode_key failed; dropping keystroke");
                Vec::new()
            }
        }
    }
}

/// Translate a wire `KeyEvent` (`kmux-protocol`) into the safe-wrapper
/// `KeyEvent` (`kmux-ghostty`).
fn proto_key_to_ghostty(ev: &ProtoKeyEvent) -> kmux_ghostty::KeyEvent {
    use kmux_protocol::messages::KeyAction as ProtoAction;
    let action = match ev.action {
        ProtoAction::Press => kmux_ghostty::KeyAction::Press,
        ProtoAction::Repeat => kmux_ghostty::KeyAction::Repeat,
    };
    kmux_ghostty::KeyEvent {
        key: Some(ev.code.into()),
        mods: kmux_ghostty::KeyMods::from_bits_truncate(ev.mods.bits()),
        action: action.into(),
        utf8: ev.text.clone(),
        unshifted_codepoint: ev.unshifted_codepoint,
    }
}

// Acknowledge that `GhosttyError::Feed` is intentionally unused in the warm
// path; keeping the variant lets callers of `kmux-ghostty` differentiate.
const _: fn() = || {
    let _ = std::mem::size_of::<GhosttyError>();
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Mutex, OnceLock};

    use super::*;
    use crate::backend::{BackendEventSink, CapabilityHandles, NullEventSink};
    use crate::diff_engine::{DiffEngine, DiffResult};
    use kmux_protocol::messages::{CellAttrs, CellColor, DiffOp};

    fn expect_cell_diff(result: DiffResult) -> kmux_protocol::messages::TerminalDiff {
        match result {
            DiffResult::CellDiff { diff, .. } => diff,
            other => panic!("expected CellDiff, got {other:?}"),
        }
    }

    fn expect_cell_diff_with_scrollback(
        result: DiffResult,
    ) -> (
        kmux_protocol::messages::TerminalDiff,
        Vec<Vec<kmux_protocol::messages::CellState>>,
    ) {
        match result {
            DiffResult::CellDiff {
                diff,
                scrollback_lines,
            } => (diff, scrollback_lines),
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

    fn test_backend(rows: u16, cols: u16) -> GhosttyBackend {
        GhosttyBackend::new(test_cfg(rows, cols))
    }

    // -------------------------------------------------------------------
    // Behavioural tests for the libghostty-vt backend; each assertion
    // guards a VT feature kmux relies on end-to-end.
    // -------------------------------------------------------------------

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
            "expected >=5 changed cells, got {total_cells}"
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
        assert_ne!(r_cell.fg, CellColor::new(0xff, 0xff, 0xff));
        assert_ne!(r_cell.fg, CellColor::new(0, 0, 0));
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
        assert_eq!(x_cell.fg, CellColor::new(255, 128, 0));
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
    fn encode_key_event_uses_live_kitty_flags() {
        use kmux_protocol::messages::{KeyAction, KeyCode, KeyEvent, KeyMods};
        let mut ts = DiffEngine::new(test_backend(24, 80));
        // Without kitty kbd negotiation, Ghostty's encoder still emits the
        // xterm modifyOtherKeys legacy form for modified Enter (`\x1b[27;2;13~`).
        // Apps that enable modifyOtherKeys=2 (or kitty kbd) decode this; the
        // bare-bones legacy `\r` fallback is no worse than today.
        let bytes = ts.encode_key_event(&KeyEvent {
            code: KeyCode::Enter,
            mods: KeyMods::SHIFT,
            action: KeyAction::Press,
            text: String::new(),
            unshifted_codepoint: 0,
        });
        assert_eq!(bytes, b"\x1b[27;2;13~");

        // After the inner program enables kitty kbd disambiguate
        // (`\x1b[>1u`), Shift+Enter must encode as CSI 13;2u — what
        // Claude Code expects.
        ts.feed(b"\x1b[>1u");
        let bytes = ts.encode_key_event(&KeyEvent {
            code: KeyCode::Enter,
            mods: KeyMods::SHIFT,
            action: KeyAction::Press,
            text: String::new(),
            unshifted_codepoint: 0,
        });
        assert_eq!(bytes, b"\x1b[13;2u");
    }

    #[test]
    fn encode_key_event_shift_tab_emits_cbt() {
        use kmux_protocol::messages::{KeyAction, KeyCode, KeyEvent, KeyMods};
        let ts = DiffEngine::new(test_backend(24, 80));
        let bytes = ts.encode_key_event(&KeyEvent {
            code: KeyCode::Tab,
            mods: KeyMods::SHIFT,
            action: KeyAction::Press,
            text: String::new(),
            unshifted_codepoint: 0,
        });
        assert_eq!(bytes, b"\x1b[Z");
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
        assert!(total_cells >= 5);
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
        let c1: usize = diff1
            .ops
            .iter()
            .map(|op| match op {
                DiffOp::Cell { .. } => 1,
                DiffOp::Row { cells, .. } => cells.len(),
                DiffOp::Clear => 0,
            })
            .sum();
        assert!(c1 >= 5);

        ts.feed(b"\x1b[3;1H world");
        let diff2 = expect_cell_diff(ts.compute_diff());
        let c2: usize = diff2
            .ops
            .iter()
            .map(|op| match op {
                DiffOp::Cell { .. } => 1,
                DiffOp::Row { cells, .. } => cells.len(),
                DiffOp::Clear => 0,
            })
            .sum();
        assert!(c2 >= 5);
    }

    #[test]
    fn fzf_cursor_hidden_state() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed(b"\x1b[?25l");
        let snap = ts.snapshot();
        assert!(!snap.cursor.visible);
    }

    #[test]
    fn bracketed_paste_mode_enable_disable() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        assert!(!ts.modes().bracketed_paste());
        ts.feed(b"\x1b[?2004h");
        assert!(ts.modes().bracketed_paste());
        ts.feed(b"\x1b[?2004l");
        assert!(!ts.modes().bracketed_paste());
    }

    #[test]
    fn mouse_report_click_mode_enable_disable() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        assert!(!ts.modes().mouse_report());
        ts.feed(b"\x1b[?1000h");
        assert!(ts.modes().mouse_report());
        ts.feed(b"\x1b[?1000l");
        assert!(!ts.modes().mouse_report());
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
        let (_, sb) = expect_cell_diff_with_scrollback(ts.compute_diff());
        assert!(!sb.is_empty());

        ts.feed(b"\x1b[?1049h");
        ts.feed(b"fzf content");
        let (_, sb) = expect_cell_diff_with_scrollback(ts.compute_diff());
        assert!(sb.is_empty(), "no scrollback on alt screen");

        ts.feed(b"\x1b[?1049l");
        let diff = ts.compute_diff();
        if let DiffResult::CellDiff {
            scrollback_lines, ..
        } = diff
        {
            assert!(scrollback_lines.is_empty());
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
        assert!(a_cell.attrs.0 & CellAttrs::DEFAULT_FG != 0);
        assert!(a_cell.attrs.0 & CellAttrs::DEFAULT_BG != 0);
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
        assert!(b_cell.attrs.0 & CellAttrs::BOLD != 0);
    }

    #[test]
    fn wide_char_marks_spacer() {
        let mut ts = DiffEngine::new(test_backend(24, 80));
        ts.feed("中".as_bytes());
        let snap = ts.snapshot();
        assert_eq!(snap.cells[0].c, '中');
        assert!(snap.cells[0].attrs.0 & CellAttrs::WIDE_CHAR != 0);
        assert!(snap.cells[1].attrs.0 & CellAttrs::WIDE_CHAR_SPACER != 0);
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
        let (_, sb) = expect_cell_diff_with_scrollback(ts.compute_diff());
        assert!(!sb.is_empty());
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

        let mut backend = GhosttyBackend::new(cfg);
        backend.feed(b"\x1b]0;My Terminal Title\x07");

        let titles = sink.0.lock().unwrap();
        assert!(
            titles.iter().any(|t| t.contains("My Terminal Title")),
            "expected title event, got: {titles:?}"
        );
    }

    // -------------------------------------------------------------------
    // Per-mode mouse bits: libghostty-vt exposes 1002 / 1003 / 1006 as
    // distinct flags so the wire protocol can report them independently.
    // -------------------------------------------------------------------

    #[test]
    fn mouse_drag_mode_enable_disable() {
        // DEC 1002 — button-event tracking.
        let mut ts = DiffEngine::new(test_backend(24, 80));
        assert_eq!(ts.modes().0 & TermModes::MOUSE_DRAG, 0);
        ts.feed(b"\x1b[?1002h");
        assert_ne!(
            ts.modes().0 & TermModes::MOUSE_DRAG,
            0,
            "MOUSE_DRAG should be set after \\e[?1002h"
        );
        ts.feed(b"\x1b[?1002l");
        assert_eq!(
            ts.modes().0 & TermModes::MOUSE_DRAG,
            0,
            "MOUSE_DRAG should be cleared after \\e[?1002l"
        );
    }

    #[test]
    fn mouse_motion_mode_enable_disable() {
        // DEC 1003 — any-event tracking (motion reports even without button).
        let mut ts = DiffEngine::new(test_backend(24, 80));
        assert_eq!(ts.modes().0 & TermModes::MOUSE_MOTION, 0);
        ts.feed(b"\x1b[?1003h");
        assert_ne!(ts.modes().0 & TermModes::MOUSE_MOTION, 0);
        ts.feed(b"\x1b[?1003l");
        assert_eq!(ts.modes().0 & TermModes::MOUSE_MOTION, 0);
    }

    #[test]
    fn sgr_mouse_mode_enable_disable() {
        // DEC 1006 — SGR extended coordinates.
        let mut ts = DiffEngine::new(test_backend(24, 80));
        assert!(!ts.modes().sgr_mouse());
        ts.feed(b"\x1b[?1006h");
        assert!(ts.modes().sgr_mouse());
        ts.feed(b"\x1b[?1006l");
        assert!(!ts.modes().sgr_mouse());
    }
}
