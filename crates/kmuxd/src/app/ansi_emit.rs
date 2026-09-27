use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{CellAttrs, CellState, GridSnapshot, SequenceNo};

use crate::diff_engine::DiffResult;
use crate::lock::lock_term_state;
use crate::scrollback::DiffBuffer;
use crate::term_state::TermState;

/// Convert a [`GridSnapshot`] plus scrollback history to ANSI/VT100 escape
/// sequences that reproduce the full terminal history when fed to a fresh
/// terminal emulator.
///
/// Emits the scrollback lines first (oldest → newest), then the visible
/// viewport rows, and — when `separator` is true — a dim "session restored"
/// marker.  Because a real terminal emulator scrolls lines into its history
/// buffer as content flows past the top of the screen, the user can scroll up to
/// see the entire restored history.
///
/// `separator` is `true` when a fresh shell was respawned (the marker
/// distinguishes the old history from the new prompt) and `false` for a live PTY
/// inherited across a handoff (seamless — there is no respawn boundary, so the
/// cursor goes back where the live program left it).
pub(super) fn snapshot_to_ansi(
    snapshot: &GridSnapshot,
    scrollback_lines: &[Vec<CellState>],
    separator: bool,
) -> Vec<u8> {
    let mut out = Vec::new();

    // Reset all SGR attributes before rendering.
    out.extend_from_slice(b"\x1b[0m");

    // Emit scrollback history first so it scrolls into the backend's history
    // buffer.  Each line ends with \r\n which advances the terminal row.
    for line in scrollback_lines {
        emit_cells_line(&mut out, line);
    }

    // Emit the visible viewport rows, with a line break only *between* them:
    // one after the last row would scroll the whole screen up a line and the
    // top row into the history.
    let rows = snapshot.rows as usize;
    let cols = snapshot.cols as usize;
    for row in 0..rows {
        let base = row * cols;
        if base + cols > snapshot.cells.len() {
            break;
        }
        if row > 0 {
            out.extend_from_slice(b"\r\n");
        }
        emit_cells(&mut out, &snapshot.cells[base..base + cols]);
    }

    // Dim separator visually distinguishing the restored history from the new
    // shell. Omitted for inherited live PTYs, where the handoff is seamless.
    if separator {
        out.extend_from_slice(b"\r\n\x1b[2m[kmux: session restored]\x1b[0m\r\n");
    } else {
        let cursor = &snapshot.cursor;
        let at = format!("\x1b[{};{}H", cursor.row + 1, cursor.col + 1);
        out.extend_from_slice(at.as_bytes());
    }

    out
}

/// [`emit_cells`], followed by `\r\n`.
fn emit_cells_line(out: &mut Vec<u8>, cells: &[CellState]) {
    emit_cells(out, cells);
    out.extend_from_slice(b"\r\n");
}

/// Emit one row of cells as ANSI bytes into `out`, ending with the SGR reset.
///
/// Trailing spaces are trimmed.  SGR sequences are coalesced so that only one
/// escape is emitted per style-change boundary.
fn emit_cells(out: &mut Vec<u8>, cells: &[CellState]) {
    use std::fmt::Write as FmtWrite;

    let last_content = cells
        .iter()
        .rposition(|cell| cell.c != ' ')
        .map_or(0, |i| i + 1);

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

    out.extend_from_slice(b"\x1b[0m");
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
        let mut ts = lock_term_state(term_state);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::fixture_term_state;

    /// A restored pane's preamble is fed to its emulator and recorded as the
    /// first replayable diff, so a client attaching right away sees it.
    #[test]
    fn seeding_a_pane_records_the_preamble_as_its_first_diff() {
        let ts = fixture_term_state(4, 20);
        let scrollback = Arc::new(Mutex::new(DiffBuffer::new(64 * 1024)));
        let seqno = Arc::new(AtomicU64::new(1));

        seed_pane_with_preamble(&ts, &scrollback, &seqno, b"hi");

        assert_eq!(seqno.load(Ordering::Relaxed), 2);
        assert_eq!(
            scrollback.lock().unwrap().oldest_seqno(),
            Some(SequenceNo(1))
        );
    }

    /// The text of `rows` × `cols` cells, one string per row.
    fn rows_of(snapshot: &GridSnapshot) -> Vec<String> {
        snapshot
            .cells
            .chunks(usize::from(snapshot.cols))
            .map(|row| {
                row.iter()
                    .map(|c| c.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// A pane inherited across a handoff shows exactly the screen it had: the
    /// same rows in the same places and the cursor where it was. A line break
    /// after the last row once scrolled the whole screen up a line, taking
    /// the top row off it — a fresh shell's prompt, which is often all there is.
    #[test]
    fn an_inherited_screen_is_seeded_exactly() {
        let before = fixture_term_state(4, 20);
        lock_term_state(&before).feed(b"top\r\n\r\nthird\r\nlast\x1b[2;3H");
        let snapshot = lock_term_state(&before).snapshot();

        let after = fixture_term_state(4, 20);
        lock_term_state(&after).feed(&snapshot_to_ansi(&snapshot, &[], false));
        let seeded = lock_term_state(&after).snapshot();

        assert_eq!(rows_of(&seeded), ["top", "", "third", "last"]);
        assert_eq!((seeded.cursor.row, seeded.cursor.col), (1, 2));
    }
}
