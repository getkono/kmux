use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use crate::error::{Result, kmuxError};
use crate::session::PtySession;

/// Expect-style helpers for interactive PTY sessions.
pub struct ExpectSession {
    session: PtySession,
    /// Internal buffer holding unconsumed output.
    buffer: Vec<u8>,
}

impl ExpectSession {
    /// Wrap an existing `PtySession`.
    pub fn new(session: PtySession) -> Self {
        Self {
            session,
            buffer: Vec::new(),
        }
    }

    /// Wait until the output contains `pattern`, collecting output into the internal buffer.
    ///
    /// Returns the buffer contents up to and including the pattern match.
    pub async fn expect(&mut self, pattern: &str, deadline: Duration) -> Result<String> {
        let start = std::time::Instant::now();
        let mut read_buf = [0u8; 4096];

        loop {
            // Check if pattern is already in buffer
            let text = String::from_utf8_lossy(&self.buffer);
            if let Some(pos) = text.find(pattern) {
                let end = pos + pattern.len();
                let matched = text[..end].to_string();
                // Drain matched portion from buffer
                self.buffer.drain(..end);
                return Ok(matched);
            }

            let remaining = deadline.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(kmuxError::Timeout);
            }

            // Read more output
            let n = timeout(remaining, self.session.read(&mut read_buf))
                .await
                .map_err(|_| kmuxError::Timeout)?
                .map_err(kmuxError::Io)?;

            if n == 0 {
                return Err(kmuxError::Closed);
            }
            self.buffer.extend_from_slice(&read_buf[..n]);
        }
    }

    /// Send a line to the PTY (appends `\r\n` as is standard for terminals).
    pub async fn send_line(&mut self, line: &str) -> Result<()> {
        let mut data = line.as_bytes().to_vec();
        data.extend_from_slice(b"\r\n");
        self.session.write_all(&data).await.map_err(kmuxError::Io)
    }

    /// Send raw bytes to the PTY.
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        self.session.write_all(data).await.map_err(kmuxError::Io)
    }

    /// Consume the `ExpectSession` and return the inner `PtySession`.
    pub fn into_inner(self) -> PtySession {
        self.session
    }

    /// Return a reference to the unconsumed output buffer.
    pub fn buffer(&self) -> &[u8] {
        &self.buffer
    }
}
