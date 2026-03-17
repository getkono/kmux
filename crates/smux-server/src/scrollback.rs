use std::collections::VecDeque;

/// A bounded ring buffer for raw PTY bytes.
///
/// Stores the most recent `capacity` bytes of output. When new data would
/// exceed the capacity the oldest bytes are discarded first. This lets
/// reconnecting clients receive a replay of recent terminal output.
pub struct ScrollbackBuffer {
    buf: VecDeque<u8>,
    capacity: usize,
}

impl ScrollbackBuffer {
    /// Create a new buffer with the given byte capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity.min(1024 * 1024)),
            capacity,
        }
    }

    /// Append `data` to the buffer, evicting the oldest bytes as needed.
    pub fn push(&mut self, data: &[u8]) {
        if data.len() >= self.capacity {
            // Single push fills or exceeds capacity — keep only the tail.
            self.buf.clear();
            let tail = &data[data.len() - self.capacity..];
            self.buf.extend(tail.iter().copied());
            return;
        }

        // Drain from the front until there is room.
        let available = self.capacity - self.buf.len();
        if data.len() > available {
            let drain_count = data.len() - available;
            drop(self.buf.drain(..drain_count));
        }

        self.buf.extend(data.iter().copied());
    }

    /// Return a contiguous copy of all buffered bytes.
    pub fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_capacity() {
        let mut sb = ScrollbackBuffer::new(100);
        sb.push(b"hello");
        sb.push(b" world");
        assert_eq!(sb.snapshot(), b"hello world");
    }

    #[test]
    fn overflow_drains_oldest() {
        let mut sb = ScrollbackBuffer::new(10);
        sb.push(b"0123456789"); // exactly fills
        sb.push(b"abc"); // 3 bytes over — oldest 3 should be dropped
        assert_eq!(sb.snapshot(), b"3456789abc");
    }

    #[test]
    fn single_push_exceeds_capacity() {
        let mut sb = ScrollbackBuffer::new(5);
        sb.push(b"0123456789"); // 10 bytes into a 5-byte buffer
        // Only the last 5 bytes should be kept
        assert_eq!(sb.snapshot(), b"56789");
    }

    #[test]
    fn empty_snapshot() {
        let sb = ScrollbackBuffer::new(1024);
        assert!(sb.snapshot().is_empty());
    }
}
