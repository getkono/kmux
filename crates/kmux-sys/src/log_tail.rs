//! Shared "last N lines" computation for the log-tailing commands (issue #187).
//!
//! Used both by the local `kmux daemon logs` / `kmux client logs` reader and by
//! the daemon when answering a remote [`kmux_protocol::messages::ClientMessage::FetchLogs`]
//! so it streams only the requested tail instead of the whole file.
//!
//! The daemon log is rotated by size (issue #207), so a follower also has to
//! notice when the file it is reading has been moved aside and switch to the
//! new one at the same path: [`names_same_file`] and [`read_appended`].

use std::path::Path;

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

/// Whether `path` currently names the file `open` describes.
///
/// False once the file was renamed away (a log rotation) or removed, and
/// whenever `path` cannot be inspected. Compares device and inode, so a new
/// file created at the same path is never mistaken for the old one.
pub fn names_same_file(path: &Path, open: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .is_ok_and(|at_path| at_path.dev() == open.dev() && at_path.ino() == open.ino())
}

/// Read what has been appended to a followed log since the last read,
/// following it across a rotation (issue #207).
///
/// Returns 0 when nothing new is there yet. Once the file being read no longer
/// sits at `path` and has been read to its end, `file` is replaced with the
/// file now at `path`, read from its start. Whether it moved is checked
/// *before* reading, so every line the old file received — up to its rename
/// and after — is read before the switch. While nothing exists at `path` the
/// old file is kept.
///
/// # Errors
///
/// A failed read of the followed file.
#[cfg(feature = "framing")]
pub async fn read_appended(
    path: &Path,
    file: &mut tokio::fs::File,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    use tokio::io::AsyncReadExt;

    let rotated = !names_same_file(path, &file.metadata().await?);
    let n = file.read(buf).await?;
    if n > 0 || !rotated {
        return Ok(n);
    }
    match tokio::fs::File::open(path).await {
        Ok(current) => {
            *file = current;
            file.read(buf).await
        }
        Err(_) => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::{last_n_lines_offset, names_same_file};

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

    #[test]
    fn a_path_names_its_file_until_the_file_is_moved_or_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, b"a\n").unwrap();
        let open = std::fs::metadata(&path).unwrap();
        assert!(names_same_file(&path, &open));

        std::fs::rename(&path, dir.path().join("daemon.log.1")).unwrap();
        assert!(!names_same_file(&path, &open), "moved away");
        std::fs::write(&path, b"b\n").unwrap();
        assert!(!names_same_file(&path, &open), "a new file at the path");
    }

    /// A follower drains the old file after its rotation, then carries on
    /// with the new one from its start; with nothing new it reads nothing.
    #[tokio::test]
    async fn read_appended_follows_the_log_across_a_rotation() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let rotated = dir.path().join("daemon.log.1");
        std::fs::write(&path, b"one\n").unwrap();
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        let mut buf = [0u8; 64];
        let mut read = async |file: &mut tokio::fs::File| {
            let n = super::read_appended(&path, file, &mut buf).await.unwrap();
            String::from_utf8(buf[..n].to_vec()).unwrap()
        };

        assert_eq!(read(&mut file).await, "one\n");
        assert_eq!(read(&mut file).await, "", "nothing new yet");

        // The writer's last line into the old file, then the rotation, then
        // the first line into the new one.
        std::fs::rename(&path, &rotated).unwrap();
        let mut old = std::fs::OpenOptions::new()
            .append(true)
            .open(&rotated)
            .unwrap();
        old.write_all(b"two\n").unwrap();
        std::fs::write(&path, b"three\n").unwrap();

        assert_eq!(read(&mut file).await, "two\n", "the old file drains first");
        assert_eq!(read(&mut file).await, "three\n");
        assert_eq!(read(&mut file).await, "");
    }

    /// While the log has been moved away and not yet recreated, the follower
    /// keeps the file it has.
    #[tokio::test]
    async fn read_appended_keeps_the_old_file_until_a_new_one_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        std::fs::write(&path, b"").unwrap();
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        std::fs::remove_file(&path).unwrap();

        let mut buf = [0u8; 8];
        let n = super::read_appended(&path, &mut file, &mut buf)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
}
