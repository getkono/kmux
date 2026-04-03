//! Platform abstraction layer for PTY operations.
//!
//! Isolates Linux/macOS differences so the rest of the codebase stays portable.

use nix::libc::winsize as Winsize;
use nix::sys::termios::Termios;

use crate::config::WindowSize;

/// Convert our `WindowSize` to the nix `Winsize` struct.
pub fn to_winsize(size: WindowSize) -> Winsize {
    Winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Set the PTY window size via `TIOCSWINSZ`.
pub fn set_winsize(fd: std::os::unix::io::RawFd, size: WindowSize) -> nix::Result<()> {
    let ws = to_winsize(size);
    // SAFETY: fd is a valid PTY master fd owned by the caller.
    unsafe { nix::libc::ioctl(fd, nix::libc::TIOCSWINSZ, &ws as *const Winsize) };
    Ok(())
}

/// Get default termios settings suitable for a PTY.
pub fn default_termios() -> Option<Termios> {
    // Return None to let forkpty use kernel defaults.
    None
}
