use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tattoy_wezterm_term::{TerminalConfiguration, color::ColorPalette};

pub(super) const SCROLLBACK_LINES: usize = 50_000;

/// Terminal configuration for the kmux server-side emulator.
///
/// The kitty feature flags are backed by shared atomics so that the daemon
/// can update them at any time (e.g. when a client attaches or detaches)
/// without rebuilding the backend.  wezterm-term queries these flags on every
/// relevant escape-sequence handler, so changes take effect immediately.
#[derive(Debug)]
pub(super) struct KmuxTerminalConfig {
    /// Whether to accept kitty graphics protocol sequences.
    /// Defaults to `false` until an attached client declares support.
    pub(super) kitty_graphics: Arc<AtomicBool>,
    /// Whether to accept kitty keyboard enhancement sequences.
    /// Defaults to `false` until an attached client declares support.
    pub(super) kitty_keyboard: Arc<AtomicBool>,
}

impl TerminalConfiguration for KmuxTerminalConfig {
    fn scrollback_size(&self) -> usize {
        SCROLLBACK_LINES
    }

    fn color_palette(&self) -> ColorPalette {
        // Use the default (xterm-256) palette. The server resolves all named /
        // indexed colours to RGB before sending to clients.  Clients then
        // substitute their own theme for cells that carried DEFAULT_FG/DEFAULT_BG.
        ColorPalette::default()
    }

    fn enable_kitty_graphics(&self) -> bool {
        // Queried per escape-sequence by wezterm-term, so changes to the
        // underlying atomic take effect on the very next advance_bytes call.
        // Phase A: image data is dropped in fill_cells_inner (TODO(images)).
        // We default this to false so we don't silently consume sequences that
        // no currently-attached client can render.
        self.kitty_graphics.load(Ordering::Relaxed)
    }

    fn enable_kitty_keyboard(&self) -> bool {
        self.kitty_keyboard.load(Ordering::Relaxed)
    }
}
