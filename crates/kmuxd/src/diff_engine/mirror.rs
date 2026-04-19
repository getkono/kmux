use std::collections::VecDeque;

use kmux_protocol::messages::CellState;

/// Bounded, append-only mirror of a pane's scrollback history that is
/// independent of the underlying VT backend.
///
/// The backend (libghostty-vt) has its own scrollback ring; on resize it may
/// reflow and evict lines, and on alt-screen entry its `history_size()`
/// collapses to zero. The mirror exists so those events do not lose
/// scrollback that clients have already been told about — every line seen by
/// the diff engine is copied into the mirror before being forwarded.
///
/// Lines are addressed by an **absolute** index that never goes backwards:
/// - `history_total()` is one past the last appended line's index.
/// - `base_index()` is the oldest line currently stored; indices below this
///   have been evicted because of the capacity bound.
///
/// Width policy: lines are stored at whatever column width they had when
/// captured. Consumers that need to render them in a narrower viewport are
/// expected to wrap, not truncate.
pub struct ScrollbackMirror {
    base_index: u64,
    lines: VecDeque<Vec<CellState>>,
    cap: usize,
}

#[allow(
    dead_code,
    reason = "full API surface; extra helpers are exercised in tests and reserved for the Phase C daemon-restart persistence path"
)]
impl ScrollbackMirror {
    /// Create a mirror that holds up to `cap` lines before evicting from the
    /// front.
    pub fn new(cap: usize) -> Self {
        Self {
            base_index: 0,
            lines: VecDeque::with_capacity(cap.min(8192)),
            cap,
        }
    }

    /// Append `new_lines` in oldest-first order.
    ///
    /// Returns the absolute index of the first line appended *in this call*
    /// and the number of lines appended. The returned index is what
    /// `ServerMessage::ScrollbackAppend.first_index` should use.
    pub fn append(&mut self, new_lines: Vec<Vec<CellState>>) -> (u64, usize) {
        let first_index = self.history_total();
        let count = new_lines.len();
        for line in new_lines {
            if self.lines.len() == self.cap {
                self.lines.pop_front();
                self.base_index += 1;
            }
            self.lines.push_back(line);
        }
        (first_index, count)
    }

    /// Absolute number of lines ever appended, regardless of eviction.
    pub fn history_total(&self) -> u64 {
        self.base_index + self.lines.len() as u64
    }

    /// Absolute index of the oldest line still stored. Indices below this
    /// have been evicted and are unrecoverable from the mirror.
    pub fn base_index(&self) -> u64 {
        self.base_index
    }

    /// Number of lines currently stored.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the mirror holds any lines.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Fetch up to `count` lines starting at absolute index `start`.
    ///
    /// If `start` is below `base_index`, the returned slice begins at
    /// `base_index` and the caller is told so via the returned `first_index`.
    /// If `start` is beyond `history_total()`, returns an empty vec.
    pub fn range(&self, start: u64, count: u32) -> (u64, Vec<Vec<CellState>>) {
        let total = self.history_total();
        if start >= total || count == 0 {
            return (start.max(self.base_index), Vec::new());
        }
        let effective_start = start.max(self.base_index);
        let offset = (effective_start - self.base_index) as usize;
        let end = (offset + count as usize).min(self.lines.len());
        let slice: Vec<Vec<CellState>> = self.lines.range(offset..end).cloned().collect();
        (effective_start, slice)
    }

    /// Return the last `n` lines (cloned) for attach/resize snapshots.
    /// The returned slice is stored at `history_total() - lines.len()` onward.
    pub fn tail(&self, n: usize) -> Vec<Vec<CellState>> {
        let take = n.min(self.lines.len());
        let start = self.lines.len() - take;
        self.lines.range(start..).cloned().collect()
    }

    /// Absolute index of the first line in `tail(n)`, for the matching call.
    pub fn tail_first_index(&self, n: usize) -> u64 {
        let take = n.min(self.lines.len());
        self.history_total() - take as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ch: char, cols: usize) -> Vec<CellState> {
        let mut row = vec![CellState::default(); cols];
        row[0].c = ch;
        row
    }

    #[test]
    fn append_assigns_absolute_indices() {
        let mut m = ScrollbackMirror::new(100);
        let (first, count) = m.append(vec![line('A', 4), line('B', 4)]);
        assert_eq!(first, 0);
        assert_eq!(count, 2);
        assert_eq!(m.history_total(), 2);

        let (first, count) = m.append(vec![line('C', 4)]);
        assert_eq!(first, 2);
        assert_eq!(count, 1);
        assert_eq!(m.history_total(), 3);
    }

    #[test]
    fn eviction_bumps_base_index() {
        let mut m = ScrollbackMirror::new(3);
        m.append(vec![line('A', 4), line('B', 4), line('C', 4)]);
        assert_eq!(m.base_index(), 0);
        assert_eq!(m.history_total(), 3);

        m.append(vec![line('D', 4), line('E', 4)]);
        // Two lines evicted; base moves to 2, history_total to 5.
        assert_eq!(m.base_index(), 2);
        assert_eq!(m.history_total(), 5);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn range_clamps_to_base_and_total() {
        let mut m = ScrollbackMirror::new(10);
        m.append(vec![line('A', 4), line('B', 4), line('C', 4), line('D', 4)]);

        let (idx, lines) = m.range(1, 2);
        assert_eq!(idx, 1);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0].c, 'B');

        // Past-the-end returns empty.
        let (_, lines) = m.range(10, 5);
        assert!(lines.is_empty());

        // Zero count returns empty.
        let (_, lines) = m.range(0, 0);
        assert!(lines.is_empty());
    }

    #[test]
    fn range_after_eviction_starts_at_base() {
        let mut m = ScrollbackMirror::new(3);
        m.append(vec![line('A', 4), line('B', 4), line('C', 4), line('D', 4)]);
        // base=1, lines hold B,C,D. Requesting from 0 should clamp to base.
        let (idx, lines) = m.range(0, 5);
        assert_eq!(idx, 1);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0][0].c, 'B');
    }

    #[test]
    fn tail_returns_last_n() {
        let mut m = ScrollbackMirror::new(100);
        m.append(vec![line('A', 4), line('B', 4), line('C', 4), line('D', 4)]);
        let tail = m.tail(2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0][0].c, 'C');
        assert_eq!(tail[1][0].c, 'D');
        assert_eq!(m.tail_first_index(2), 2);
    }

    #[test]
    fn tail_caps_to_available() {
        let mut m = ScrollbackMirror::new(100);
        m.append(vec![line('A', 4), line('B', 4)]);
        let tail = m.tail(10);
        assert_eq!(tail.len(), 2);
        assert_eq!(m.tail_first_index(10), 0);
    }

    #[test]
    fn empty_mirror_is_consistent() {
        let m = ScrollbackMirror::new(10);
        assert_eq!(m.history_total(), 0);
        assert_eq!(m.base_index(), 0);
        assert_eq!(m.len(), 0);
        assert!(m.is_empty());
        assert!(m.tail(5).is_empty());
        let (idx, lines) = m.range(0, 5);
        assert_eq!(idx, 0);
        assert!(lines.is_empty());
    }
}
