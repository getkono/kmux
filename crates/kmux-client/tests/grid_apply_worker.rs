//! Off-UI-thread grid apply worker (issue #182, §1): correctness + tear-freedom.
//!
//! These exercise the real worker thread + `ArcSwap` publish handoff that a
//! `Published` `CellGrid` uses, against a synchronous `Local` reference.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kmux_client::grid::{ApplyHandle, CellGrid};
use kmux_protocol::messages::{CellState, CursorState, DiffOp, TermModes, TerminalDiff};
use proptest::prelude::*;

const ROWS: usize = 8;
const COLS: usize = 8;

fn cell_diff(row: u16, col: u16, ch: char) -> TerminalDiff {
    TerminalDiff {
        ops: vec![DiffOp::Cell {
            row,
            col,
            cell: CellState {
                c: ch,
                ..CellState::default()
            },
        }],
        cursor: CursorState {
            row,
            col,
            ..CursorState::default()
        },
        modes: TermModes::EMPTY,
        history_total: 0,
        scrollback_reset: None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Applying the same diff stream to a worker-backed `Published` grid and a
    /// synchronous `Local` grid yields byte-identical content — the worker
    /// reconstructs the grid exactly, and the published snapshot the UI reads
    /// matches the authoritative state once drained.
    #[test]
    fn published_grid_matches_local(
        ops in prop::collection::vec((0u16..ROWS as u16, 0u16..COLS as u16, 0u32..4), 0..60)
    ) {
        let handle = ApplyHandle::spawn();
        let published = handle.register_pane("p".into(), ROWS, COLS);
        let mut pub_grid = CellGrid::published("p".into(), handle.sender(), published);
        let mut local = CellGrid::new(ROWS, COLS);

        let glyph = |n: u32| char::from(b'a' + n as u8);
        for (row, col, ch) in ops {
            let diff = cell_diff(row, col, glyph(ch));
            pub_grid.apply_diff(diff.clone());
            local.apply_diff(diff);
        }

        // Drain the worker, then load what it published.
        handle.barrier();
        pub_grid.refresh();

        prop_assert_eq!(pub_grid.cells(), local.cells());
        prop_assert_eq!(pub_grid.cursor(), local.cursor());
        prop_assert_eq!(
            pub_grid.to_snapshot().digest(),
            local.to_snapshot().digest(),
            "worker-reconstructed grid must share the reference digest"
        );
    }
}

/// A reader loading the published snapshot concurrently with the worker applying
/// a burst never observes a torn `(dimensions, cells, generation)` tuple: every
/// load is a complete prior value with `cells.len() == rows * cols` and a
/// monotonically non-decreasing generation.
#[test]
fn concurrent_reader_never_tears() {
    let handle = ApplyHandle::spawn();
    let published = handle.register_pane("p".into(), ROWS, COLS);
    let reader_slot = Arc::clone(&published);
    let final_slot = Arc::clone(&published);
    let mut grid = CellGrid::published("p".into(), handle.sender(), published);

    let stop = Arc::new(AtomicBool::new(false));
    let observed_generation = Arc::new(AtomicU64::new(0));
    let reader_stop = Arc::clone(&stop);
    let reader_generation = Arc::clone(&observed_generation);
    let reader = std::thread::spawn(move || {
        let mut last_gen = 0u64;
        let mut reads = 0u64;
        while !reader_stop.load(Ordering::Relaxed) {
            let snap = reader_slot.load_full();
            // No torn dimensions/cells: the cell buffer always matches the dims.
            assert_eq!(
                snap.cells().len(),
                snap.rows * snap.cols,
                "torn read: cells len does not match dimensions"
            );
            let generation = snap.cells_generation();
            assert!(
                generation >= last_gen,
                "published generation went backwards"
            );
            last_gen = generation;
            reader_generation.store(generation, Ordering::Relaxed);
            reads += 1;
        }
        reads
    });

    // Apply a burst of mixed mutations: cells, a resize, scrollback, a clear.
    for i in 0..500u16 {
        let r = i % ROWS as u16;
        let c = (i / ROWS as u16) % COLS as u16;
        grid.apply_diff(cell_diff(r, c, char::from(b'a' + (i % 26) as u8)));
        if i == 200 {
            grid.resize(10, 12);
        }
        if i % 50 == 0 {
            grid.apply_scrollback_append(
                grid.scrollback().history_total(),
                vec![vec![CellState::default(); COLS].into()],
            );
        }
    }
    handle.barrier();

    // Do not let a fast writer finish and stop the reader before the scheduler
    // gives it a turn. Require the reader to observe the final publication so
    // this remains a real concurrent handoff test on single-core CI runners.
    let final_generation = final_slot.load().cells_generation();
    let deadline = Instant::now() + Duration::from_secs(1);
    while observed_generation.load(Ordering::Relaxed) < final_generation
        && Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    let reads = reader.join().expect("reader thread");
    assert_eq!(
        observed_generation.load(Ordering::Relaxed),
        final_generation,
        "reader observed the final published generation"
    );
    assert!(reads > 0, "reader observed at least one published snapshot");

    // Final published state reflects the whole burst.
    grid.refresh();
    assert_eq!(grid.rows, 10);
    assert_eq!(grid.cols, 12);
}

/// A `Local` grid (no worker) applies synchronously — the daemon mirror / test
/// path. Reads reflect every apply immediately, with no refresh needed.
#[test]
fn local_grid_applies_synchronously() {
    let mut grid = CellGrid::new(4, 4);
    grid.apply_diff(cell_diff(1, 1, 'Z'));
    // Row 1, col 1 in a 4-wide grid is row-major index 5.
    assert_eq!(grid.cells()[5].c, 'Z');
}
