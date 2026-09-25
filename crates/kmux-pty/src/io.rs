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
    /// The fd is made close-on-exec here, whatever its origin (`forkpty`, an
    /// `SCM_RIGHTS` transfer, a pipe), so no later child — a shell, a
    /// `kmux-vt-worker`, a handoff successor — inherits it by accident. A child
    /// that should hold it is sent it over `SCM_RIGHTS` instead.
    ///
    /// # Safety
    /// The caller must ensure `fd` is a valid, open PTY master fd and that
    /// this struct takes sole ownership.
    pub fn new(fd: RawFd) -> io::Result<Self> {
        set_cloexec(fd)?;
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
        use std::os::fd::IntoRawFd;
        Self::new(dup_cloexec(self.as_raw_fd())?.into_raw_fd())
    }

    /// Duplicate the underlying fd into an owning handle for transfer to another
    /// process via `SCM_RIGHTS`.
    ///
    /// The returned fd refers to the same open file description as this one, so
    /// the child keeps its controlling terminal as long as *either* fd stays
    /// open — the guarantee that lets a live PTY survive a daemon handoff. The
    /// caller owns the result and is responsible for closing (or sending) it.
    pub fn dup_owned(&self) -> io::Result<std::os::fd::OwnedFd> {
        dup_cloexec(self.as_raw_fd())
    }
}

/// Set `FD_CLOEXEC` on `fd`, keeping its other descriptor flags.
fn set_cloexec(fd: RawFd) -> io::Result<()> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    // SAFETY: borrowed only for these two calls; an invalid fd is EBADF.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let flags = FdFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFD)?);
    fcntl(fd, FcntlArg::F_SETFD(flags | FdFlag::FD_CLOEXEC))?;
    Ok(())
}

/// Duplicate `fd` with `FD_CLOEXEC` already set on the copy.
///
/// `F_DUPFD_CLOEXEC` sets the flag atomically, so a concurrent fork on another
/// thread can never observe the copy without it (a `dup` followed by
/// `fcntl(F_SETFD)` would leave that window open).
fn dup_cloexec(fd: RawFd) -> io::Result<std::os::fd::OwnedFd> {
    use nix::fcntl::{FcntlArg, fcntl};
    use std::os::fd::FromRawFd;
    // SAFETY: borrowed only for this call; an invalid fd is EBADF.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let new_fd = fcntl(fd, FcntlArg::F_DUPFD_CLOEXEC(0))?;
    // SAFETY: `F_DUPFD_CLOEXEC` returned a fresh fd that nothing else owns.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(new_fd) })
}

impl PtyMasterIo {
    /// Non-blocking read without interacting with the async reactor.
    ///
    /// Returns `Ok(n)` if data was available, or `Err` with `WouldBlock` if
    /// the kernel buffer is empty. Intended for coalescing burst output after
    /// an async read has already returned data.
    pub fn try_read_raw(&self, buf: &mut [u8]) -> io::Result<usize> {
        let fd = self.inner.as_raw_fd();
        let n =
            unsafe { nix::libc::read(fd, buf.as_mut_ptr().cast::<nix::libc::c_void>(), buf.len()) };
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
                        slice.as_mut_ptr().cast::<nix::libc::c_void>(),
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
                    nix::libc::write(fd, data.as_ptr().cast::<nix::libc::c_void>(), data.len())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;

    fn is_cloexec(fd: RawFd) -> bool {
        // SAFETY: fcntl(F_GETFD) on an fd this test owns.
        let flags = unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFD) };
        flags != -1 && flags & nix::libc::FD_CLOEXEC != 0
    }

    /// Every way a master handle comes into being — adoption, `try_clone`,
    /// `dup_owned`, and re-adopting an fd that already carries the flag —
    /// leaves it close-on-exec. A pipe stands in for the PTY: the flag belongs
    /// to the descriptor, not to the device behind it, so no child is needed.
    #[tokio::test]
    async fn every_master_handle_is_close_on_exec() {
        let (read_end, _write_end) = nix::unistd::pipe().expect("pipe");
        let raw = read_end.into_raw_fd();
        // SAFETY: clears the flag on an fd this test owns, whatever `pipe` set.
        assert_ne!(unsafe { nix::libc::fcntl(raw, nix::libc::F_SETFD, 0) }, -1);
        assert!(!is_cloexec(raw), "precondition: an inheritable fd");

        let io = PtyMasterIo::new(raw).expect("adopt");
        let clone = io.try_clone().expect("try_clone");
        let owned = io.dup_owned().expect("dup_owned");
        assert!(is_cloexec(io.as_raw_fd()), "adopted fd");
        assert!(is_cloexec(clone.as_raw_fd()), "try_clone");
        assert!(is_cloexec(owned.as_raw_fd()), "dup_owned");

        let readopted = PtyMasterIo::new(owned.into_raw_fd()).expect("re-adopt");
        assert!(
            is_cloexec(readopted.as_raw_fd()),
            "re-adopting keeps the flag"
        );
    }
}
