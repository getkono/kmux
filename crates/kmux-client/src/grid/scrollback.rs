use std::collections::VecDeque;

use kmux_protocol::messages::CellState;

/// Maximum number of scrollback lines per session.
pub const MAX_SCROLLBACK_LINES: usize = 50_000;

/// Ring buffer of scrollback lines, stored oldest-first.
pub struct ScrollbackBuffer {
    lines: VecDeque<Vec<CellState>>,
    max_lines: usize,
}

impl ScrollbackBuffer {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            max_lines,
        }
    }

    /// Push new scrollback lines (oldest first).
    pub fn push_lines(&mut self, new_lines: Vec<Vec<CellState>>) {
        for line in new_lines {
            if self.lines.len() >= self.max_lines {
                self.lines.pop_front();
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

    /// Get a scrollback line by index.
    pub fn get(&self, index: usize) -> Option<&Vec<CellState>> {
        self.lines.get(index)
    }

    /// Clear all scrollback.
    pub fn clear(&mut self) {
        self.lines.clear();
    }
}

impl Default for ScrollbackBuffer {
    fn default() -> Self {
        Self::new(MAX_SCROLLBACK_LINES)
    }
}
