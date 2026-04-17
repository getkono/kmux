//! Rolling JSONL sink for cross-session metrics samples.
//!
//! Multiple concurrent `kmux` processes share a single file at
//! `kmux_protocol::dirs::metrics_log_path()`. Every sample is appended as a
//! single JSON object on its own line, guarded by an advisory `flock`
//! (exclusive for append, shared for read). The lock is held only for the
//! duration of the write or read — microseconds — so contention between
//! multiple clients is negligible even at the 10s flush cadence.
//!
//! When the file grows past [`ROTATE_BYTES`] it's renamed to
//! `metrics.jsonl.1` (overwriting any prior generation) and a fresh file
//! is started. One-generation rotation is deliberate: deeper history would
//! warrant a real time-series store, not an ever-growing append log.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[allow(deprecated)]
use nix::fcntl::{FlockArg, flock};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::network::{TransportCounters, TransportKey};
use super::rtt::RttSummary;

const ROTATE_BYTES: u64 = 10 * 1024 * 1024;
const SCHEMA_VERSION: u32 = 1;

/// One row appended to the metrics JSONL file. Stable schema — bump
/// `schema` on any incompatible change.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Sample {
    pub schema: u32,
    pub ts_ms: u64,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conn_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub msgs_in: u64,
    pub msgs_out: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ewma_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_recent_max_ms: Option<f64>,
    pub net_apply_avg_ms: f64,
    pub net_apply_max_ms: f64,
}

impl Sample {
    /// Build a sample from the individual collectors' current state.
    pub fn build(
        ts_ms: u64,
        conn_id: Option<u64>,
        transport: Option<&TransportKey>,
        counters: TransportCounters,
        rtt: Option<RttSummary>,
        net_apply_avg_ms: f64,
        net_apply_max_ms: f64,
    ) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            ts_ms,
            pid: std::process::id(),
            conn_id,
            transport: transport.map(|t| t.kind.to_string()),
            endpoint: transport.map(|t| t.address.clone()),
            bytes_in: counters.bytes_in,
            bytes_out: counters.bytes_out,
            msgs_in: counters.msgs_in,
            msgs_out: counters.msgs_out,
            rtt_ewma_ms: rtt.and_then(|r| r.ewma_ms),
            rtt_recent_max_ms: rtt.map(|r| r.recent_max_ms).filter(|v| *v > 0.0),
            net_apply_avg_ms,
            net_apply_max_ms,
        }
    }
}

/// File-backed sink. Holds only the resolved path — the file handle is
/// opened, locked, written, and closed on each append so concurrent
/// `kmux` processes don't hold the lock across long idle periods.
pub struct JsonlSink {
    path: PathBuf,
}

impl JsonlSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a sample. Acquires LOCK_EX briefly, rotates if the file is
    /// over [`ROTATE_BYTES`], writes one JSON line terminated by `\n`,
    /// releases the lock. Errors are logged but not propagated — metrics
    /// collection must never take down the client.
    pub fn append(&self, sample: &Sample) {
        if let Err(e) = self.append_inner(sample) {
            warn!(target: "kmux::metrics", "jsonl append failed: {e}");
        }
    }

    fn append_inner(&self, sample: &Sample) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        #[allow(deprecated)]
        flock(file.as_raw_fd(), FlockArg::LockExclusive)
            .map_err(|e| io::Error::other(format!("flock: {e}")))?;

        let len = file.metadata()?.len();
        if len >= ROTATE_BYTES {
            // Drop our lock, rotate, re-open.
            #[allow(deprecated)]
            let _ = flock(file.as_raw_fd(), FlockArg::Unlock);
            drop(file);
            self.rotate()?;
            file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            #[allow(deprecated)]
            flock(file.as_raw_fd(), FlockArg::LockExclusive)
                .map_err(|e| io::Error::other(format!("flock (post-rotate): {e}")))?;
        }

        let line = serde_json::to_string(sample)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        writeln!(file, "{line}")?;
        file.flush()?;

        #[allow(deprecated)]
        let _ = flock(file.as_raw_fd(), FlockArg::Unlock);
        Ok(())
    }

    fn rotate(&self) -> io::Result<()> {
        let rotated = self.path.with_extension("jsonl.1");
        // `rename` silently replaces the prior generation if it exists.
        std::fs::rename(&self.path, rotated)?;
        Ok(())
    }

    /// Read the last `limit` samples, newest-last. Opens with a shared
    /// lock so concurrent appenders block briefly but don't see partial
    /// lines. Skips malformed lines (from older schemas or truncated
    /// writes).
    pub fn read_history(&self, limit: usize) -> io::Result<Vec<Sample>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&self.path)?;
        #[allow(deprecated)]
        flock(file.as_raw_fd(), FlockArg::LockShared)
            .map_err(|e| io::Error::other(format!("flock read: {e}")))?;

        let mut reader = BufReader::new(&file);
        reader.seek(SeekFrom::Start(0))?;

        let mut ring: std::collections::VecDeque<Sample> =
            std::collections::VecDeque::with_capacity(limit);
        let mut line = String::new();
        while reader.read_line(&mut line)? > 0 {
            let trimmed = line.trim();
            if !trimmed.is_empty()
                && let Ok(sample) = serde_json::from_str::<Sample>(trimmed)
            {
                if ring.len() == limit && limit > 0 {
                    ring.pop_front();
                }
                ring.push_back(sample);
            }
            line.clear();
        }

        #[allow(deprecated)]
        let _ = flock(file.as_raw_fd(), FlockArg::Unlock);

        Ok(ring.into())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use kmux_protocol::messages::TransportKind;

    use super::*;

    fn sample(n: u64) -> Sample {
        Sample {
            schema: SCHEMA_VERSION,
            ts_ms: n,
            pid: std::process::id(),
            conn_id: Some(n),
            transport: Some("QUIC".into()),
            endpoint: Some("1.2.3.4:8443".into()),
            bytes_in: n,
            bytes_out: n * 2,
            msgs_in: 1,
            msgs_out: 2,
            rtt_ewma_ms: Some(25.0),
            rtt_recent_max_ms: Some(100.0),
            net_apply_avg_ms: 3.0,
            net_apply_max_ms: 12.0,
        }
    }

    #[test]
    fn round_trip_append_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::new(tmp.path().join("metrics.jsonl"));
        for i in 0..5 {
            sink.append(&sample(i));
        }
        let read = sink.read_history(100).unwrap();
        assert_eq!(read.len(), 5);
        assert_eq!(read[0].ts_ms, 0);
        assert_eq!(read[4].ts_ms, 4);
    }

    #[test]
    fn read_history_respects_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::new(tmp.path().join("metrics.jsonl"));
        for i in 0..10 {
            sink.append(&sample(i));
        }
        let read = sink.read_history(3).unwrap();
        assert_eq!(read.len(), 3);
        // Newest-last: the ring keeps the tail.
        assert_eq!(read[0].ts_ms, 7);
        assert_eq!(read[2].ts_ms, 9);
    }

    #[test]
    fn missing_file_reads_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = JsonlSink::new(tmp.path().join("does-not-exist.jsonl"));
        assert!(sink.read_history(10).unwrap().is_empty());
    }

    #[test]
    fn concurrent_writers_produce_intact_lines() {
        // Two threads each append N samples concurrently. The flock must
        // serialise writes so every line remains a valid JSON object
        // (no interleaving).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metrics.jsonl");
        let sink1 = JsonlSink::new(path.clone());
        let sink2 = JsonlSink::new(path.clone());

        const N: u64 = 64;
        let barrier = Arc::new(Barrier::new(2));
        let b1 = Arc::clone(&barrier);
        let b2 = Arc::clone(&barrier);

        let t1 = thread::spawn(move || {
            b1.wait();
            for i in 0..N {
                sink1.append(&sample(i));
            }
        });
        let t2 = thread::spawn(move || {
            b2.wait();
            for i in N..(N * 2) {
                sink2.append(&sample(i));
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();

        let sink_read = JsonlSink::new(path);
        let lines = sink_read.read_history(10_000).unwrap();
        assert_eq!(lines.len() as u64, N * 2);
    }

    #[test]
    fn build_populates_transport_fields() {
        let key = TransportKey::new(TransportKind::Uds, "/run/sock");
        let counters = TransportCounters {
            bytes_in: 10,
            bytes_out: 20,
            msgs_in: 1,
            msgs_out: 2,
        };
        let rtt = RttSummary {
            ewma_ms: Some(5.5),
            recent_avg_ms: 5.0,
            recent_max_ms: 9.0,
            sample_count: 4,
        };
        let s = Sample::build(1_000, Some(42), Some(&key), counters, Some(rtt), 1.2, 4.5);
        assert_eq!(s.transport.as_deref(), Some("UDS"));
        assert_eq!(s.endpoint.as_deref(), Some("/run/sock"));
        assert_eq!(s.bytes_in, 10);
        assert_eq!(s.rtt_ewma_ms, Some(5.5));
        assert_eq!(s.rtt_recent_max_ms, Some(9.0));
    }

    #[test]
    fn rotation_drops_prior_generation() {
        // Directly test rotate() without waiting for ROTATE_BYTES.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metrics.jsonl");
        let sink = JsonlSink::new(path.clone());
        sink.append(&sample(1));
        sink.rotate().unwrap();
        assert!(!path.exists());
        assert!(path.with_extension("jsonl.1").exists());
        sink.append(&sample(2));
        let read = sink.read_history(10).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].ts_ms, 2);
    }
}
