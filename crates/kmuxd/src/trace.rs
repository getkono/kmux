//! Daemon frame-trace sink for diagnosing shell tearing (issue #72).
//!
//! When `KMUX_FRAME_TRACE` is truthy, the diff-emission path appends one
//! [`DaemonDiffRecord`] per emitted diff to `dirs::daemon_trace_path()`. The
//! `kmux debug tearing` analyzer pairs this with the client trace to
//! reconstruct logical frames and report torn frames.
//!
//! Zero-cost when disabled: [`sink`] returns `None` and [`record`] is a cheap
//! early return. A single daemon process owns one append handle guarded by a
//! mutex (concurrent pane tasks serialize their writes).

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::sync::{Mutex, OnceLock};

use kmux_protocol::trace::{DaemonDiffRecord, DiffKind};
use tracing::{info, warn};

/// Append-only JSONL handle for daemon diff records.
pub struct DiffTraceSink {
    writer: Mutex<BufWriter<std::fs::File>>,
}

impl DiffTraceSink {
    fn append(&self, record: &DaemonDiffRecord) {
        let line = match serde_json::to_string(record) {
            Ok(l) => l,
            Err(e) => {
                warn!(target: "kmux::trace", "serialize diff record failed: {e}");
                return;
            }
        };
        if let Ok(mut w) = self.writer.lock() {
            // Flush each line so a crash/kill still leaves a complete trace.
            if writeln!(w, "{line}").and_then(|()| w.flush()).is_err() {
                warn!(target: "kmux::trace", "write diff record failed");
            }
        }
    }
}

static SINK: OnceLock<Option<DiffTraceSink>> = OnceLock::new();

fn enabled() -> bool {
    std::env::var_os("KMUX_FRAME_TRACE").is_some_and(|v| !v.is_empty() && v != "0" && v != "false")
}

fn build_sink() -> Option<DiffTraceSink> {
    if !enabled() {
        return None;
    }
    let path = kmux_protocol::dirs::daemon_trace_path().ok()?;
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => Some(DiffTraceSink {
            writer: Mutex::new(BufWriter::new(file)),
        }),
        Err(e) => {
            warn!(target: "kmux::trace", "could not open daemon trace {}: {e}", path.display());
            None
        }
    }
}

fn sink() -> Option<&'static DiffTraceSink> {
    SINK.get_or_init(build_sink).as_ref()
}

/// Log a line at startup if frame tracing is active.
pub fn init_and_log() {
    if let Some(s) = sink() {
        let _ = s; // touch to force init
        if let Ok(path) = kmux_protocol::dirs::daemon_trace_path() {
            info!(
                "frame tracing ACTIVE (KMUX_FRAME_TRACE) — daemon diffs → {}",
                path.display()
            );
        }
    }
}

/// Record one emitted diff. No-op unless `KMUX_FRAME_TRACE` is set.
pub fn record(pane_id: &str, seqno: u64, sent_at_ms: u64, ops: usize, kind: DiffKind) {
    let Some(sink) = sink() else { return };
    sink.append(&DaemonDiffRecord {
        pane_id: pane_id.to_string(),
        seqno,
        sent_at_ms,
        ops,
        kind,
    });
}
