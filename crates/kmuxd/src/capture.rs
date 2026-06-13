//! Dev-only outbound frame capture for the compression-strategy investigation
//! (issue #59).
//!
//! Set `KMUX_CAPTURE_FRAMES=/path/to/file` to append every server→client frame's
//! **pre-compression** payload. The capture can then be replayed offline by
//! `kmux-protocol`'s `compression_bench` example to compare codecs and levels
//! and pick the optimal wire-compression strategy (see `docs/compression.md`).
//!
//! Record format, repeated until EOF:
//!
//! ```text
//! [u8 category sort-key][u32 big-endian payload length][payload bytes…]
//! ```
//!
//! When the env var is unset, [`record`] is a cheap no-op (a single cached
//! `OnceLock` read), so it is safe to call on the hot path.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use kmux_protocol::messages::MessageCategory;

static CAPTURE: OnceLock<Option<Mutex<File>>> = OnceLock::new();

fn sink() -> &'static Option<Mutex<File>> {
    CAPTURE.get_or_init(|| {
        let path = std::env::var_os("KMUX_CAPTURE_FRAMES")?;
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                tracing::warn!(
                    path = ?path,
                    "KMUX_CAPTURE_FRAMES is set: capturing outbound frame payloads (dev only)"
                );
                Some(Mutex::new(f))
            }
            Err(e) => {
                tracing::error!(
                    ?e,
                    "failed to open KMUX_CAPTURE_FRAMES path; capture disabled"
                );
                None
            }
        }
    })
}

/// Append one outbound frame's pre-compression payload to the capture file.
/// No-op unless `KMUX_CAPTURE_FRAMES` is set.
pub fn record(category: MessageCategory, payload: &[u8]) {
    let Some(lock) = sink() else { return };
    let Ok(mut f) = lock.lock() else { return };
    let mut header = [0u8; 5];
    header[0] = category.as_sort_key();
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    // Best-effort: a capture write failure must never disrupt a live session.
    let _ = f.write_all(&header);
    let _ = f.write_all(payload);
}
