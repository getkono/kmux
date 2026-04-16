use std::collections::VecDeque;
use std::sync::Arc;

use kmux_protocol::messages::{SequenceNo, TerminalDiff};

/// A bounded ring buffer storing terminal diffs indexed by sequence number.
///
/// When the estimated total size exceeds `capacity`, the oldest diffs are
/// evicted from the front. The live `TermState` surface is the authoritative
/// snapshot -- no keyframes are stored here.
pub struct DiffBuffer {
    /// Each entry stores (seqno, diff, cached_estimated_size).
    diffs: VecDeque<(SequenceNo, Arc<TerminalDiff>, usize)>,
    total_estimated_size: usize,
    capacity: usize,
}

impl DiffBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            diffs: VecDeque::new(),
            total_estimated_size: 0,
            capacity,
        }
    }

    /// Append a diff tagged with `seqno`, evicting oldest entries as needed.
    pub fn push(&mut self, seqno: SequenceNo, diff: Arc<TerminalDiff>) {
        let size = estimate_diff_size(&diff);

        while self.total_estimated_size + size >= self.capacity {
            if let Some((_, _, old_size)) = self.diffs.pop_front() {
                self.total_estimated_size -= old_size;
            } else {
                break;
            }
        }

        self.total_estimated_size += size;
        self.diffs.push_back((seqno, diff, size));
    }

    /// Return all diffs with `seqno > after` in order.
    pub fn since(&self, after: SequenceNo) -> Vec<(SequenceNo, Arc<TerminalDiff>)> {
        self.diffs
            .iter()
            .filter(|(seq, _, _)| *seq > after)
            .map(|(seq, diff, _)| (*seq, Arc::clone(diff)))
            .collect()
    }

    /// The oldest sequence number still in the buffer, or `None` if empty.
    pub fn oldest_seqno(&self) -> Option<SequenceNo> {
        self.diffs.front().map(|(seq, _, _)| *seq)
    }

    /// The newest sequence number still in the buffer, or `None` if empty.
    #[cfg(test)]
    pub fn newest_seqno(&self) -> Option<SequenceNo> {
        self.diffs.back().map(|(seq, _, _)| *seq)
    }
}

/// Estimate the memory footprint of a diff for capacity management.
fn estimate_diff_size(diff: &TerminalDiff) -> usize {
    use kmux_protocol::messages::DiffOp;
    let ops_size: usize = diff
        .ops
        .iter()
        .map(|op| match op {
            DiffOp::Cell { .. } => 16,
            DiffOp::Row { cells, .. } => 8 + cells.len() * 16,
            DiffOp::Clear => 4,
        })
        .sum();
    ops_size + 16 // cursor + modes overhead
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::{CursorState, DiffOp, TermModes};

    fn seq(n: u64) -> SequenceNo {
        SequenceNo(n)
    }

    fn make_diff(n_cells: usize) -> Arc<TerminalDiff> {
        Arc::new(TerminalDiff {
            ops: (0..n_cells)
                .map(|i| DiffOp::Cell {
                    row: 0,
                    col: i as u16,
                    cell: kmux_protocol::messages::CellState::default(),
                })
                .collect(),
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            scrollback_lines: vec![],
        })
    }

    #[test]
    fn within_capacity() {
        let mut db = DiffBuffer::new(10000);
        db.push(seq(1), make_diff(2));
        db.push(seq(2), make_diff(3));
        assert_eq!(db.oldest_seqno(), Some(seq(1)));
        assert_eq!(db.newest_seqno(), Some(seq(2)));
    }

    #[test]
    fn overflow_evicts_oldest() {
        // Each cell op is estimated at 16 bytes + 16 overhead = 32 for single-cell diff
        let mut db = DiffBuffer::new(80);
        db.push(seq(1), make_diff(2)); // ~48 bytes
        db.push(seq(2), make_diff(2)); // ~48 bytes -- pushes over, evicts seq(1)
        assert_eq!(db.oldest_seqno(), Some(seq(2)));
    }

    #[test]
    fn empty_buffer() {
        let db = DiffBuffer::new(1024);
        assert!(db.oldest_seqno().is_none());
        assert!(db.newest_seqno().is_none());
    }

    #[test]
    fn since_returns_diffs_after_seqno() {
        let mut db = DiffBuffer::new(10000);
        db.push(seq(1), make_diff(1));
        db.push(seq(2), make_diff(1));
        db.push(seq(3), make_diff(1));

        let result = db.since(seq(1));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, seq(2));
        assert_eq!(result[1].0, seq(3));
    }

    #[test]
    fn since_returns_empty_when_up_to_date() {
        let mut db = DiffBuffer::new(10000);
        db.push(seq(3), make_diff(1));
        let result = db.since(seq(3));
        assert!(result.is_empty());
    }

    #[test]
    fn since_returns_all_when_seqno_before_oldest() {
        let mut db = DiffBuffer::new(10000);
        db.push(seq(5), make_diff(1));
        db.push(seq(6), make_diff(1));
        let result = db.since(seq(0));
        assert_eq!(result.len(), 2);
    }
}
