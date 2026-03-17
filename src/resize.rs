use std::os::unix::io::RawFd;

use crate::config::WindowSize;
use crate::error::Result;
use crate::platform::set_winsize;

/// Resize a PTY by issuing `TIOCSWINSZ` on the master fd.
pub fn resize_pty(fd: RawFd, size: WindowSize) -> Result<()> {
    set_winsize(fd, size).map_err(crate::error::SmuxError::Pty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_size_default() {
        let size = WindowSize::default();
        assert_eq!(size.rows, 24);
        assert_eq!(size.cols, 80);
    }
}
