//! Diff-pipeline conformance: the desync oracle, exercised deterministically.
//!
//! This drives a **real** server-side VT (`TermState = DiffEngine<GhosttyBackend>`)
//! with scripted byte sequences and reconstructs the screen on the client side by
//! applying the emitted diff stream to a `CellGrid` — the two independent grid
//! implementations the live oracle compares. After each frame it asserts
//! `client_grid.digest() == server_grid.digest()` (the same
//! `GridSnapshot::digest` the wire-level `GridDigest` will carry).
//!
//! A bug anywhere on seams 2–5 (server grid → `TerminalDiff` → wire → client
//! `CellGrid`) diverges the digests here, deterministically, with no daemon,
//! sockets, or timing. It is the foundation every later optimization is gated on:
//! if these stay green through a refactor, the diff pipeline still round-trips.
//!
//! Scrollback note: `TermState::snapshot()` caps its tail at `SNAPSHOT_TAIL_LINES`
//! (500). These scripts deliberately keep total scrollback under that cap so the
//! server's tail and the client's full scrollback are directly comparable; the
//! live oracle normalizes the tail window instead (handled when `GridDigest` is
//! wired).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use kmux_client::grid::CellGrid;
use kmux_vt_core::backend::{BackendConfig, BackendSize, CapabilityHandles, NullEventSink};
use kmux_vt_core::diff_engine::DiffResult;
use kmux_vt_core::term_state::{TermState, new_term_state};

const ROWS: u16 = 10;
const COLS: u16 = 40;

fn make_term_state() -> TermState {
    new_term_state(BackendConfig {
        size: BackendSize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        },
        capabilities: CapabilityHandles {
            kitty_graphics: Arc::new(AtomicBool::new(false)),
            kitty_keyboard: Arc::new(AtomicBool::new(false)),
        },
        events: Arc::new(NullEventSink),
        scrollback: 10_000,
    })
}

/// Apply one computed `DiffResult` to the client grid exactly as a client would
/// observe it on the wire — mirroring `dispatch_diff_result` in `relay.rs`.
///
/// Ordering matters on a scrollback-reset frame: the relay emits the viewport
/// `TerminalUpdate` (carrying `scrollback_reset`) *before* the `ScrollbackAppend`
/// so the client wipes history before the surviving lines land; otherwise the
/// append precedes the diff. The final digest is order-insensitive (the transient
/// `pending_history_total` is not part of a snapshot), but we replicate the real
/// order anyway.
fn apply_result(grid: &mut CellGrid, result: DiffResult) {
    match result {
        DiffResult::CellDiff {
            diff,
            scrollback_lines,
        } => {
            let first_index = diff
                .history_total
                .saturating_sub(scrollback_lines.len() as u64);
            let reset_first = diff.scrollback_reset.is_some();
            let has_sb = !scrollback_lines.is_empty();
            if reset_first {
                grid.apply_diff(diff);
                if has_sb {
                    grid.apply_scrollback_append(first_index, scrollback_lines);
                }
            } else {
                if has_sb {
                    grid.apply_scrollback_append(first_index, scrollback_lines);
                }
                grid.apply_diff(diff);
            }
        }
        DiffResult::CursorOnly { cursor, modes, .. } => {
            grid.apply_cursor_update(cursor, modes);
        }
        DiffResult::None => {}
    }
}

/// Feed each chunk through the server VT, apply the emitted diff to a client
/// grid, and assert the two grids hash identically after every frame.
fn check_conformance(chunks: &[&[u8]]) {
    let mut ts = make_term_state();
    let mut grid = CellGrid::new(ROWS as usize, COLS as usize);

    // Fresh attach: the client seeds from the server's authoritative snapshot.
    grid.apply_snapshot(ts.snapshot());
    assert_eq!(
        grid.to_snapshot().digest(),
        ts.snapshot().digest(),
        "initial attach snapshot must match"
    );

    for (i, chunk) in chunks.iter().enumerate() {
        ts.feed(chunk);
        let result = ts.compute_diff();
        apply_result(&mut grid, result);
        assert_eq!(
            grid.to_snapshot().digest(),
            ts.snapshot().digest(),
            "desync after frame {i} (input {:?})",
            String::from_utf8_lossy(chunk)
        );
    }
}

#[test]
fn conformance_plain_text_and_newlines() {
    check_conformance(&[
        b"Hello, world!",
        b"\r\n",
        b"second line",
        b"\r\nthird\r\nfourth\r\n",
    ]);
}

#[test]
fn conformance_sgr_colors_and_attrs() {
    check_conformance(&[
        b"\x1b[31mred\x1b[0m ",
        b"\x1b[1;32mbold-green\x1b[0m ",
        b"\x1b[4munderline\x1b[24m ",
        b"\x1b[7minverse\x1b[0m",
    ]);
}

#[test]
fn conformance_clear_and_cursor_positioning() {
    check_conformance(&[
        b"scatter some text across the grid",
        b"\x1b[5;10HX",   // absolute cursor move + write
        b"\x1b[2J\x1b[H", // clear screen, home cursor
        b"after clear",
        b"\x1b[3;3Hmid", // another positioned write
    ]);
}

#[test]
fn conformance_scrollback_accumulation() {
    // The 10-row viewport overflows, pushing lines into scrollback. Kept well
    // under SNAPSHOT_TAIL_LINES so the server tail == client full scrollback.
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    for n in 0..40 {
        chunks.push(format!("row number {n}\r\n").into_bytes());
    }
    let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
    check_conformance(&refs);
}

#[test]
fn conformance_wide_chars_and_unicode() {
    check_conformance(&[
        "café ".as_bytes(),
        "日本語テスト".as_bytes(),
        "emoji: 🚀🔥".as_bytes(),
        b"\r\nascii tail",
    ]);
}

#[test]
fn conformance_full_reset_wipes_consistently() {
    check_conformance(&[
        b"content before reset\r\nmore lines\r\nand more\r\n",
        b"\x1bc", // RIS — full reset, wipes screen + scrollback
        b"fresh start after RIS",
    ]);
}

/// Deterministic, seeded pseudo-random stream. A fixed seed makes any failure
/// reproducible; the generator mixes printable runs, newlines, SGR colour
/// changes, absolute cursor moves, and occasional clears — the operations most
/// likely to surface a baseline/representation inconsistency like the `'\0'`
/// vs `' '` desync. Newlines are bounded so total scrollback stays under
/// `SNAPSHOT_TAIL_LINES`.
#[test]
fn conformance_seeded_random_stream() {
    // SplitMix64 — same family the impairment shim uses; no external rng dep.
    struct Rng(u64);
    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }
    }

    // A handful of seeds so the single test covers several distinct streams.
    for seed in [0x1234_5678u64, 0xdead_beef, 0x0f0f_0f0f, 1, 99] {
        let mut rng = Rng(seed);
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        let mut newlines = 0u32;
        for _ in 0..200 {
            let mut chunk: Vec<u8> = Vec::new();
            match rng.below(10) {
                0..=4 => {
                    // Printable run.
                    let len = 1 + rng.below(12);
                    for _ in 0..len {
                        let ch = b' ' + rng.below(95) as u8; // 0x20..0x7e
                        chunk.push(ch);
                    }
                }
                5 => {
                    if newlines < 300 {
                        chunk.extend_from_slice(b"\r\n");
                        newlines += 1;
                    }
                }
                6 => {
                    // SGR colour / attribute change.
                    let code = rng.below(8) + 30;
                    chunk.extend_from_slice(format!("\x1b[{code}m").as_bytes());
                }
                7 => chunk.extend_from_slice(b"\x1b[0m"),
                8 => {
                    // Absolute cursor positioning within the grid.
                    let row = 1 + rng.below(ROWS as u64);
                    let col = 1 + rng.below(COLS as u64);
                    chunk.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
                }
                _ => chunk.extend_from_slice(b"\x1b[2J\x1b[H"), // clear + home
            }
            if !chunk.is_empty() {
                chunks.push(chunk);
            }
        }
        let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        // Surface which seed diverged if the oracle ever trips.
        std::panic::catch_unwind(|| check_conformance(&refs))
            .unwrap_or_else(|_| panic!("conformance diverged for seed {seed:#x}"));
    }
}
