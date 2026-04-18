use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tattoy_wezterm_term::{TerminalConfiguration, color::ColorPalette};

/// Terminal configuration for the kmux server-side emulator.
///
/// The kitty feature flags are backed by shared atomics so that the daemon
/// can update them at any time (e.g. when a client attaches or detaches)
/// without rebuilding the backend.  wezterm-term queries these flags on every
/// relevant escape-sequence handler, so changes take effect immediately.
#[derive(Debug)]
pub(super) struct KmuxTerminalConfig {
    /// Whether to accept kitty graphics protocol sequences.
    pub(super) kitty_graphics: Arc<AtomicBool>,
    /// Whether to accept kitty keyboard enhancement sequences.
    pub(super) kitty_keyboard: Arc<AtomicBool>,
    /// Maximum scrollback lines to retain.
    pub(super) scrollback: usize,
}

impl TerminalConfiguration for KmuxTerminalConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    fn color_palette(&self) -> ColorPalette {
        // Use the default (xterm-256) palette. The server resolves all named /
        // indexed colours to RGB before sending to clients.
        ColorPalette::default()
    }

    fn enable_kitty_graphics(&self) -> bool {
        self.kitty_graphics.load(Ordering::Relaxed)
    }

    fn enable_kitty_keyboard(&self) -> bool {
        self.kitty_keyboard.load(Ordering::Relaxed)
    }
}
