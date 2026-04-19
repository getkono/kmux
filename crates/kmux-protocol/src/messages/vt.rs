use serde::{Deserialize, Serialize};

/// Portable cell color -- resolved to RGB on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl CellColor {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Packed attribute bitfield.
///
/// Bit layout: bold=0, italic=1, underline=2, strikethrough=3,
/// inverse=4, hidden=5, dim=6, blink=7, wide_char=8, wide_char_spacer=9,
/// default_fg=10, default_bg=11.
///
/// `DEFAULT_FG` means the displayed foreground came from the terminal's
/// "default foreground" colour (i.e. no explicit colour was set).  Clients
/// should substitute their theme's foreground colour.  Likewise for
/// `DEFAULT_BG`.  Both flags account for `INVERSE`-mode cells: if INVERSE
/// is set by the server the fg/bg values in `CellState` are already swapped,
/// and the DEFAULT_* flags refer to the *displayed* position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellAttrs(pub u16);

impl CellAttrs {
    pub const EMPTY: Self = Self(0);
    pub const BOLD: u16 = 1 << 0;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const STRIKETHROUGH: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const HIDDEN: u16 = 1 << 5;
    pub const DIM: u16 = 1 << 6;
    pub const BLINK: u16 = 1 << 7;
    pub const WIDE_CHAR: u16 = 1 << 8;
    pub const WIDE_CHAR_SPACER: u16 = 1 << 9;
    /// Displayed foreground uses the terminal's default foreground colour.
    pub const DEFAULT_FG: u16 = 1 << 10;
    /// Displayed background uses the terminal's default background colour.
    pub const DEFAULT_BG: u16 = 1 << 11;

    pub fn contains(self, flag: u16) -> bool {
        self.0 & flag != 0
    }
}

/// State of a single terminal cell -- character + colors + attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellState {
    pub c: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub attrs: CellAttrs,
}

impl Default for CellState {
    fn default() -> Self {
        Self {
            c: ' ',
            // Fallback RGB values; clients should use their theme colours instead
            // when DEFAULT_FG / DEFAULT_BG are set (see CellAttrs).
            fg: CellColor::new(0xab, 0xb2, 0xbf),
            bg: CellColor::new(0x28, 0x2c, 0x34),
            attrs: CellAttrs(CellAttrs::DEFAULT_FG | CellAttrs::DEFAULT_BG),
        }
    }
}

/// Cursor shape in the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
    HollowBlock,
    Hidden,
}

/// Cursor position and appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
    pub visible: bool,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            shape: CursorShape::Block,
            visible: true,
        }
    }
}

/// Terminal mode flags sent alongside diffs.
///
/// Bit 0: APP_CURSOR (application cursor keys mode).
/// Bit 1: BRACKETED_PASTE (DEC private mode 2004).
/// Bit 2: MOUSE_REPORT_CLICK (DEC mode 1000 — normal mouse tracking).
/// Bit 3: MOUSE_DRAG (DEC mode 1002 — button-event tracking).
/// Bit 4: MOUSE_MOTION (DEC mode 1003 — any-event tracking).
/// Bit 5: SGR_MOUSE (DEC mode 1006 — SGR extended coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermModes(pub u16);

impl TermModes {
    pub const EMPTY: Self = Self(0);
    pub const APP_CURSOR: u16 = 1 << 0;
    pub const BRACKETED_PASTE: u16 = 1 << 1;
    pub const MOUSE_REPORT_CLICK: u16 = 1 << 2;
    pub const MOUSE_DRAG: u16 = 1 << 3;
    pub const MOUSE_MOTION: u16 = 1 << 4;
    pub const SGR_MOUSE: u16 = 1 << 5;

    pub fn app_cursor(self) -> bool {
        self.0 & Self::APP_CURSOR != 0
    }

    pub fn bracketed_paste(self) -> bool {
        self.0 & Self::BRACKETED_PASTE != 0
    }

    /// Whether any mouse reporting mode is active (1000, 1002, or 1003).
    pub fn mouse_report(self) -> bool {
        self.0 & (Self::MOUSE_REPORT_CLICK | Self::MOUSE_DRAG | Self::MOUSE_MOTION) != 0
    }

    /// Whether SGR extended mouse coordinates are active (mode 1006).
    pub fn sgr_mouse(self) -> bool {
        self.0 & Self::SGR_MOUSE != 0
    }
}

/// A single diff operation describing changed cells.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DiffOp {
    /// A single cell changed.
    Cell { row: u16, col: u16, cell: CellState },
    /// A contiguous run of cells changed on the same row.
    Row {
        row: u16,
        start_col: u16,
        cells: Vec<CellState>,
    },
    /// The entire screen was cleared.
    Clear,
}

/// A set of cell changes + cursor/mode state for one frame.
///
/// Scrollback lines are delivered out-of-band via
/// `ServerMessage::ScrollbackAppend`, keyed by absolute index. Clients fetch
/// gaps lazily with `ClientMessage::FetchHistory` instead of receiving every
/// line inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalDiff {
    pub ops: Vec<DiffOp>,
    pub cursor: CursorState,
    pub modes: TermModes,
    /// Absolute number of lines ever scrolled off (mirror's `history_total()`
    /// as of this frame). Clients assert monotonic growth.
    #[serde(default)]
    pub history_total: u64,
}

/// Full grid snapshot -- sent on attach or after resize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridSnapshot {
    pub rows: u16,
    pub cols: u16,
    /// Row-major cell data (length = rows * cols).
    pub cells: Vec<CellState>,
    pub cursor: CursorState,
    pub modes: TermModes,
    /// Absolute number of lines ever scrolled off from this pane.
    #[serde(default)]
    pub history_total: u64,
    /// The last N scrollback lines (width-native, oldest first). The first
    /// line's absolute index is `history_total - scrollback_tail.len()`.
    /// Empty when the pane has no scrollback yet.
    #[serde(default)]
    pub scrollback_tail: Vec<Vec<CellState>>,
}
