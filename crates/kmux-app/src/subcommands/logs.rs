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
        Some(n) => kmux_protocol::log_tail::last_n_lines_offset(&buf, n),
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
