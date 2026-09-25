//! Shared "last N lines" computation for the log-tailing commands (issue #187).
//!
//! Used both by the local `kmux daemon logs` / `kmux client logs` reader and by
//! the daemon when answering a remote [`kmux_protocol::messages::ClientMessage::FetchLogs`]
//! so it streams only the requested tail instead of the whole file.

/// Byte offset where the last `n` lines of `buf` begin.
///
/// Returns 0 when `buf` holds `n` lines or fewer (so `&buf[offset..]` is the
/// whole file). A single trailing newline is ignored, so "last 1 line" is the
/// final non-empty line rather than the empty string after it.
pub fn last_n_lines_offset(buf: &[u8], n: usize) -> usize {
    if n == 0 {
        return buf.len();
    }
    let end = if buf.last() == Some(&b'\n') {
        buf.len() - 1
    } else {
        buf.len()
    };
    let mut count = 0;
    let mut i = end;
    while i > 0 {
        if buf[i - 1] == b'\n' {
            count += 1;
            if count == n {
                return i;
            }
        }
        i -= 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::last_n_lines_offset;

    #[test]
    fn trailing_newline() {
        let buf = b"a\nb\nc\n";
        assert_eq!(&buf[last_n_lines_offset(buf, 2)..], b"b\nc\n");
        assert_eq!(&buf[last_n_lines_offset(buf, 1)..], b"c\n");
    }

    #[test]
    fn no_trailing_newline() {
        let buf = b"a\nb\nc";
        assert_eq!(&buf[last_n_lines_offset(buf, 2)..], b"b\nc");
        assert_eq!(&buf[last_n_lines_offset(buf, 1)..], b"c");
    }

    #[test]
    fn more_than_available_returns_whole_buffer() {
        assert_eq!(last_n_lines_offset(b"a\nb\n", 10), 0);
    }

    #[test]
    fn edge_cases() {
        assert_eq!(last_n_lines_offset(b"", 5), 0);
        assert_eq!(last_n_lines_offset(b"abc\n", 0), 4); // -n 0 selects nothing
    }
}
