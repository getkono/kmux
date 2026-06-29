//! Shared plumbing for `kmux daemon logs` / `kmux client logs`.
//!
//! Both commands read a profile-local log file (the daemon or GUI-client log),
//! optionally trimmed to the last N lines for a quick sanity check, and
//! optionally followed (`tail -f`). Deep debugging still means opening the file
//! directly — these are the at-a-glance views. `kmux daemon logs` additionally
//! fetches from a remote daemon over the data plane; that lives in
//! `daemon_cmd.rs` since only the daemon log is reachable across machines.

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Print a local log file to stdout, then optionally follow it.
///
/// * `lines` — `Some(n)` prints only the last `n` lines of the existing content;
///   `None` prints the whole file.
/// * `follow` — after the initial dump, poll for and stream appended bytes like
///   `tail -f` (does not return until interrupted).
///
/// Exits the process with status 1 if the file does not exist, printing
/// `not_found_hint` so the caller can explain which process populates it.
pub async fn tail_local_log(
    path: &Path,
    lines: Option<usize>,
    follow: bool,
    not_found_hint: &str,
) -> anyhow::Result<()> {
    if !path.exists() {
        eprintln!("Log file not found: {}\n{not_found_hint}", path.display());
        std::process::exit(1);
    }

    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;

    let start = match lines {
        Some(n) => tail_offset(&buf, n),
        None => 0,
    };
    let mut stdout = io::stdout();
    stdout.write_all(&buf[start..])?;
    stdout.flush()?;

    if follow {
        // Seek to end and poll for new bytes.
        file.seek(io::SeekFrom::End(0)).await?;
        let mut read_buf = vec![0u8; 4096];
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let n = file.read(&mut read_buf).await?;
            if n > 0 {
                stdout.write_all(&read_buf[..n])?;
                stdout.flush()?;
            }
        }
    }
    Ok(())
}

/// Byte offset where the last `n` lines of `buf` begin.
///
/// Returns 0 when `buf` holds `n` lines or fewer. A single trailing newline is
/// ignored so "last 1 line" is the final non-empty line, not the empty string
/// after it.
fn tail_offset(buf: &[u8], n: usize) -> usize {
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
    use super::tail_offset;

    #[test]
    fn tail_offset_trailing_newline() {
        let buf = b"a\nb\nc\n";
        assert_eq!(&buf[tail_offset(buf, 2)..], b"b\nc\n");
        assert_eq!(&buf[tail_offset(buf, 1)..], b"c\n");
    }

    #[test]
    fn tail_offset_no_trailing_newline() {
        let buf = b"a\nb\nc";
        assert_eq!(&buf[tail_offset(buf, 2)..], b"b\nc");
        assert_eq!(&buf[tail_offset(buf, 1)..], b"c");
    }

    #[test]
    fn tail_offset_more_than_available_returns_whole_buffer() {
        let buf = b"a\nb\n";
        assert_eq!(tail_offset(buf, 10), 0);
    }

    #[test]
    fn tail_offset_edge_cases() {
        assert_eq!(tail_offset(b"", 5), 0);
        assert_eq!(tail_offset(b"abc\n", 0), 4); // -n 0 prints nothing
    }
}
