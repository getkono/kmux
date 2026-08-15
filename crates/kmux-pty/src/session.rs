use std::os::fd::IntoRawFd;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Mutex;

use crate::config::{PtyConfig, WindowSize};
use crate::error::{KmuxError, Result};
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

    /// Wrap an already-spawned (or inherited) `PtyProcess` in a session.
    ///
    /// Used when adopting a live PTY handed off from a previous daemon: the
    /// `PtyProcess` was created via [`PtyProcess::from_inherited`] against a
    /// still-alive (foreign) child.
    pub fn from_process(pty: PtyProcess) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner { pty })),
            shutdown_grace: None,
        }
    }

    /// Split into independent reader and writer halves.
    ///
    /// Each half owns a dup'd copy of the PTY master fd, giving them independent
    /// `AsyncFd` registrations. This eliminates shared-Mutex contention: the
    /// reader can block on `poll_read` without preventing the writer from
    /// completing `write_all`, and vice versa.
    pub async fn split(self) -> Result<(PtyReader, PtyWriter)> {
        let inner = self.inner.lock().await;
        let reader_io = inner.pty.io.try_clone().map_err(KmuxError::Io)?;
        let writer_io = inner.pty.io.try_clone().map_err(KmuxError::Io)?;
        drop(inner);
        Ok((
            PtyReader { io: reader_io },
            PtyWriter {
                io: Mutex::new(writer_io),
            },
        ))
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

    /// Initiate graceful shutdown without waiting for the process to exit.
    ///
    /// Sends SIGTERM and spawns a background task to SIGKILL + reap after the
    /// grace period. Sets keep-alive on the inner `PtyProcess` so its `Drop`
    /// impl does not race with the background task. Returns immediately.
    pub async fn close_nowait(self) {
        let (pid, grace) = {
            let inner = self.inner.lock().await;
            (inner.pty.pid, self.shutdown_grace)
        };
        self.set_keep_alive(true).await;
        crate::shutdown::graceful_shutdown_nowait(pid, grace);
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
            .map_err(KmuxError::Io)
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
            .map_err(KmuxError::Io)
    }

    /// Send a Unix signal to the child process.
    pub async fn send_signal(&self, signal: nix::sys::signal::Signal) -> Result<()> {
        let pid = self.inner.lock().await.pty.pid;
        crate::shutdown::send_signal(pid, signal)
    }

    /// Return the PID of the child process.
    pub async fn child_pid(&self) -> nix::unistd::Pid {
        self.inner.lock().await.pty.pid
    }

    /// Duplicate the PTY master fd into an owning handle for transfer to a
    /// successor daemon via `SCM_RIGHTS`.
    ///
    /// The returned fd shares the underlying open file description, so the child
    /// keeps its controlling terminal as long as either the original or this dup
    /// remains open — the basis for live PTY migration across a daemon handoff.
    pub async fn dup_master_fd(&self) -> Result<std::os::fd::OwnedFd> {
        self.inner
            .lock()
            .await
            .pty
            .io
            .dup_owned()
            .map_err(KmuxError::Io)
    }

    /// Enable or disable keep-alive mode.
    ///
    /// When `true`, dropping the underlying `PtyProcess` will not send SIGKILL
    /// to the child — it remains alive for reattachment after a daemon restart.
    pub async fn set_keep_alive(&self, val: bool) {
        self.inner.lock().await.pty.set_keep_alive(val);
    }
}

/// Read half of a split `PtySession`.
///
/// Owns a dup'd PTY master fd; no shared Mutex with `PtyWriter`.
pub struct PtyReader {
    io: crate::io::PtyMasterIo,
}

/// Write half of a split `PtySession`.
///
/// Owns a dup'd PTY master fd; the inner `Mutex` here is solely for interior
/// mutability (`AsyncWrite` requires `&mut self`) and is never contested since
/// only one task calls `write_all` at a time.
pub struct PtyWriter {
    io: Mutex<crate::io::PtyMasterIo>,
}

impl PtyWriter {
    /// Write bytes to the PTY (sends to child's stdin).
    pub async fn write_all(&self, data: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        self.io
            .lock()
            .await
            .write_all(data)
            .await
            .map_err(KmuxError::Io)
    }

    /// Create a no-op write handle backed by a pipe.
    ///
    /// Used to construct a `PaneRelay` for a dead (restored-but-exited)
    /// session where there is no live PTY to write to. Writes succeed but
    /// the data is silently discarded (the read end of the pipe is dropped).
    ///
    /// Pipes are epoll-able so this works with tokio's `AsyncFd`, unlike
    /// `/dev/null` which is not a pollable fd and would cause `EPERM` when
    /// registered with epoll.
    pub fn sink() -> Result<Self> {
        // Create a pipe; keep the write end, drop the read end immediately.
        // Writes to the write end will succeed until the kernel pipe buffer
        // fills up. Since nobody reads, we make the write end non-blocking so
        // that writes return `EAGAIN` rather than blocking when the buffer is
        // full — PtyWriter::write_all ignores errors for dead panes anyway.
        let (read_fd, write_fd) =
            nix::unistd::pipe().map_err(|e| KmuxError::Io(std::io::Error::from(e)))?;

        // Drop the read end immediately; the write end can still be written to.
        drop(read_fd);

        let write_raw = write_fd.into_raw_fd();
        let io = crate::io::PtyMasterIo::new(write_raw).map_err(KmuxError::Io)?;
        Ok(Self { io: Mutex::new(io) })
    }
}

impl PtyReader {
    /// Read available bytes from the PTY output.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        use tokio::io::AsyncReadExt;
        self.io.read(buf).await.map_err(KmuxError::Io)
    }

    /// Non-blocking read for output coalescing.
    ///
    /// Returns `Err(WouldBlock)` when no data is immediately available.
    pub fn try_read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.io.try_read_raw(buf)
    }

    /// Raw PTY master file descriptor. Valid for the lifetime of this reader.
    ///
    /// Used for `tcgetpgrp` polling to derive the foreground process name.
    pub fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.io.as_raw_fd()
    }
}

// AsyncRead impl for PtySession (non-split use)
impl AsyncRead for PtySession {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let Ok(mut guard) = self.inner.try_lock() else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
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
        let Ok(mut guard) = self.inner.try_lock() else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        Pin::new(&mut guard.pty.io).poll_write(cx, data)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let Ok(mut guard) = self.inner.try_lock() else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        Pin::new(&mut guard.pty.io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let Ok(mut guard) = self.inner.try_lock() else {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        };
        Pin::new(&mut guard.pty.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PtyConfig;

    fn bash_config() -> PtyConfig {
        PtyConfig::new("/bin/bash").args(["-c", "read line; echo \"got: $line\""])
    }

    /// Verify that reader and writer halves obtained from `split()` can operate
    /// concurrently without deadlocking -- the core regression test for the
    /// shared-Mutex deadlock that was fixed by giving each half its own fd.
    ///
    /// Before the fix, `writer.write_all()` would block waiting on the inner
    /// Mutex held by `reader.read()`, and `reader.read()` would never complete
    /// because the shell never received the input. With independent dup'd fds
    /// there is no shared Mutex, so both complete immediately.
    #[tokio::test]
    async fn split_reader_writer_concurrent() {
        let session = PtySession::spawn(&bash_config()).expect("spawn failed");
        // Clone before splitting: `split()` consumes its receiver, which would
        // drop the only Arc reference and SIGKILL the child.  Keeping `session`
        // alive mirrors production usage where the registry holds its own clone.
        let (mut reader, writer) = session.clone().split().await.expect("split failed");

        // Spawn writer task: send a line of input to the shell.
        let write_task = tokio::spawn(async move {
            writer.write_all(b"hello\n").await.expect("write failed");
        });

        // Read until we see the PTY echo of our input, or time out.
        // The PTY echoes "hello\r\n" back to the reader -- this is proof that
        // the write reached the kernel and the read received output concurrently.
        let read_task = tokio::spawn(async move {
            let mut output = Vec::new();
            let mut buf = [0u8; 256];
            loop {
                match tokio::time::timeout(Duration::from_secs(5), reader.read(&mut buf)).await {
                    Ok(Ok(0) | Err(_)) | Err(_) => break,
                    Ok(Ok(n)) => {
                        output.extend_from_slice(&buf[..n]);
                        // PTY echoes our input back -- that's enough to verify
                        // concurrent operation worked end-to-end.
                        if String::from_utf8_lossy(&output).contains("hello") {
                            break;
                        }
                    }
                }
            }
            output
        });

        write_task.await.expect("writer task panicked");
        let output = read_task.await.expect("reader task panicked");
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("hello"),
            "expected PTY echo of input in output, got: {text:?}"
        );
    }
}
