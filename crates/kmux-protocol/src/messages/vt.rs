use std::sync::Arc;

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
/// inverse=4, hidden=5, dim=6, blink=7, `wide_char=8`, `wide_char_spacer=9`,
/// `default_fg=10`, `default_bg=11`.
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

/// One scrollback line, shared by reference.
///
/// A whole row of cells captured at the width it had when it scrolled off.
/// Lines are reference-counted so the daemon's `ScrollbackMirror` and the
/// outgoing `ScrollbackAppend` / `GridSnapshot::scrollback_tail` can share the
/// same allocation instead of deep-copying it on every scrolling frame, and so
/// fanning one append out to N clients is N `Arc` bumps rather than N grid
/// copies. Serde's `rc` feature (enabled workspace-wide) serialises `Arc<[T]>`
/// exactly as `[T]`, so this is byte-identical on the wire to a plain
/// `Vec<CellState>` under every codec — no protocol/state/worker version bump.
pub type ScrollbackLine = Arc<[CellState]>;

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
    /// Whether the inner program requested a blinking cursor (DECSCUSR
    /// `blinking_*` / DEC private mode 12). Rendering the blink is the
    /// frontend's job; this only carries the request so a steady cursor
    /// (DECSCUSR `steady_*`) is not blinked.
    pub blink: bool,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            shape: CursorShape::Block,
            visible: true,
            blink: false,
        }
    }
}

/// Terminal mode flags sent alongside diffs.
///
/// Bit 0: `APP_CURSOR` (application cursor keys mode).
/// Bit 1: `BRACKETED_PASTE` (DEC private mode 2004).
/// Bit 2: `MOUSE_REPORT_CLICK` (DEC mode 1000 — normal mouse tracking).
/// Bit 3: `MOUSE_DRAG` (DEC mode 1002 — button-event tracking).
/// Bit 4: `MOUSE_MOTION` (DEC mode 1003 — any-event tracking).
/// Bit 5: `SGR_MOUSE` (DEC mode 1006 — SGR extended coordinates).
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

    /// Whether button-event tracking is active (mode 1002): report motion only
    /// while a button is held.
    pub fn mouse_drag(self) -> bool {
        self.0 & Self::MOUSE_DRAG != 0
    }

    /// Whether any-event tracking is active (mode 1003): report every motion,
    /// even with no button held.
    pub fn mouse_motion(self) -> bool {
        self.0 & Self::MOUSE_MOTION != 0
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
    /// Set when the inner program wiped scrollback this frame (e.g. `clear`'s
    /// `CSI 3J`, or `RIS`). `Some(base)` is the new oldest absolute index: the
    /// client must drop every scrollback line below `base` before applying.
    /// `history_total` stays monotonic across the wipe (the daemon's mirror
    /// just advances its `base_index`), so any surviving lines arrive after via
    /// `ScrollbackAppend` and the normal gap-fill path.
    #[serde(default)]
    pub scrollback_reset: Option<u64>,
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
    /// Oldest absolute scrollback index the daemon can still serve (its mirror's
    /// `base_index`). Lines below this have been evicted or wiped (e.g. by a
    /// `clear`) and are unrecoverable; a reattaching client must drop any it
    /// still holds below `scrollback_base`.
    #[serde(default)]
    pub scrollback_base: u64,
    /// The last N scrollback lines (width-native, oldest first). The first
    /// line's absolute index is `history_total - scrollback_tail.len()`.
    /// Empty when the pane has no scrollback yet.
    #[serde(default)]
    pub scrollback_tail: Vec<ScrollbackLine>,
}

/// A small, dependency-free, deterministic 128-bit FNV-1a hasher used to digest
/// grid state for the desync oracle.
///
/// This is a *self-consistency* check between the server's authoritative grid
/// and a client's reconstructed grid — not an adversarial hash — so a fast
/// non-cryptographic function with a fixed basis is sufficient, and 128 bits
/// keeps accidental collisions negligible (~2^-128). It is hand-rolled (rather
/// than reusing `std::hash`) precisely because the result must be byte-stable
/// across processes and Rust versions; `DefaultHasher` guarantees neither.
struct Fnv1a128(u128);

impl Fnv1a128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u128;
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    /// Feed one cell in a fixed field order. Shared by visible cells and
    /// scrollback cells so both hash identically.
    fn write_cell(&mut self, cell: &CellState) {
        self.write(&(cell.c as u32).to_le_bytes());
        self.write(&[cell.fg.r, cell.fg.g, cell.fg.b]);
        self.write(&[cell.bg.r, cell.bg.g, cell.bg.b]);
        self.write(&cell.attrs.0.to_le_bytes());
    }

    fn write_cursor(&mut self, cursor: &CursorState) {
        self.write(&cursor.row.to_le_bytes());
        self.write(&cursor.col.to_le_bytes());
        // Map the shape to an explicit, stable discriminant — never rely on the
        // enum's in-memory repr, which is not part of the wire contract.
        let shape: u8 = match cursor.shape {
            CursorShape::Block => 0,
            CursorShape::Underline => 1,
            CursorShape::Bar => 2,
            CursorShape::HollowBlock => 3,
            CursorShape::Hidden => 4,
        };
        self.write(&[shape, cursor.visible as u8, cursor.blink as u8]);
    }

    fn finish(self) -> u128 {
        self.0
    }
}

impl GridSnapshot {
    /// Canonical 128-bit digest of the full grid state: dimensions, every cell
    /// (row-major), cursor, modes, and the scrollback envelope
    /// (`history_total`, `scrollback_base`, and the tail lines oldest-first).
    ///
    /// Two grids that render identically hash identically; any divergence in a
    /// covered field changes the digest. The server computes this over its
    /// VT-authoritative grid and the client over its reconstructed grid; an
    /// inequality means the diff stream desynced (see `ServerMessage::GridDigest`).
    ///
    /// Variable-length fields are length-prefixed so two grids sharing a prefix
    /// (e.g. one cell longer, or a char moved between the grid and scrollback)
    /// can never collide.
    pub fn digest(&self) -> u128 {
        let mut h = Fnv1a128::new();
        h.write(&self.rows.to_le_bytes());
        h.write(&self.cols.to_le_bytes());
        h.write(&(self.cells.len() as u64).to_le_bytes());
        for cell in &self.cells {
            h.write_cell(cell);
        }
        h.write_cursor(&self.cursor);
        h.write(&self.modes.0.to_le_bytes());
        h.write(&self.history_total.to_le_bytes());
        h.write(&self.scrollback_base.to_le_bytes());
        h.write(&(self.scrollback_tail.len() as u64).to_le_bytes());
        for line in &self.scrollback_tail {
            h.write(&(line.len() as u64).to_le_bytes());
            for cell in line.iter() {
                h.write_cell(cell);
            }
        }
        h.finish()
    }

    /// Digest of the live state the wire-level oracle compares: the visible grid,
    /// cursor, modes, and the scrollback *envelope* (`history_total` +
    /// `scrollback_base`) — but NOT the scrollback tail contents.
    ///
    /// The tail is excluded on purpose. The server caps its snapshot tail at a
    /// fixed window while a client accumulates full scrollback and may be
    /// transiently behind during lazy `FetchHistory`, so hashing tail *contents*
    /// would produce false mismatches. Tail content correctness is instead
    /// covered exhaustively by the deterministic diff-pipeline conformance suite,
    /// which controls both sides. This digest still catches viewport desync and
    /// scrollback *count* corruption (reset/eviction), which is what the live
    /// self-heal needs. See [`digest`](Self::digest) for the full version.
    pub fn live_digest(&self) -> u128 {
        let mut h = Fnv1a128::new();
        h.write(&self.rows.to_le_bytes());
        h.write(&self.cols.to_le_bytes());
        h.write(&(self.cells.len() as u64).to_le_bytes());
        for cell in &self.cells {
            h.write_cell(cell);
        }
        h.write_cursor(&self.cursor);
        h.write(&self.modes.0.to_le_bytes());
        h.write(&self.history_total.to_le_bytes());
        h.write(&self.scrollback_base.to_le_bytes());
        h.finish()
    }
}

#[cfg(test)]
mod digest_tests {
    use super::*;

    fn sample() -> GridSnapshot {
        GridSnapshot {
            rows: 2,
            cols: 3,
            cells: vec![CellState::default(); 6],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_base: 0,
            scrollback_tail: Vec::new(),
        }
    }

    fn glyph(c: char) -> CellState {
        CellState {
            c,
            ..CellState::default()
        }
    }

    fn sb_line(cells: Vec<CellState>) -> ScrollbackLine {
        cells.into()
    }

    #[test]
    fn digest_is_stable_and_clone_equal() {
        let s = sample();
        assert_eq!(s.digest(), s.digest(), "same value must hash the same");
        assert_eq!(s.clone().digest(), s.digest(), "a clone must hash equal");
    }

    #[test]
    fn one_cell_mutation_changes_digest() {
        let base = sample().digest();
        let mut s = sample();
        s.cells[4] = glyph('X');
        assert_ne!(base, s.digest(), "a changed cell must change the digest");
    }

    #[test]
    fn each_field_is_covered() {
        let base = sample().digest();

        let mut dims = sample();
        dims.rows = 3;
        assert_ne!(base, dims.digest(), "rows");

        let mut cols = sample();
        cols.cols = 4;
        assert_ne!(base, cols.digest(), "cols");

        let mut cur = sample();
        cur.cursor.col = 1;
        assert_ne!(base, cur.digest(), "cursor position");

        let mut shape = sample();
        shape.cursor.shape = CursorShape::Bar;
        assert_ne!(base, shape.digest(), "cursor shape");

        let mut modes = sample();
        modes.modes = TermModes(TermModes::APP_CURSOR);
        assert_ne!(base, modes.digest(), "modes");

        let mut hist = sample();
        hist.history_total = 7;
        assert_ne!(base, hist.digest(), "history_total");

        let mut sbbase = sample();
        sbbase.scrollback_base = 3;
        assert_ne!(base, sbbase.digest(), "scrollback_base");

        let mut tail = sample();
        tail.scrollback_tail = vec![sb_line(vec![glyph('a'), glyph('b')])];
        assert_ne!(base, tail.digest(), "scrollback tail content");
    }

    #[test]
    fn live_digest_ignores_tail_content_but_covers_envelope() {
        let base = sample().live_digest();

        // Tail *content* differs → full digest changes, live digest does not.
        let mut tail = sample();
        tail.scrollback_tail = vec![sb_line(vec![glyph('a')])];
        // history_total/base unchanged, only the held tail content differs.
        assert_eq!(base, tail.live_digest(), "tail content excluded from live");
        assert_ne!(
            sample().digest(),
            tail.digest(),
            "tail content in full digest"
        );

        // The envelope counts ARE covered (catches reset/eviction count drift).
        let mut hist = sample();
        hist.history_total = 9;
        assert_ne!(base, hist.live_digest(), "history_total");

        let mut sbbase = sample();
        sbbase.scrollback_base = 4;
        assert_ne!(base, sbbase.live_digest(), "scrollback_base");

        // Viewport changes are covered.
        let mut cellmut = sample();
        cellmut.cells[0] = glyph('Q');
        assert_ne!(base, cellmut.live_digest(), "viewport cell");

        let mut cur = sample();
        cur.cursor.row = 1;
        assert_ne!(base, cur.live_digest(), "cursor");
    }

    #[test]
    fn length_prefixes_prevent_boundary_collisions() {
        // A char in the last grid cell vs. the same char as a one-cell
        // scrollback line must not collide: the length prefixes disambiguate
        // where the bytes belong.
        let mut in_grid = sample();
        in_grid.cells[5] = glyph('Z');

        let mut in_scrollback = sample();
        in_scrollback.scrollback_tail = vec![sb_line(vec![glyph('Z')])];

        assert_ne!(in_grid.digest(), in_scrollback.digest());

        // An empty scrollback line is still observable (its length prefix is fed).
        let mut empty_line = sample();
        empty_line.scrollback_tail = vec![sb_line(Vec::new())];
        assert_ne!(sample().digest(), empty_line.digest());
    }
}
