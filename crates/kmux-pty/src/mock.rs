//! Mock PTY test double.
//!
//! `MockPty` simulates a PTY using in-memory pipes. Useful for unit tests
//! that don't want to spawn real processes.

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf, duplex};

/// A mock PTY backed by a `tokio::io::duplex` pipe.
///
/// Data written to `MockPty` can be read back from the test-side handle,
/// and data written to the test-side handle can be read from `MockPty`.
pub struct MockPty {
    stream: DuplexStream,
}

/// The test-side handle for a `MockPty`.
pub struct MockPtyHandle {
    stream: DuplexStream,
}

impl MockPty {
    /// Create a new mock PTY pair.
    ///
    /// Returns `(pty, handle)` where `pty` is used by the code under test
    /// and `handle` is used by the test to inject/inspect data.
    pub fn new() -> (Self, MockPtyHandle) {
        let (pty_side, test_side) = duplex(65536);
        (
            Self { stream: pty_side },
            MockPtyHandle { stream: test_side },
        )
    }
}

impl AsyncRead for MockPty {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for MockPty {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

impl AsyncRead for MockPtyHandle {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for MockPtyHandle {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn roundtrip() {
        let (mut pty, mut handle) = MockPty::new();

        // Test writes to handle, pty reads it
        handle.write_all(b"hello from test").await.unwrap();
        let mut buf = [0u8; 64];
        let n = pty.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello from test");

        // Pty writes, test reads it back
        pty.write_all(b"response from pty").await.unwrap();
        let n = handle.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"response from pty");
    }
}
