use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{CellAttrs, CellColor, CellState, GridSnapshot, SequenceNo};

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

/// Emit one row of cells as ANSI bytes into `out`, ending with the SGR reset,
/// so that a fresh emulator fed them holds the same cells: characters,
/// colours and every attribute.
///
/// Only trailing blanks of the default style are trimmed — a fresh emulator
/// holds those already — while a coloured or underlined blank is content. A
/// wide character's spacer cell is not written: the emulator makes it when
/// it writes the character. SGR sequences are coalesced so that only one
/// escape is emitted per style-change boundary.
fn emit_cells(out: &mut Vec<u8>, cells: &[CellState]) {
    let end = cells
        .iter()
        .rposition(|cell| !is_default_blank(cell))
        .map_or(0, |i| i + 1);

    let mut prev_style: Option<(CellColor, CellColor, CellAttrs)> = None;
    for cell in &cells[..end] {
        if cell.attrs.contains(CellAttrs::WIDE_CHAR_SPACER) {
            continue;
        }
        let style = (cell.fg, cell.bg, cell.attrs);
        if prev_style != Some(style) {
            prev_style = Some(style);
            out.extend_from_slice(sgr_for(cell).as_bytes());
        }
        let mut buf = [0u8; 4];
        out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
    }

    out.extend_from_slice(b"\x1b[0m");
}

/// Each attribute and the SGR parameter that sets it.
const SGR_ATTRS: [(u16, &str); 8] = [
    (CellAttrs::BOLD, "1"),
    (CellAttrs::DIM, "2"),
    (CellAttrs::ITALIC, "3"),
    (CellAttrs::UNDERLINE, "4"),
    (CellAttrs::BLINK, "5"),
    (CellAttrs::INVERSE, "7"),
    (CellAttrs::HIDDEN, "8"),
    (CellAttrs::STRIKETHROUGH, "9"),
];

/// Whether `cell` is a blank of the default style: what a fresh emulator
/// holds in a cell nothing was written to.
fn is_default_blank(cell: &CellState) -> bool {
    cell.c == ' ' && cell.attrs == CellState::default().attrs
}

/// The SGR sequence that sets `cell`'s style from scratch.
///
/// An inverse cell's colours are stored as displayed — foreground and
/// background swapped, `DEFAULT_FG`/`DEFAULT_BG` with them — so they are
/// swapped back to the colours the program set, which SGR 7 swaps again.
fn sgr_for(cell: &CellState) -> String {
    use std::fmt::Write as _;

    let attrs = cell.attrs;
    let mut sgr = String::from("\x1b[0");
    for (flag, param) in SGR_ATTRS {
        if attrs.contains(flag) {
            sgr.push(';');
            sgr.push_str(param);
        }
    }
    let displayed = [
        (cell.fg, attrs.contains(CellAttrs::DEFAULT_FG)),
        (cell.bg, attrs.contains(CellAttrs::DEFAULT_BG)),
    ];
    let [fg, bg] = if attrs.contains(CellAttrs::INVERSE) {
        [displayed[1], displayed[0]]
    } else {
        displayed
    };
    for (param, (color, is_default)) in [("38", fg), ("48", bg)] {
        if !is_default {
            let _ = write!(sgr, ";{param};2;{};{};{}", color.r, color.g, color.b);
        }
    }
    sgr.push('m');
    sgr
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

    /// A pane inherited across a handoff shows exactly the screen it had: its
    /// history above, the same rows in the same places, and the cursor where
    /// it was. A line break after the last row once scrolled the whole screen
    /// up a line, taking the top row into the history — a fresh shell's
    /// prompt, which is often all there is.
    ///
    /// Cell for cell, with every attribute (issue #234): each SGR attribute,
    /// alone and together, an inverse cell with colours of its own, a wide
    /// character, and blanks that are coloured or underlined — at the end of
    /// a row, where plain blanks are trimmed — on the screen and in the
    /// history alike.
    #[test]
    fn an_inherited_screen_is_seeded_exactly() {
        let before = fixture_term_state(4, 30);
        lock_term_state(&before).feed(
            concat!(
                // Scrolls into the history: a styled line, ending in styled blanks.
                "\x1b[1;4;38;2;9;8;7mold\x1b[0m\x1b[41m  \x1b[0m\r\n",
                "\x1b[1;31mtop\x1b[0m \x1b[2mdim\x1b[0m \x1b[3mit\x1b[0m \x1b[4mund\x1b[0m\r\n",
                "\x1b[5mbl\x1b[0m \x1b[7minv\x1b[0m \x1b[8mhid\x1b[0m \x1b[9mstr\x1b[0m ",
                "\x1b[7;38;2;1;2;3;48;2;4;5;6mboth\x1b[0m\r\n",
                "wide \u{4e16}\u{754c} \x1b[1;2;3;4;5;7;8;9mall\x1b[0m\r\n",
                "\x1b[44mbg\x1b[4m  \x1b[0m\x1b[7m \x1b[0m",
                "\x1b[2;3H",
            )
            .as_bytes(),
        );
        let snapshot = lock_term_state(&before).snapshot();
        let history = read_history(&before);
        assert_eq!(history.len(), 1, "one line scrolled off");

        let after = fixture_term_state(4, 30);
        let preamble = snapshot_to_ansi(&snapshot, &history, false);
        let diffs = Arc::new(Mutex::new(DiffBuffer::new(64 * 1024)));
        seed_pane_with_preamble(&after, &diffs, &Arc::new(AtomicU64::new(1)), &preamble);
        let seeded = lock_term_state(&after).snapshot();

        assert_eq!(
            rows_of(&seeded),
            [
                "top dim it und",
                "bl inv hid str both",
                "wide \u{4e16} \u{754c}  all",
                "bg",
            ]
        );
        for (i, (got, want)) in seeded.cells.iter().zip(&snapshot.cells).enumerate() {
            let (row, col) = (i / 30, i % 30);
            assert_eq!(got, want, "screen cell at row {row}, column {col}");
        }
        assert_eq!(read_history(&after), history, "the history, cell for cell");
        assert_eq!((seeded.cursor.row, seeded.cursor.col), (1, 2));
        assert_eq!(seeded.history_total, 1, "the one history line, no more");
    }

    /// Every line in `term`'s history, oldest first.
    fn read_history(term: &Arc<Mutex<TermState>>) -> Vec<Vec<CellState>> {
        let term = lock_term_state(term);
        term.read_history_lines(0, term.history_size())
            .iter()
            .map(|line| line.to_vec())
            .collect()
    }
}
