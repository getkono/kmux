use kmux_protocol::messages::CellColor;

/// An absolute position in the terminal's combined scrollback + visible grid.
/// Row 0 = oldest scrollback line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPos {
    pub row: usize,
    pub col: usize,
}

impl GridPos {
    pub fn min(a: Self, b: Self) -> Self {
        if (a.row, a.col) <= (b.row, b.col) {
            a
        } else {
            b
        }
    }
    pub fn max(a: Self, b: Self) -> Self {
        if (a.row, a.col) >= (b.row, b.col) {
            a
        } else {
            b
        }
    }
}

/// Selection mode based on click count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    Normal,
    Word,
    Line,
}

/// A text selection range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: GridPos,
    pub end: GridPos,
    pub mode: SelectionMode,
}

impl Selection {
    /// The earlier (top-left) position.
    pub fn start(&self) -> GridPos {
        GridPos::min(self.anchor, self.end)
    }
    /// The later (bottom-right) position.
    pub fn end_pos(&self) -> GridPos {
        GridPos::max(self.anchor, self.end)
    }
}

/// Double/triple-click detection timeout.
pub const MULTI_CLICK_TIMEOUT_MS: u128 = 400;

/// Default background color (One Dark). Matches `CellState::default().bg`.
pub const DEFAULT_BG: CellColor = CellColor::new(0x28, 0x2c, 0x34);
