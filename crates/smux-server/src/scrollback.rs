use std::collections::VecDeque;

use smux_protocol::messages::SequenceNo;

/// A bounded ring buffer storing PTY output chunks indexed by sequence number.
///
/// Chunks are stored as `(SequenceNo, Vec<u8>)` pairs. When the total byte
/// size exceeds `capacity`, the oldest chunks are evicted from the front.
/// This enables both full snapshots (for fresh attaches) and delta replays
/// (for reconnecting clients that provide a `last_seqno`).
pub struct SeqnoBuffer {
    chunks: VecDeque<(SequenceNo, Vec<u8>)>,
    total_bytes: usize,
    capacity: usize,
}

impl SeqnoBuffer {
    /// Create a new buffer with the given byte capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            total_bytes: 0,
            capacity,
        }
    }

    /// Append a chunk tagged with `seqno`, evicting oldest chunks as needed.
    pub fn push(&mut self, seqno: SequenceNo, data: Vec<u8>) {
        let len = data.len();

        // If the new chunk alone exceeds capacity, keep only its tail.
        if len >= self.capacity {
            self.chunks.clear();
            self.total_bytes = 0;
            let tail_start = len - self.capacity;
            let tail = data[tail_start..].to_vec();
            self.chunks.push_back((seqno, tail));
            self.total_bytes = self.capacity;
            return;
        }

        // Evict oldest chunks until there is room (keep strictly under capacity).
        while self.total_bytes + len >= self.capacity {
            if let Some((_, old)) = self.chunks.pop_front() {
                self.total_bytes -= old.len();
            } else {
                break;
            }
        }

        self.total_bytes += len;
        self.chunks.push_back((seqno, data));
    }

    /// Return all chunks with `seqno > after` in order.
    ///
    /// Returns an empty vec if `after` is greater than all stored seqnos.
    /// The caller is responsible for requesting a full snapshot when the
    /// returned slice is empty but the session has produced output.
    pub fn since(&self, after: SequenceNo) -> Vec<(SequenceNo, Vec<u8>)> {
        // Find the first chunk with seqno > after using linear scan.
        // VecDeque doesn't support binary search directly; since N is typically
        // small (scrollback window) this is acceptable.
        self.chunks
            .iter()
            .filter(|(seq, _)| *seq > after)
            .map(|(seq, data)| (*seq, data.clone()))
            .collect()
    }

    /// Return a contiguous snapshot of all buffered bytes (oldest-first).
    /// Used for full-resync attaches where `last_seqno` is absent or too old.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.total_bytes);
        for (_, data) in &self.chunks {
            out.extend_from_slice(data);
        }
        out
    }

    /// The oldest sequence number still in the buffer, or `None` if empty.
    pub fn oldest_seqno(&self) -> Option<SequenceNo> {
        self.chunks.front().map(|(seq, _)| *seq)
    }

    /// The newest sequence number still in the buffer, or `None` if empty.
    #[allow(dead_code)]
    pub fn newest_seqno(&self) -> Option<SequenceNo> {
        self.chunks.back().map(|(seq, _)| *seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(n: u64) -> SequenceNo {
        SequenceNo(n)
    }

    #[test]
    fn within_capacity() {
        let mut sb = SeqnoBuffer::new(100);
        sb.push(seq(1), b"hello".to_vec());
        sb.push(seq(2), b" world".to_vec());
        assert_eq!(sb.snapshot(), b"hello world");
    }

    #[test]
    fn overflow_evicts_oldest_chunks() {
        let mut sb = SeqnoBuffer::new(10);
        sb.push(seq(1), b"0123456789".to_vec()); // exactly fills
        sb.push(seq(2), b"abc".to_vec()); // pushes over limit; chunk 1 evicted
        assert_eq!(sb.snapshot(), b"abc");
        assert_eq!(sb.oldest_seqno(), Some(seq(2)));
    }

    #[test]
    fn single_push_exceeds_capacity() {
        let mut sb = SeqnoBuffer::new(5);
        sb.push(seq(1), b"0123456789".to_vec()); // 10 bytes into a 5-byte buffer
        // Only the last 5 bytes should be kept
        assert_eq!(sb.snapshot(), b"56789");
    }

    #[test]
    fn empty_snapshot() {
        let sb = SeqnoBuffer::new(1024);
        assert!(sb.snapshot().is_empty());
        assert!(sb.oldest_seqno().is_none());
    }

    #[test]
    fn since_returns_chunks_after_seqno() {
        let mut sb = SeqnoBuffer::new(1024);
        sb.push(seq(1), b"a".to_vec());
        sb.push(seq(2), b"b".to_vec());
        sb.push(seq(3), b"c".to_vec());

        let result = sb.since(seq(1));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], (seq(2), b"b".to_vec()));
        assert_eq!(result[1], (seq(3), b"c".to_vec()));
    }

    #[test]
    fn since_returns_all_when_seqno_before_oldest() {
        let mut sb = SeqnoBuffer::new(1024);
        sb.push(seq(5), b"x".to_vec());
        sb.push(seq(6), b"y".to_vec());

        // Requesting since(0) should return everything
        let result = sb.since(seq(0));
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn since_returns_empty_when_up_to_date() {
        let mut sb = SeqnoBuffer::new(1024);
        sb.push(seq(3), b"z".to_vec());

        let result = sb.since(seq(3));
        assert!(result.is_empty());
    }

    #[test]
    fn multi_chunk_overflow_preserves_newest() {
        let mut sb = SeqnoBuffer::new(15);
        sb.push(seq(1), b"aaaaa".to_vec()); // 5
        sb.push(seq(2), b"bbbbb".to_vec()); // 10
        sb.push(seq(3), b"ccccc".to_vec()); // 15 -- chunk 1 evicted
        assert_eq!(sb.oldest_seqno(), Some(seq(2)));
        assert_eq!(sb.newest_seqno(), Some(seq(3)));
        assert_eq!(sb.since(seq(1)).len(), 2);
    }
}
