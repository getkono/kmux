use std::collections::VecDeque;

use kmux_protocol::messages::CellState;

/// Maximum number of scrollback lines per session kept on the client.
///
/// The daemon's mirror has its own (larger) cap. If the user scrolls past
/// this client-side cap, the renderer issues `FetchHistory` to pull the
/// older range from the daemon.
pub const MAX_SCROLLBACK_LINES: usize = 50_000;

/// Ring buffer of scrollback lines, stored oldest-first, with absolute
/// `u64` indices shared with the daemon's `ScrollbackMirror`.
///
/// `base_index` is the absolute index of `lines[0]`; when the buffer is
/// full and a new line is appended, the front is evicted and `base_index`
/// increments. `history_total()` is `base_index + lines.len()`.
pub struct ScrollbackBuffer {
    lines: VecDeque<Vec<CellState>>,
    max_lines: usize,
    base_index: u64,
}

impl ScrollbackBuffer {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            max_lines,
            base_index: 0,
        }
    }

    /// Absolute count of lines ever appended (`base_index + len()`).
    pub fn history_total(&self) -> u64 {
        self.base_index + self.lines.len() as u64
    }

    /// Absolute index of the oldest line currently held.
    pub fn base_index(&self) -> u64 {
        self.base_index
    }

    /// Append lines that the caller claims start at absolute index
    /// `first_index`. Returns `true` when the append was contiguous (i.e.
    /// `first_index == history_total()` on entry); `false` indicates a gap
    /// or overlap, in which case the caller should clear or request a fetch.
    pub fn append_with_index(&mut self, first_index: u64, new_lines: Vec<Vec<CellState>>) -> bool {
        let expected = self.history_total();
        if first_index != expected {
            return false;
        }
        for line in new_lines {
            if self.lines.len() >= self.max_lines {
                self.lines.pop_front();
                self.base_index += 1;
            }
            self.lines.push_back(line);
        }
        true
    }

    /// Legacy contiguous append when the caller has no absolute index
    /// (v14-style `TerminalDiff.scrollback_lines`). Assumes lines attach
    /// directly after the current end.
    pub fn push_lines(&mut self, new_lines: Vec<Vec<CellState>>) {
        let _ = self.append_with_index(self.history_total(), new_lines);
    }

    /// Reseed the buffer with a contiguous tail ending at absolute
    /// `history_total`. Used by `GridSnapshot::scrollback_tail` on attach
    /// or resize — preserves existing newer lines if the snapshot is
    /// strictly older.
    pub fn seed_tail(&mut self, history_total: u64, tail: Vec<Vec<CellState>>) {
        if tail.is_empty() && history_total <= self.history_total() {
            return;
        }
        let tail_len = tail.len() as u64;
        let first_index = history_total.saturating_sub(tail_len);

        // If the seed is strictly behind what we already hold, do nothing.
        if history_total <= self.base_index() {
            return;
        }

        // Replace the buffer with the seed. Future diffs/appends will land
        // at `history_total`.
        self.lines.clear();
        self.base_index = first_index;
        for line in tail {
            if self.lines.len() >= self.max_lines {
                self.lines.pop_front();
                self.base_index += 1;
            }
            self.lines.push_back(line);
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// All currently held scrollback lines, oldest-first (cloned).
    ///
    /// The first line's absolute index is `base_index()` (equivalently
    /// `history_total() - len()`), so this pairs with `history_total()` and
    /// `base_index()` to fill the scrollback fields of a `GridSnapshot`.
    pub fn tail(&self) -> Vec<Vec<CellState>> {
        self.lines.iter().cloned().collect()
    }

    /// Get a scrollback line by buffer-local index.
    pub fn get(&self, index: usize) -> Option<&Vec<CellState>> {
        self.lines.get(index)
    }

    /// Get a scrollback line by absolute index. `None` if evicted or not
    /// yet received.
    pub fn get_absolute(&self, abs: u64) -> Option<&Vec<CellState>> {
        if abs < self.base_index {
            return None;
        }
        let local = (abs - self.base_index) as usize;
        self.lines.get(local)
    }

    /// Clear all scrollback. Resets `base_index` to 0.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.base_index = 0;
    }

    /// Wipe all held lines and re-anchor at absolute index `history_total`,
    /// keeping the absolute index space monotonic. Mirrors the daemon's
    /// `ScrollbackMirror::reset` on a `clear`/`RIS`: afterwards `history_total()`
    /// equals `history_total`, the buffer is empty, and the next contiguous
    /// append (the surviving lines) lands at `history_total`.
    pub fn reset_to(&mut self, history_total: u64) {
        self.lines.clear();
        self.base_index = history_total;
    }

    /// Drop every line older than absolute index `base` (the daemon's oldest
    /// still-serveable index). Used on (re)attach to discard lines the daemon
    /// has since evicted or wiped. If the buffer holds fewer lines than that,
    /// it empties and re-anchors `base_index` at `base`.
    pub fn evict_before(&mut self, base: u64) {
        while self.base_index < base {
            if self.lines.pop_front().is_none() {
                self.base_index = base;
                return;
            }
            self.base_index += 1;
        }
    }
}

impl Default for ScrollbackBuffer {
    fn default() -> Self {
        Self::new(MAX_SCROLLBACK_LINES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(c: char) -> Vec<CellState> {
        let mut row = vec![CellState::default(); 4];
        row[0].c = c;
        row
    }

    #[test]
    fn contiguous_append_tracks_absolute_index() {
        let mut sb = ScrollbackBuffer::new(100);
        assert!(sb.append_with_index(0, vec![line('A'), line('B')]));
        assert_eq!(sb.history_total(), 2);
        assert!(sb.append_with_index(2, vec![line('C')]));
        assert_eq!(sb.history_total(), 3);
        assert_eq!(sb.get_absolute(0).unwrap()[0].c, 'A');
        assert_eq!(sb.get_absolute(2).unwrap()[0].c, 'C');
    }

    #[test]
    fn non_contiguous_append_is_rejected() {
        let mut sb = ScrollbackBuffer::new(100);
        sb.append_with_index(0, vec![line('A')]);
        // Claim to start at 5 — gap from 1..5. Must be rejected so caller
        // can trigger a resync.
        assert!(!sb.append_with_index(5, vec![line('Z')]));
        assert_eq!(sb.history_total(), 1);
    }

    #[test]
    fn eviction_bumps_base_index() {
        let mut sb = ScrollbackBuffer::new(3);
        sb.append_with_index(0, vec![line('A'), line('B'), line('C'), line('D')]);
        assert_eq!(sb.base_index(), 1);
        assert_eq!(sb.history_total(), 4);
        assert!(sb.get_absolute(0).is_none(), "evicted line unreachable");
        assert_eq!(sb.get_absolute(1).unwrap()[0].c, 'B');
    }

    #[test]
    fn seed_tail_establishes_base_index() {
        let mut sb = ScrollbackBuffer::new(100);
        sb.seed_tail(10, vec![line('X'), line('Y')]);
        assert_eq!(sb.base_index(), 8);
        assert_eq!(sb.history_total(), 10);
        assert_eq!(sb.get_absolute(8).unwrap()[0].c, 'X');
    }

    #[test]
    fn reset_to_wipes_and_reanchors_monotonic() {
        let mut sb = ScrollbackBuffer::new(100);
        sb.append_with_index(0, vec![line('A'), line('B'), line('C')]);
        sb.reset_to(3);
        assert!(sb.is_empty());
        assert_eq!(sb.base_index(), 3);
        assert_eq!(sb.history_total(), 3);
        // Surviving lines append contiguously at the new anchor.
        assert!(sb.append_with_index(3, vec![line('D')]));
        assert_eq!(sb.get_absolute(3).unwrap()[0].c, 'D');
    }

    #[test]
    fn evict_before_drops_old_lines() {
        let mut sb = ScrollbackBuffer::new(100);
        sb.append_with_index(0, vec![line('A'), line('B'), line('C'), line('D')]);
        sb.evict_before(2);
        assert_eq!(sb.base_index(), 2);
        assert_eq!(sb.history_total(), 4);
        assert!(sb.get_absolute(1).is_none(), "evicted line unreachable");
        assert_eq!(sb.get_absolute(2).unwrap()[0].c, 'C');
    }

    #[test]
    fn evict_before_past_end_empties_and_reanchors() {
        let mut sb = ScrollbackBuffer::new(100);
        sb.append_with_index(0, vec![line('A'), line('B')]);
        // Daemon wiped everything up to index 5 (more than we hold).
        sb.evict_before(5);
        assert!(sb.is_empty());
        assert_eq!(sb.base_index(), 5);
        assert_eq!(sb.history_total(), 5);
    }

    #[test]
    fn evict_before_noop_when_base_already_ahead() {
        let mut sb = ScrollbackBuffer::new(100);
        sb.seed_tail(10, vec![line('X'), line('Y')]); // base 8
        sb.evict_before(5); // below base -> no-op
        assert_eq!(sb.base_index(), 8);
        assert_eq!(sb.history_total(), 10);
    }

    #[test]
    fn clear_resets_base_index() {
        let mut sb = ScrollbackBuffer::new(3);
        sb.append_with_index(0, vec![line('A'), line('B'), line('C'), line('D')]);
        assert!(sb.base_index() > 0);
        sb.clear();
        assert_eq!(sb.base_index(), 0);
        assert_eq!(sb.history_total(), 0);
    }
}
