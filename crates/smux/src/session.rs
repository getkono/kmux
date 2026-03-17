use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Mutex;

use crate::config::{PtyConfig, WindowSize};
use crate::error::{Result, SmuxError};
use crate::process::ExitStatus;
use crate::pty::PtyProcess;
use crate::shutdown::graceful_shutdown;

/// Shared inner state of a PTY session.
struct Inner {
    pty: PtyProcess,
}

/// A detachable PTY session.
///
/// Wraps `PtyProcess` behind `Arc<Mutex<...>>` so that reader and writer
/// halves can be split off and used independently. Cloning a `PtySession`
/// produces a handle that shares the same underlying PTY process.
#[derive(Clone)]
pub struct PtySession {
    inner: Arc<Mutex<Inner>>,
    shutdown_grace: Option<Duration>,
}

impl PtySession {
    /// Spawn a new PTY session from a config.
    pub fn spawn(config: &PtyConfig) -> Result<Self> {
        let pty = PtyProcess::spawn(config)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner { pty })),
            shutdown_grace: config.timeouts.shutdown_grace,
        })
    }

    /// Split into independent reader and writer halves.
    pub fn split(self) -> (PtyReader, PtyWriter) {
        let reader = PtyReader {
            inner: Arc::clone(&self.inner),
        };
        let writer = PtyWriter {
            inner: Arc::clone(&self.inner),
        };
        // Keep self alive via the cloned Arcs; original self is dropped here
        // but the Arc refcount keeps Inner alive.
        (reader, writer)
    }

    /// Resize the PTY window.
    pub async fn resize(&self, size: WindowSize) -> Result<()> {
        self.inner.lock().await.pty.resize(size)
    }

    /// Wait for the child process to exit.
    pub async fn wait(&self) -> ExitStatus {
        self.inner.lock().await.pty.wait().await
    }

    /// Gracefully shut down the session.
    pub async fn close(self) -> Result<ExitStatus> {
        let pid = self.inner.lock().await.pty.pid;
        graceful_shutdown(pid, self.shutdown_grace).await
    }

    /// Check if the child process has exited.
    pub async fn is_exited(&self) -> bool {
        self.inner.lock().await.pty.is_exited()
    }

    /// Read bytes from the PTY output (child stdout).
    ///
    /// Holds the inner lock for the duration of the read. For concurrent
    /// read + write access, prefer splitting via [`PtySession::split`].
    pub async fn read_bytes(&self, buf: &mut [u8]) -> Result<usize> {
        use tokio::io::AsyncReadExt;
        self.inner
            .lock()
            .await
            .pty
            .io
            .read(buf)
            .await
            .map_err(SmuxError::Io)
    }

    /// Write bytes to the PTY input (child stdin).
    ///
    /// Holds the inner lock for the duration of the write. For concurrent
    /// read + write access, prefer splitting via [`PtySession::split`].
    pub async fn write_bytes(&self, data: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        self.inner
            .lock()
            .await
            .pty
            .io
            .write_all(data)
            .await
            .map_err(SmuxError::Io)
    }

    /// Send a Unix signal to the child process.
    pub async fn send_signal(&self, signal: nix::sys::signal::Signal) -> Result<()> {
        let pid = self.inner.lock().await.pty.pid;
        crate::shutdown::send_signal(pid, signal)
    }
}

/// Read half of a split `PtySession`.
pub struct PtyReader {
    inner: Arc<Mutex<Inner>>,
}

/// Write half of a split `PtySession`.
pub struct PtyWriter {
    inner: Arc<Mutex<Inner>>,
}

impl PtyWriter {
    /// Write bytes to the PTY (sends to child's stdin).
    pub async fn write_all(&self, data: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        self.inner
            .lock()
            .await
            .pty
            .io
            .write_all(data)
            .await
            .map_err(SmuxError::Io)
    }
}

impl PtyReader {
    /// Read available bytes from the PTY output.
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        use tokio::io::AsyncReadExt;
        self.inner
            .lock()
            .await
            .pty
            .io
            .read(buf)
            .await
            .map_err(SmuxError::Io)
    }
}

// AsyncRead impl for PtySession (non-split use)
impl AsyncRead for PtySession {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut guard = match self.inner.try_lock() {
            Ok(g) => g,
            Err(_) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };
        Pin::new(&mut guard.pty.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for PtySession {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut guard = match self.inner.try_lock() {
            Ok(g) => g,
            Err(_) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };
        Pin::new(&mut guard.pty.io).poll_write(cx, data)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut guard = match self.inner.try_lock() {
            Ok(g) => g,
            Err(_) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };
        Pin::new(&mut guard.pty.io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let mut guard = match self.inner.try_lock() {
            Ok(g) => g,
            Err(_) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };
        Pin::new(&mut guard.pty.io).poll_shutdown(cx)
    }
}
