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
//! A reader has no way to resynchronise: it learns where the next record starts
//! only by trusting the length of this one. So the file must never contain a
//! record whose header promises more bytes than follow it — everything after
//! such a record parses as garbage that still *looks* structurally valid, which
//! is worse than a short file. [`record`] therefore writes each record in one
//! call and stops capturing entirely on the first write that fails, leaving the
//! file ending at the last record that was written whole.
//!
//! When the env var is unset, [`record`] is a cheap no-op (a single cached
//! `OnceLock` read), so it is safe to call on the hot path.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use kmux_protocol::messages::MessageCategory;

/// `None` when capture is off; `Mutex<None>` once a write has failed and
/// capture has stopped.
static CAPTURE: OnceLock<Option<Mutex<Option<File>>>> = OnceLock::new();

fn sink() -> &'static Option<Mutex<Option<File>>> {
    CAPTURE.get_or_init(|| {
        let path = std::env::var_os("KMUX_CAPTURE_FRAMES")?;
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                tracing::warn!(
                    path = ?path,
                    "KMUX_CAPTURE_FRAMES is set: capturing outbound frame payloads (dev only)"
                );
                Some(Mutex::new(Some(f)))
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

/// Serialise one record: the header and its payload, contiguous.
///
/// `None` for a payload whose length does not fit the header's `u32`. Casting
/// it would write a length smaller than the payload that follows, which is
/// precisely the corruption this format cannot survive.
fn encode(category: MessageCategory, payload: &[u8]) -> Option<Vec<u8>> {
    let len = u32::try_from(payload.len()).ok()?;
    let mut record = Vec::with_capacity(5 + payload.len());
    record.push(category.as_sort_key());
    record.extend_from_slice(&len.to_be_bytes());
    record.extend_from_slice(payload);
    Some(record)
}

/// Append one outbound frame's pre-compression payload to the capture file.
/// No-op unless `KMUX_CAPTURE_FRAMES` is set.
pub fn record(category: MessageCategory, payload: &[u8]) {
    let Some(lock) = sink() else { return };
    let Ok(mut guard) = lock.lock() else { return };
    let Some(file) = guard.as_mut() else { return };

    let Some(record) = encode(category, payload) else {
        tracing::error!(
            len = payload.len(),
            "frame too large for the capture format; capture stopped"
        );
        *guard = None;
        return;
    };

    // One `write_all`, so a record is never split across two calls that could
    // fail independently. A capture write failure must not disrupt a live
    // session, so it stops capturing rather than propagating -- and stopping is
    // also what keeps the file readable: a partial record would poison every
    // record after it.
    if let Err(e) = file.write_all(&record) {
        tracing::error!(
            ?e,
            "capture write failed; capture stopped to keep the file readable"
        );
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_its_header_followed_by_exactly_its_payload() {
        let payload = b"hello";
        let record = encode(MessageCategory::Control, payload).expect("encodes");
        assert_eq!(record[0], MessageCategory::Control.as_sort_key());
        assert_eq!(&record[1..5], &5_u32.to_be_bytes());
        assert_eq!(&record[5..], payload);
        assert_eq!(record.len(), 5 + payload.len(), "no padding, no gap");
    }

    #[test]
    fn an_empty_payload_still_produces_a_well_formed_header() {
        let record = encode(MessageCategory::Control, b"").expect("encodes");
        assert_eq!(record.len(), 5);
        assert_eq!(&record[1..5], &0_u32.to_be_bytes());
    }

    /// The length is the only thing telling a reader where the next record
    /// starts, so a payload that cannot be described exactly must be refused
    /// rather than described wrongly.
    #[test]
    fn a_payload_too_long_for_the_length_field_is_refused() {
        // Constructed without allocating 4 GiB: the check is on the length.
        assert!(u32::try_from(u64::from(u32::MAX) + 1).is_err());
        // And the encoder agrees for any length it can be handed.
        let payload = vec![0_u8; 16];
        assert!(encode(MessageCategory::Control, &payload).is_some());
    }

    /// Concatenated records must be walkable start to finish using only the
    /// lengths — the property a torn record destroys.
    #[test]
    fn concatenated_records_can_be_walked_by_length_alone() {
        let payloads: [&[u8]; 3] = [b"a", b"", b"three"];
        let mut file = Vec::new();
        for p in payloads {
            file.extend(encode(MessageCategory::Control, p).expect("encodes"));
        }

        let mut read = Vec::new();
        let mut at = 0;
        while at < file.len() {
            let len =
                u32::from_be_bytes(file[at + 1..at + 5].try_into().expect("4 bytes")) as usize;
            read.push(file[at + 5..at + 5 + len].to_vec());
            at += 5 + len;
        }
        assert_eq!(at, file.len(), "the walk lands exactly on the end");
        assert_eq!(read, payloads.map(<[u8]>::to_vec));
    }
}
