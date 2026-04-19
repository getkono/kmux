use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{CellAttrs, CellState, GridSnapshot, SequenceNo};

use crate::diff_engine::DiffResult;
use crate::scrollback::DiffBuffer;
use crate::term_state::TermState;

/// Convert a [`GridSnapshot`] plus scrollback history to ANSI/VT100 escape
/// sequences that reproduce the full terminal history when fed to a fresh
/// terminal emulator.
///
/// Emits the scrollback lines first (oldest → newest), then the visible
/// viewport rows, then a dim "session restored" separator.  Because a real
/// terminal emulator scrolls lines into its history buffer as content flows
/// past the top of the screen, the user can scroll up to see the entire
/// restored history.
pub(super) fn snapshot_to_ansi(
    snapshot: &GridSnapshot,
    scrollback_lines: &[Vec<CellState>],
) -> Vec<u8> {
    let mut out = Vec::new();

    // Reset all SGR attributes before rendering.
    out.extend_from_slice(b"\x1b[0m");

    // Emit scrollback history first so it scrolls into the backend's history
    // buffer.  Each line ends with \r\n which advances the terminal row.
    for line in scrollback_lines {
        emit_cells_line(&mut out, line);
    }

    // Emit the visible viewport rows.
    let rows = snapshot.rows as usize;
    let cols = snapshot.cols as usize;
    for row in 0..rows {
        let base = row * cols;
        if base + cols > snapshot.cells.len() {
            break;
        }
        emit_cells_line(&mut out, &snapshot.cells[base..base + cols]);
    }

    // Dim separator visually distinguishing the restored history from the new shell.
    out.extend_from_slice(b"\x1b[2m[kmux: session restored]\x1b[0m\r\n");

    out
}

/// Emit one row of cells as ANSI bytes into `out`, followed by `\r\n`.
///
/// Trailing spaces are trimmed.  SGR sequences are coalesced so that only one
/// escape is emitted per style-change boundary.
pub(super) fn emit_cells_line(out: &mut Vec<u8>, cells: &[CellState]) {
    use std::fmt::Write as FmtWrite;

    let last_content = cells
        .iter()
        .rposition(|cell| cell.c != ' ')
        .map(|i| i + 1)
        .unwrap_or(0);

    #[derive(PartialEq)]
    struct StyleKey {
        fg: (u8, u8, u8),
        bg: (u8, u8, u8),
        attrs: u16,
    }

    if last_content > 0 {
        let mut prev_key: Option<StyleKey> = None;

        for cell in &cells[..last_content] {
            let style_key = StyleKey {
                fg: (cell.fg.r, cell.fg.g, cell.fg.b),
                bg: (cell.bg.r, cell.bg.g, cell.bg.b),
                attrs: cell.attrs.0,
            };
            if prev_key.as_ref() != Some(&style_key) {
                prev_key = Some(style_key);
                let mut sgr = String::from("\x1b[0");
                if cell.attrs.contains(CellAttrs::BOLD) {
                    sgr.push_str(";1");
                }
                if cell.attrs.contains(CellAttrs::DIM) {
                    sgr.push_str(";2");
                }
                if cell.attrs.contains(CellAttrs::ITALIC) {
                    sgr.push_str(";3");
                }
                if cell.attrs.contains(CellAttrs::UNDERLINE) {
                    sgr.push_str(";4");
                }
                if !cell.attrs.contains(CellAttrs::DEFAULT_FG) {
                    let _ = write!(sgr, ";38;2;{};{};{}", cell.fg.r, cell.fg.g, cell.fg.b);
                }
                if !cell.attrs.contains(CellAttrs::DEFAULT_BG) {
                    let _ = write!(sgr, ";48;2;{};{};{}", cell.bg.r, cell.bg.g, cell.bg.b);
                }
                sgr.push('m');
                out.extend_from_slice(sgr.as_bytes());
            }

            let mut buf = [0u8; 4];
            let s = cell.c.encode_utf8(&mut buf);
            out.extend_from_slice(s.as_bytes());
        }
    }

    out.extend_from_slice(b"\x1b[0m\r\n");
}

/// Feed preamble bytes into `term_state`, compute the resulting diff, and push
/// it into `scrollback` so clients that attach immediately after restore receive
/// the old visual content.
///
/// Calling `compute_diff()` here also synchronises `prev_cells` so the first
/// real PTY read does not re-emit the preamble content as a spurious diff.
pub(super) fn seed_pane_with_preamble(
    term_state: &Arc<Mutex<TermState>>,
    scrollback: &Arc<Mutex<DiffBuffer>>,
    seqno_counter: &Arc<AtomicU64>,
    preamble: &[u8],
) {
    if preamble.is_empty() {
        return;
    }

    let diff_opt = {
        let mut ts = term_state.lock().unwrap();
        ts.feed(preamble);
        match ts.compute_diff() {
            DiffResult::CellDiff { diff: d, .. } => Some(d),
            _ => None,
        }
    };

    if let Some(diff) = diff_opt {
        let seqno = SequenceNo(seqno_counter.fetch_add(1, Ordering::Relaxed));
        scrollback.lock().unwrap().push(seqno, Arc::new(diff));
    }
}
