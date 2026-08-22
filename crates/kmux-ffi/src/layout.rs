//! Everything positional: client rows, pane rects and sizes, dividers,
//! selection spans and scroll state.

use super::*;

/// One connected client attached to the active session (issue #146). Mirrors
/// `kmux_protocol::messages::ClientInfo`; the Swift `ConnectedClientsView`
/// renders one row per entry with a Kick button. Polled via
/// [`KmuxDriver::client_rows`]; `client_id` is the kick target for
/// [`KmuxDriver::kick_client`].
#[derive(uniffi::Record)]
pub struct FfiClientRow {
    /// Stable per-connection id, passed back to `kick_client`.
    pub client_id: u64,
    /// User-readable label `username@hostname[#N]`.
    pub label: String,
    /// Cryptographic machine identity (hex SHA-256 of the public key).
    pub machine_id: String,
    pub hostname: String,
    pub username: String,
    pub transport: String,
    /// Pane indices of the session this client is viewing.
    pub panes: Vec<u32>,
    /// True for the requester's own connection (rendered as "(you)").
    pub is_self: bool,
}

/// OSC 9;4 (ConEmu/Windows-Terminal) progress-bar state for a pane (issue #125).
/// Mirrors [`PaneProgressState`]; drives the per-pane progress bar the SwiftUI
/// frontend overlays on each tile.
#[derive(uniffi::Enum, Debug, PartialEq, Eq)]
pub enum FfiProgressState {
    /// No bar.
    Remove,
    /// Normal progress (accent).
    Set,
    /// Error (red).
    Error,
    /// Indeterminate / busy (full-width accent).
    Indeterminate,
    /// Paused / warning (amber).
    Pause,
}

impl From<PaneProgressState> for FfiProgressState {
    fn from(s: PaneProgressState) -> Self {
        match s {
            PaneProgressState::Remove => Self::Remove,
            PaneProgressState::Set => Self::Set,
            PaneProgressState::Error => Self::Error,
            PaneProgressState::Indeterminate => Self::Indeterminate,
            PaneProgressState::Pause => Self::Pause,
        }
    }
}

/// One resolved pane rectangle in the active tab, in cell coordinates within the
/// content area passed to [`KmuxDriver::layout`]. `(col, row)` is the top-left
/// corner; the frontend tiles one terminal view per rect and flags the
/// `focused` one. Mirrors `kmux_app::layout::PaneRect` plus the pane id + focus.
#[derive(uniffi::Record)]
pub struct FfiPaneRect {
    pub pane_id: String,
    pub pane_index: u32,
    pub col: u32,
    pub row: u32,
    pub cols: u32,
    pub rows: u32,
    pub focused: bool,
    /// Latest OSC 9;4 progress state for the pane (issue #125); `Remove` = no bar.
    pub progress_state: FfiProgressState,
    /// Progress percentage `0..=100`, or `None` for value-less states.
    pub progress: Option<u8>,
    /// Whether terminal output for this pane is currently withheld by a
    /// connection pause (issue #68) — drives the per-pane "Paused" badge.
    pub paused: bool,
    /// Whether this pane is marked exempt from *auto*-pause (keeps streaming when
    /// the window is backgrounded); drives the pane menu's checkmark (issue #68).
    pub no_auto_pause: bool,
}

/// A per-pane resolved size the frontend pushes down via
/// [`KmuxDriver::set_pane_sizes`] (the analog of the GTK `tiles::push_sizes`):
/// each visible pane's PTY is sized to its tile, not the whole window.
#[derive(uniffi::Record)]
pub struct FfiPaneSize {
    pub pane_id: String,
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

/// A draggable boundary between two adjacent tiles, mirroring
/// `kmux_app::layout::Divider`. Returned by [`KmuxDriver::dividers`] for
/// hit-testing + cursor; pass one back to [`KmuxDriver::apply_divider_drag`]
/// with a pointer cell to resize. `vertical_bar` is `true` for a vertical bar
/// dragged along the column axis (a col-resize), `false` for a horizontal bar
/// dragged along the row axis (a row-resize).
#[derive(uniffi::Record)]
pub struct FfiDivider {
    pub path: Vec<u32>,
    pub vertical_bar: bool,
    pub before: u32,
    pub hit_col: u32,
    pub hit_row: u32,
    pub hit_cols: u32,
    pub hit_rows: u32,
    pub pair_start: u32,
    pub pair_len: u32,
}

impl FfiDivider {
    pub(crate) fn from_layout(d: kmux_app::layout::Divider) -> Self {
        Self {
            path: d.path,
            vertical_bar: matches!(d.dir, SplitDir::Horizontal),
            before: d.before as u32,
            hit_col: d.hit_col as u32,
            hit_row: d.hit_row as u32,
            hit_cols: d.hit_cols as u32,
            hit_rows: d.hit_rows as u32,
            pair_start: d.pair_start as u32,
            pair_len: d.pair_len as u32,
        }
    }

    pub(crate) fn into_layout(self) -> kmux_app::layout::Divider {
        kmux_app::layout::Divider {
            path: self.path,
            dir: if self.vertical_bar {
                SplitDir::Horizontal
            } else {
                SplitDir::Vertical
            },
            before: self.before as usize,
            hit_col: self.hit_col as u16,
            hit_row: self.hit_row as u16,
            hit_cols: self.hit_cols as u16,
            hit_rows: self.hit_rows as u16,
            pair_start: self.pair_start as u16,
            pair_len: self.pair_len as u16,
        }
    }
}

/// One selected span on a *visible* display row (row 0 = top visible row), in
/// viewport cell coordinates (`col_start..=col_end` inclusive). Returned by
/// [`KmuxDriver::selection`] — one per visible row the selection covers,
/// computed scroll- and wrap-aware by `CellGrid`, so the wash paints over
/// scrollback rows too while scrolled into history.
#[derive(uniffi::Record)]
pub struct FfiSelectionSpan {
    pub row: u32,
    pub col_start: u32,
    pub col_end: u32,
}

/// Scrollback position for the scroll indicator: `offset` lines back from the
/// live bottom, out of `total` scrollback display rows.
#[derive(uniffi::Record)]
pub struct FfiScrollInfo {
    pub offset: u32,
    pub total: u32,
}
