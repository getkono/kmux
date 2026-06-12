//! Client per-tick frame trace for diagnosing shell tearing (issue #72).
//!
//! When `KMUX_FRAME_TRACE` is truthy, the driver appends one
//! [`ClientTickRecord`] per pump tick that applied diffs to
//! `dirs::client_trace_path()`. The `kmux debug tearing` analyzer pairs this
//! with the daemon trace to reconstruct logical frames and report torn frames.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};

use kmux_protocol::trace::ClientTickRecord;
use tracing::warn;

/// Append-only JSONL handle for client tick records. The driver runs on a
/// single thread, so no locking is needed.
pub struct ClientTraceSink {
    writer: BufWriter<File>,
}

impl ClientTraceSink {
    /// Open the sink if `KMUX_FRAME_TRACE` is set; otherwise `None` (no-op).
    pub fn from_env() -> Option<Self> {
        let enabled = std::env::var_os("KMUX_FRAME_TRACE")
            .is_some_and(|v| !v.is_empty() && v != "0" && v != "false");
        if !enabled {
            return None;
        }
        let path = kmux_protocol::dirs::client_trace_path().ok()?;
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                tracing::info!("frame tracing ACTIVE — client ticks → {}", path.display());
                Some(Self {
                    writer: BufWriter::new(file),
                })
            }
            Err(e) => {
                warn!(target: "kmux::trace", "could not open client trace {}: {e}", path.display());
                None
            }
        }
    }

    pub fn record(&mut self, rec: &ClientTickRecord) {
        let Ok(line) = serde_json::to_string(rec) else {
            return;
        };
        // Flush each line so a crash still leaves a usable trace.
        if writeln!(self.writer, "{line}")
            .and_then(|()| self.writer.flush())
            .is_err()
        {
            warn!(target: "kmux::trace", "write client tick record failed");
        }
    }
}
