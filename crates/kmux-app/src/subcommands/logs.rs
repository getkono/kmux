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

/// Print a local log file to stdout, then optionally follow it: see
/// [`tail_local_log_to`], which this runs against stdout.
///
/// # Errors
///
/// As [`tail_local_log_to`].
pub async fn tail_local_log(
    path: &Path,
    lines: Option<usize>,
    follow: bool,
    not_found_hint: &str,
) -> anyhow::Result<()> {
    tail_local_log_to(path, lines, follow, not_found_hint, &mut io::stdout()).await
}

/// Print a local log file to `out`, then optionally follow it.
///
/// * `lines` — `Some(n)` prints only the last `n` lines of the existing content;
///   `None` prints the whole file.
/// * `follow` — after the initial dump, poll for and stream appended bytes like
///   `tail -f` (does not return until interrupted).
///
/// Exits the process with status 1 if the file does not exist, printing
/// `not_found_hint` so the caller can explain which process populates it.
async fn tail_local_log_to(
    path: &Path,
    lines: Option<usize>,
    follow: bool,
    not_found_hint: &str,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    if !path.exists() {
        eprintln!("Log file not found: {}\n{not_found_hint}", path.display());
        std::process::exit(1);
    }

    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;

    let start = match lines {
        Some(n) => kmux_sys::log_tail::last_n_lines_offset(&buf, n),
        None => 0,
    };
    out.write_all(&buf[start..])?;
    out.flush()?;

    if follow {
        // Seek to end and poll for new bytes, following the log across a
        // rotation (issue #207).
        file.seek(io::SeekFrom::End(0)).await?;
        let mut read_buf = vec![0u8; 4096];
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            forward_appended(path, &mut file, &mut read_buf, out).await?;
        }
    }
    Ok(())
}

/// One step of `-f`: copy what the followed log gained since the last step
/// (across a rotation, see [`kmux_sys::log_tail::read_appended`]) to `out`,
/// and return how many bytes that was. Nothing new writes nothing.
async fn forward_appended(
    path: &Path,
    file: &mut tokio::fs::File,
    buf: &mut [u8],
    out: &mut impl Write,
) -> anyhow::Result<usize> {
    let n = kmux_sys::log_tail::read_appended(path, file, buf).await?;
    out.write_all(&buf[..n])?;
    out.flush()?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::{forward_appended, tail_local_log_to};

    /// Without `follow`, the last `lines` lines of the log are written out
    /// and the call returns.
    #[tokio::test]
    async fn tail_local_log_writes_the_last_lines_and_returns() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.log");
        std::fs::write(&path, b"one\ntwo\nthree\n").unwrap();
        let mut out = Vec::new();

        tail_local_log_to(&path, Some(2), false, "hint", &mut out)
            .await
            .unwrap();

        assert_eq!(out, b"two\nthree\n");
    }

    /// A follow step forwards exactly what was appended, nothing when
    /// nothing was, and carries on in the new file after a rotation.
    #[tokio::test]
    async fn a_follow_step_forwards_what_the_log_gained() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.log");
        std::fs::write(&path, b"old\n").unwrap();
        let mut file = tokio::fs::File::open(&path).await.unwrap();
        let mut buf = [0u8; 64];
        let mut out = Vec::new();

        let n = forward_appended(&path, &mut file, &mut buf, &mut out).await;
        assert_eq!(n.unwrap(), 4);
        assert_eq!(out, b"old\n");

        let n = forward_appended(&path, &mut file, &mut buf, &mut out).await;
        assert_eq!(n.unwrap(), 0, "nothing new");
        assert_eq!(out, b"old\n");

        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        log.write_all(b"new\n").unwrap();
        std::fs::rename(&path, tmp.path().join("daemon.log.1")).unwrap();
        std::fs::write(&path, b"rotated\n").unwrap();

        forward_appended(&path, &mut file, &mut buf, &mut out)
            .await
            .unwrap();
        forward_appended(&path, &mut file, &mut buf, &mut out)
            .await
            .unwrap();
        assert_eq!(out, b"old\nnew\nrotated\n");
    }
}
