use kmux_protocol::messages::{CellState, DiffOp, TerminalDiff};

use crate::backend::TerminalBackend;

use super::{DiffEngine, DiffResult};

impl<B: TerminalBackend> DiffEngine<B> {
    /// Compute a diff between the current grid and `prev_cells`, then update
    /// `prev_cells` to match the current grid.
    ///
    /// Returns a [`DiffResult`] that distinguishes between cell changes,
    /// cursor-only changes, and no changes at all.
    pub fn compute_diff(&mut self) -> DiffResult {
        let rows = self.rows as usize;
        let cols = self.cols as usize;

        // Reset scratch buffer and populate from backend (single renderable_content() call)
        self.current_cells.fill(CellState::default());
        let (cursor_state, modes) = self.backend.fill_cells_and_cursor(&mut self.current_cells);

        // Compare all rows, tracking clear-detection metadata inline to avoid
        // a second O(rows*cols) scan.
        let total = rows * cols;
        let mut ops = Vec::new();
        let mut total_changed: usize = 0;
        let mut all_current_default = true;
        for r in 0..rows {
            let base = r * cols;
            let mut c = 0;
            while c < cols {
                if self.current_cells[base + c] != self.prev_cells[base + c] {
                    let start = c;
                    if all_current_default && self.current_cells[base + c] != CellState::default() {
                        all_current_default = false;
                    }
                    c += 1;
                    while c < cols && self.current_cells[base + c] != self.prev_cells[base + c] {
                        if all_current_default
                            && self.current_cells[base + c] != CellState::default()
                        {
                            all_current_default = false;
                        }
                        c += 1;
                    }
                    let run_len = c - start;
                    total_changed += run_len;
                    if run_len >= 2 {
                        ops.push(DiffOp::Row {
                            row: r as u16,
                            start_col: start as u16,
                            cells: self.current_cells[base + start..base + c].to_vec(),
                        });
                    } else {
                        ops.push(DiffOp::Cell {
                            row: r as u16,
                            col: start as u16,
                            cell: self.current_cells[base + start],
                        });
                    }
                } else {
                    if all_current_default && self.current_cells[base + c] != CellState::default() {
                        all_current_default = false;
                    }
                    c += 1;
                }
            }
        }

        // Detect full-screen clear: if all current cells are default and more
        // than half the screen changed, collapse into a single DiffOp::Clear.
        if !ops.is_empty() && all_current_default && total_changed > total / 2 {
            ops = vec![DiffOp::Clear];
        }

        // Swap buffers: current becomes prev for next frame
        std::mem::swap(&mut self.prev_cells, &mut self.current_cells);

        // Extract scrollback lines that were pushed to history since last diff.
        // Alt-screen transitions need special handling: entering alt screen drops
        // history_size to 0, and exiting restores it to the previous value. Without
        // tracking this, the exit would re-send the entire scrollback as "new."
        let is_alt = self.backend.is_alt_screen();
        let current_history_size = self.backend.history_size();

        let scrollback_lines = if is_alt {
            // On alt screen: save main history size (first time only), emit nothing.
            if self.saved_main_history_size.is_none() {
                self.saved_main_history_size = Some(self.prev_history_size);
            }
            self.prev_history_size = current_history_size;
            vec![]
        } else if let Some(saved) = self.saved_main_history_size.take() {
            // Just exited alt screen: only emit lines added since before alt entry.
            let lines = if current_history_size > saved {
                self.backend
                    .read_history_lines(saved, current_history_size - saved, cols)
            } else {
                vec![]
            };
            self.prev_history_size = current_history_size;
            lines
        } else if current_history_size > self.prev_history_size {
            // Normal operation: emit new scrollback lines.
            let new_count = current_history_size - self.prev_history_size;
            let start = self.prev_history_size;
            self.prev_history_size = current_history_size;
            self.backend.read_history_lines(start, new_count, cols)
        } else {
            self.prev_history_size = current_history_size;
            vec![]
        };

        // Mirror every scrollback line before anything else touches it. This
        // makes the mirror authoritative: later we read `history_total` from
        // it and the relay emits the lines out-of-band as `ScrollbackAppend`.
        if !scrollback_lines.is_empty() {
            self.mirror.append(scrollback_lines.clone());
        }
        let history_total = self.mirror.history_total();

        let cells_changed = !ops.is_empty();
        let has_scrollback = !scrollback_lines.is_empty();
        let cursor_or_modes_changed = cursor_state != self.prev_cursor || modes != self.prev_modes;

        self.prev_cursor = cursor_state;
        self.prev_modes = modes;

        if cells_changed || has_scrollback {
            DiffResult::CellDiff {
                diff: TerminalDiff {
                    ops,
                    cursor: cursor_state,
                    modes,
                    history_total,
                },
                scrollback_lines,
            }
        } else if cursor_or_modes_changed {
            DiffResult::CursorOnly {
                cursor: cursor_state,
                modes,
                history_total,
            }
        } else {
            DiffResult::None
        }
    }
}

#[cfg(test)]
mod tests {
    use kmux_protocol::messages::{CellColor, CellState, DiffOp};

    use crate::backend::mock::MockBackend;
    use crate::diff_engine::{DiffEngine, DiffResult};
    use kmux_protocol::messages::{GridSnapshot, TermModes};

    fn mock_engine(rows: u16, cols: u16) -> DiffEngine<MockBackend> {
        DiffEngine::new(MockBackend::new(rows, cols))
    }

    #[test]
    fn no_op_feed_produces_none() {
        let mut engine = mock_engine(24, 80);
        let _ = engine.compute_diff(); // consume initial (all blank)
        engine.feed(b"");
        assert!(matches!(engine.compute_diff(), DiffResult::None));
    }

    #[test]
    fn cell_change_produces_diff() {
        let mut engine = mock_engine(24, 80);
        let _ = engine.compute_diff(); // consume initial

        // Mutate a cell in the mock backend
        engine.backend.cells[0] = CellState {
            c: 'X',
            fg: CellColor::new(0xff, 0x00, 0x00),
            bg: CellColor::new(0x28, 0x2c, 0x34),
            ..CellState::default()
        };
        let DiffResult::CellDiff { diff, .. } = engine.compute_diff() else {
            panic!("expected CellDiff");
        };
        assert!(!diff.ops.is_empty());
    }

    #[test]
    fn cursor_move_without_cell_change_produces_cursor_only() {
        let mut engine = mock_engine(24, 80);
        let _ = engine.compute_diff(); // consume initial

        engine.backend.cursor_state.col = 5;
        let DiffResult::CursorOnly { cursor, .. } = engine.compute_diff() else {
            panic!("expected CursorOnly");
        };
        assert_eq!(cursor.col, 5);
    }

    #[test]
    fn blink_change_without_cell_change_produces_cursor_only() {
        let mut engine = mock_engine(24, 80);
        let _ = engine.compute_diff(); // consume initial (blink defaults to false)

        // A bare DECSCUSR steady→blinking toggle changes only the blink flag;
        // it must still reach the client as a cursor-only diff.
        engine.backend.cursor_state.blink = true;
        let DiffResult::CursorOnly { cursor, .. } = engine.compute_diff() else {
            panic!("expected CursorOnly");
        };
        assert!(cursor.blink, "blink flag propagates in a cursor-only diff");
    }

    #[test]
    fn mode_change_without_cell_change_produces_cursor_only() {
        let mut engine = mock_engine(24, 80);
        let _ = engine.compute_diff(); // consume initial

        engine.backend.mode_flags = TermModes(TermModes::APP_CURSOR);
        let DiffResult::CursorOnly { modes, .. } = engine.compute_diff() else {
            panic!("expected CursorOnly");
        };
        assert!(modes.app_cursor());
    }

    #[test]
    fn clear_detection_when_all_default() {
        let mut engine = mock_engine(4, 4);
        // Fill all cells with non-default content
        for cell in &mut engine.backend.cells {
            *cell = CellState {
                c: 'X',
                ..CellState::default()
            };
        }
        let _ = engine.compute_diff(); // consume the fill

        // Now reset all to default (simulating CSI 2J)
        for cell in &mut engine.backend.cells {
            *cell = CellState::default();
        }
        let DiffResult::CellDiff { diff, .. } = engine.compute_diff() else {
            panic!("expected CellDiff");
        };
        assert!(
            matches!(diff.ops.as_slice(), [DiffOp::Clear]),
            "expected DiffOp::Clear, got {:?}",
            diff.ops
        );
    }

    #[test]
    fn partial_clear_does_not_produce_clear_op() {
        let mut engine = mock_engine(4, 4);
        // Fill first row
        for c in 0..4 {
            engine.backend.cells[c] = CellState {
                c: 'X',
                ..CellState::default()
            };
        }
        let _ = engine.compute_diff();

        // Clear only first row
        for c in 0..4 {
            engine.backend.cells[c] = CellState::default();
        }
        let DiffResult::CellDiff { diff, .. } = engine.compute_diff() else {
            panic!("expected CellDiff");
        };
        let has_clear = diff.ops.iter().any(|op| matches!(op, DiffOp::Clear));
        assert!(!has_clear, "partial erase should not produce DiffOp::Clear");
    }

    #[test]
    fn resize_resets_state() {
        let mut engine = mock_engine(24, 80);
        engine.backend.cells[0] = CellState {
            c: 'A',
            ..CellState::default()
        };
        let _ = engine.compute_diff();

        engine.resize(crate::backend::BackendSize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        });
        assert_eq!(engine.rows, 30);
        assert_eq!(engine.cols, 100);
        // After resize, prev state is reset
        let _ = engine.compute_diff();
    }

    #[test]
    fn snapshot_returns_backend_state() {
        let mut engine = mock_engine(4, 4);
        engine.backend.cells[0] = CellState {
            c: 'A',
            ..CellState::default()
        };
        engine.backend.cursor_state.col = 1;

        let snap = engine.snapshot();
        assert_eq!(snap.rows, 4);
        assert_eq!(snap.cols, 4);
        assert_eq!(snap.cells[0].c, 'A');
        assert_eq!(snap.cursor.col, 1);
    }

    #[test]
    fn feed_records_bytes_in_mock() {
        let mut engine = mock_engine(4, 4);
        engine.feed(b"hello");
        assert_eq!(engine.backend.fed_bytes, b"hello");
    }

    #[test]
    fn cursor_only_returns_cursor_only_variant() {
        let mut engine = mock_engine(4, 4);
        let _ = engine.compute_diff(); // consume initial

        engine.backend.cursor_state.row = 2;
        engine.backend.cursor_state.col = 3;
        assert!(matches!(
            engine.compute_diff(),
            DiffResult::CursorOnly { .. }
        ));
    }

    #[test]
    fn cell_change_returns_cell_diff_variant() {
        let mut engine = mock_engine(4, 4);
        let _ = engine.compute_diff(); // consume initial

        engine.backend.cells[0] = CellState {
            c: 'Z',
            ..CellState::default()
        };
        assert!(matches!(engine.compute_diff(), DiffResult::CellDiff { .. }));
    }

    #[test]
    fn both_changes_returns_cell_diff() {
        let mut engine = mock_engine(4, 4);
        let _ = engine.compute_diff(); // consume initial

        engine.backend.cells[0] = CellState {
            c: 'Z',
            ..CellState::default()
        };
        engine.backend.cursor_state.col = 1;
        let DiffResult::CellDiff { diff, .. } = engine.compute_diff() else {
            panic!("expected CellDiff when both cells and cursor change");
        };
        assert!(!diff.ops.is_empty());
        assert_eq!(diff.cursor.col, 1);
    }

    #[test]
    fn no_change_returns_none() {
        let mut engine = mock_engine(4, 4);
        let _ = engine.compute_diff(); // consume initial
        assert!(matches!(engine.compute_diff(), DiffResult::None));
    }

    #[test]
    fn delegate_cursor_reads_backend() {
        let mut engine = mock_engine(4, 4);
        engine.backend.cursor_state.row = 3;
        engine.backend.cursor_state.col = 7;
        let c = engine.cursor();
        assert_eq!(c.row, 3);
        assert_eq!(c.col, 7);
    }

    #[test]
    fn delegate_modes_reads_backend() {
        let mut engine = mock_engine(4, 4);
        engine.backend.mode_flags = TermModes(TermModes::APP_CURSOR);
        assert!(engine.modes().app_cursor());
    }

    /// Helper to build mock history lines for scrollback tests.
    fn make_history_lines(count: usize, cols: usize) -> Vec<Vec<CellState>> {
        (0..count)
            .map(|i| {
                let mut line = vec![CellState::default(); cols];
                if !line.is_empty() {
                    line[0].c = char::from(b'A' + (i as u8 % 26));
                }
                line
            })
            .collect()
    }

    #[test]
    fn alt_screen_enter_exit_no_scrollback_duplication() {
        let mut engine = mock_engine(4, 4);
        // Simulate 5 lines of scrollback history on the main screen.
        engine.backend.history_len = 5;
        engine.backend.history_lines = make_history_lines(5, 4);
        let _ = engine.compute_diff(); // sync prev_history_size = 5

        // Enter alt screen: history drops to 0.
        engine.backend.alt_screen = true;
        engine.backend.history_len = 0;
        engine.backend.cells[0].c = 'X'; // fzf draws something
        let diff = engine.compute_diff();
        match diff {
            DiffResult::CellDiff {
                scrollback_lines, ..
            } => {
                assert!(
                    scrollback_lines.is_empty(),
                    "no scrollback should be emitted on alt screen"
                );
            }
            _ => panic!("expected CellDiff on alt screen entry"),
        }

        // Exit alt screen: history restored to 5.
        engine.backend.alt_screen = false;
        engine.backend.history_len = 5;
        engine.backend.cells[0].c = ' '; // main screen restored
        let diff = engine.compute_diff();
        match diff {
            DiffResult::CellDiff {
                scrollback_lines, ..
            } => {
                assert!(
                    scrollback_lines.is_empty(),
                    "no scrollback duplication on alt screen exit, got {} lines",
                    scrollback_lines.len()
                );
            }
            _ => panic!("expected CellDiff on alt screen exit"),
        }
    }

    #[test]
    fn alt_screen_with_new_lines_after_exit() {
        let mut engine = mock_engine(4, 4);
        // Start with 3 lines of history.
        engine.backend.history_len = 3;
        engine.backend.history_lines = make_history_lines(3, 4);
        let _ = engine.compute_diff(); // sync prev_history_size = 3

        // Enter alt screen.
        engine.backend.alt_screen = true;
        engine.backend.history_len = 0;
        engine.backend.cells[0].c = 'F';
        let _ = engine.compute_diff();

        // Exit alt screen with 5 lines of history (2 genuinely new).
        engine.backend.alt_screen = false;
        engine.backend.history_len = 5;
        engine.backend.history_lines = make_history_lines(5, 4);
        engine.backend.cells[0].c = ' ';
        let diff = engine.compute_diff();
        match diff {
            DiffResult::CellDiff {
                scrollback_lines, ..
            } => {
                assert_eq!(
                    scrollback_lines.len(),
                    2,
                    "should emit exactly 2 new scrollback lines"
                );
            }
            _ => panic!("expected CellDiff with new scrollback lines"),
        }
    }

    #[test]
    fn from_snapshot_no_spurious_diff() {
        // Seed a snapshot with a non-default cell at (0,0).
        let snap = {
            let rows = 4u16;
            let cols = 4u16;
            let mut cells = vec![CellState::default(); (rows * cols) as usize];
            cells[0] = CellState {
                c: 'Z',
                fg: CellColor::new(0xff, 0x00, 0x00),
                bg: CellColor::new(0x28, 0x2c, 0x34),
                ..CellState::default()
            };
            GridSnapshot {
                rows,
                cols,
                cells,
                cursor: Default::default(),
                modes: TermModes::EMPTY,
                history_total: 0,
                scrollback_tail: Vec::new(),
            }
        };

        // Build a fresh MockBackend matching the snapshot.
        let mut backend = MockBackend::new(4, 4);
        backend.cells[0] = CellState {
            c: 'Z',
            fg: CellColor::new(0xff, 0x00, 0x00),
            bg: CellColor::new(0x28, 0x2c, 0x34),
            ..CellState::default()
        };

        let mut engine = DiffEngine::from_snapshot(backend, &snap);

        // The first compute_diff should see no change (prev == current).
        let result = engine.compute_diff();
        assert!(
            matches!(result, DiffResult::None),
            "from_snapshot should seed prev_cells so there is no spurious diff; got {result:?}"
        );
    }

    #[test]
    fn history_size_and_read_delegate_to_backend() {
        let mut engine = mock_engine(4, 4);
        assert_eq!(engine.history_size(), 0);

        // Inject history into the mock.
        engine.backend.history_len = 3;
        engine.backend.history_lines = make_history_lines(3, 4);

        assert_eq!(engine.history_size(), 3);

        let lines = engine.read_history_lines(0, 3);
        assert_eq!(lines.len(), 3);
        // First cell of each line should be 'A', 'B', 'C' per make_history_lines.
        assert_eq!(lines[0][0].c, 'A');
        assert_eq!(lines[1][0].c, 'B');
        assert_eq!(lines[2][0].c, 'C');
    }

    #[test]
    fn normal_scrollback_unaffected() {
        let mut engine = mock_engine(4, 4);
        engine.backend.history_len = 0;
        engine.backend.history_lines = Vec::new();
        let _ = engine.compute_diff(); // sync

        // Add 3 lines of scrollback (normal shell usage, no alt screen).
        engine.backend.history_len = 3;
        engine.backend.history_lines = make_history_lines(3, 4);
        engine.backend.cells[0].c = 'A';
        let diff = engine.compute_diff();
        match diff {
            DiffResult::CellDiff {
                scrollback_lines, ..
            } => {
                assert_eq!(
                    scrollback_lines.len(),
                    3,
                    "should emit all 3 new scrollback lines"
                );
            }
            _ => panic!("expected CellDiff with scrollback"),
        }

        // Add 2 more lines.
        engine.backend.history_len = 5;
        engine.backend.history_lines = make_history_lines(5, 4);
        engine.backend.cells[0].c = 'B';
        let diff = engine.compute_diff();
        match diff {
            DiffResult::CellDiff {
                scrollback_lines, ..
            } => {
                assert_eq!(
                    scrollback_lines.len(),
                    2,
                    "should emit only the 2 new scrollback lines"
                );
            }
            _ => panic!("expected CellDiff with scrollback"),
        }
    }
}
