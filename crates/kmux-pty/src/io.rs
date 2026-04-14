use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Async I/O wrapper around a PTY master file descriptor.
///
/// Wraps the raw fd in tokio's `AsyncFd` so reads/writes integrate with
/// the async executor's event loop (epoll on Linux, kqueue on macOS).
pub struct PtyMasterIo {
    inner: AsyncFd<OwnedFd>,
}

/// A thin newtype that owns a raw fd and implements `AsRawFd`.
struct OwnedFd(RawFd);

impl AsRawFd for OwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        // Close the fd when this wrapper is dropped
        unsafe { nix::libc::close(self.0) };
    }
}

impl PtyMasterIo {
    /// Wrap an existing PTY master fd.
    ///
    /// # Safety
    /// The caller must ensure `fd` is a valid, open PTY master fd and that
    /// this struct takes sole ownership.
    pub fn new(fd: RawFd) -> io::Result<Self> {
        // Set the fd to non-blocking mode so AsyncFd works correctly
        let flags = unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        let rc = unsafe { nix::libc::fcntl(fd, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            inner: AsyncFd::new(OwnedFd(fd))?,
        })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }

    /// Duplicate the PTY master fd for independent concurrent I/O.
    ///
    /// The kernel handles concurrent reads and writes on PTY master fds safely,
    /// so giving reader and writer their own `AsyncFd` registrations eliminates
    /// shared-Mutex contention across async await points.
    pub fn try_clone(&self) -> io::Result<Self> {
        let fd = self.as_raw_fd();
        let new_fd = unsafe { nix::libc::dup(fd) };
        if new_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Self::new(new_fd)
    }
}

impl PtyMasterIo {
    /// Non-blocking read without interacting with the async reactor.
    ///
    /// Returns `Ok(n)` if data was available, or `Err` with `WouldBlock` if
    /// the kernel buffer is empty. Intended for coalescing burst output after
    /// an async read has already returned data.
    pub fn try_read_raw(&self, buf: &mut [u8]) -> io::Result<usize> {
        let fd = self.inner.as_raw_fd();
        let n = unsafe {
            nix::libc::read(fd, buf.as_mut_ptr() as *mut nix::libc::c_void, buf.len())
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
}

impl AsyncRead for PtyMasterIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = match self.inner.poll_read_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };

            let result = guard.try_io(|inner| {
                let fd = inner.as_raw_fd();
                let slice = buf.initialize_unfilled();
                let n = unsafe {
                    nix::libc::read(
                        fd,
                        slice.as_mut_ptr() as *mut nix::libc::c_void,
                        slice.len(),
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });

            match result {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for PtyMasterIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = match self.inner.poll_write_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };

            let result = guard.try_io(|inner| {
                let fd = inner.as_raw_fd();
                let n = unsafe {
                    nix::libc::write(fd, data.as_ptr() as *const nix::libc::c_void, data.len())
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });

            match result {
                Ok(Ok(n)) => return Poll::Ready(Ok(n)),
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // PTY master fds don't require explicit flushing
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
