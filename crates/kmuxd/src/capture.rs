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
//! call and stops capturing entirely on the first write that fails. One call is
//! not one write, though: `write_all` can land part of a record before a full
//! disk refuses the rest. So a failed append truncates the file back to the
//! length it had after the last whole record, leaving the file ending there.
//!
//! When the env var is unset, [`record`] is a cheap no-op (a single cached
//! `OnceLock` read), so it is safe to call on the hot path.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

use kmux_protocol::messages::MessageCategory;

/// `None` when capture is off; `Mutex<None>` once an append has failed and
/// capture has stopped.
static CAPTURE: OnceLock<Option<Mutex<Option<Capture<File>>>>> = OnceLock::new();

fn sink() -> &'static Option<Mutex<Option<Capture<File>>>> {
    CAPTURE.get_or_init(|| {
        let path = std::env::var_os("KMUX_CAPTURE_FRAMES")?;
        let opened = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(Capture::new);
        match opened {
            Ok(capture) => {
                tracing::warn!(
                    path = ?path,
                    "KMUX_CAPTURE_FRAMES is set: capturing outbound frame payloads (dev only)"
                );
                Some(Mutex::new(Some(capture)))
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

/// A file a capture can be cut back to a known length.
trait CaptureFile: Write {
    /// The file's current length.
    fn current_len(&self) -> io::Result<u64>;
    /// Cut the file back to `len` bytes.
    fn truncate_to(&mut self, len: u64) -> io::Result<()>;
}

impl CaptureFile for File {
    fn current_len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn truncate_to(&mut self, len: u64) -> io::Result<()> {
        // Opened for append, so the next write lands at the new end.
        self.set_len(len)
    }
}

/// Why a capture stopped.
#[derive(Debug)]
enum CaptureStop {
    TooLarge {
        len: usize,
    },
    Write(io::Error),
    Torn {
        write: io::Error,
        truncate: io::Error,
    },
}

impl std::fmt::Display for CaptureStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { len } => write!(
                f,
                "a {len}-byte frame does not fit the capture format's u32 length"
            ),
            Self::Write(e) => write!(f, "capture write failed: {e}"),
            Self::Torn { write, truncate } => write!(
                f,
                "capture write failed ({write}) and cutting back the partial record \
                 failed ({truncate}); the file may end in a torn record"
            ),
        }
    }
}

/// An open capture: the file and the length at which its last whole record
/// ends.
struct Capture<F> {
    file: F,
    whole_len: u64,
}

impl<F: CaptureFile> Capture<F> {
    fn new(file: F) -> io::Result<Self> {
        let whole_len = file.current_len()?;
        Ok(Self { file, whole_len })
    }

    /// Append one record, or say why capture must stop. On a failed write the
    /// file is cut back to its last whole record first, so whatever part of
    /// this record reached the disk does not poison the file.
    fn append(&mut self, category: MessageCategory, payload: &[u8]) -> Result<(), CaptureStop> {
        let header =
            header(category, payload.len()).ok_or(CaptureStop::TooLarge { len: payload.len() })?;
        let mut record = Vec::with_capacity(header.len() + payload.len());
        record.extend_from_slice(&header);
        record.extend_from_slice(payload);
        match self.file.write_all(&record) {
            Ok(()) => {
                self.whole_len += record.len() as u64;
                Ok(())
            }
            Err(write) => match self.file.truncate_to(self.whole_len) {
                Ok(()) => Err(CaptureStop::Write(write)),
                Err(truncate) => Err(CaptureStop::Torn { write, truncate }),
            },
        }
    }
}

/// The record header for a `len`-byte payload: its category sort key, then
/// the length as a big-endian `u32`.
///
/// `None` for a length that does not fit the `u32`. Casting it would write a
/// length smaller than the payload that follows, which is precisely the
/// corruption this format cannot survive.
fn header(category: MessageCategory, len: usize) -> Option<[u8; 5]> {
    let len = u32::try_from(len).ok()?.to_be_bytes();
    Some([category.as_sort_key(), len[0], len[1], len[2], len[3]])
}

/// Serialise one record: the header and its payload, contiguous.
#[cfg(test)]
fn encode(category: MessageCategory, payload: &[u8]) -> Option<Vec<u8>> {
    let mut record = header(category, payload.len())?.to_vec();
    record.extend_from_slice(payload);
    Some(record)
}

/// Append one outbound frame's pre-compression payload to the capture file.
/// No-op unless `KMUX_CAPTURE_FRAMES` is set.
///
/// A capture failure must not disrupt a live session, so it stops capturing
/// rather than propagating.
pub fn record(category: MessageCategory, payload: &[u8]) {
    let Some(lock) = sink() else { return };
    let Ok(mut guard) = lock.lock() else { return };
    let Some(capture) = guard.as_mut() else {
        return;
    };
    if let Err(stop) = capture.append(category, payload) {
        tracing::error!(%stop, "capture stopped");
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
        // The header takes the length, so this needs no 4 GiB allocation.
        let max = u32::MAX as usize;
        assert_eq!(
            header(MessageCategory::Control, max),
            Some([
                MessageCategory::Control.as_sort_key(),
                0xff,
                0xff,
                0xff,
                0xff
            ])
        );
        assert_eq!(header(MessageCategory::Control, max + 1), None);
    }

    /// A file that accepts `room` more bytes and then fails like a full disk —
    /// partway through a record, which is what `write_all` does not prevent.
    struct FullDisk {
        bytes: Vec<u8>,
        room: usize,
        truncate_fails: bool,
    }

    impl Write for FullDisk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.room == 0 {
                return Err(io::Error::other("no space left on device"));
            }
            let n = buf.len().min(self.room);
            self.bytes.extend_from_slice(&buf[..n]);
            self.room -= n;
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl CaptureFile for FullDisk {
        fn current_len(&self) -> io::Result<u64> {
            Ok(self.bytes.len() as u64)
        }

        fn truncate_to(&mut self, len: u64) -> io::Result<()> {
            if self.truncate_fails {
                return Err(io::Error::other("truncate refused"));
            }
            self.bytes.truncate(usize::try_from(len).expect("fits"));
            Ok(())
        }
    }

    /// A write that lands half a record before failing is cut back, so the
    /// file ends at the last whole record and stays walkable.
    #[test]
    fn a_failed_append_leaves_the_file_ending_at_the_last_whole_record() {
        let existing = encode(MessageCategory::Control, b"old").expect("encodes");
        let mut capture = Capture::new(FullDisk {
            bytes: existing.clone(),
            room: 5 + 2 + 3, // one whole two-byte record, then three bytes
            truncate_fails: false,
        })
        .expect("opens");

        capture
            .append(MessageCategory::Control, b"ok")
            .expect("room for it");
        let stop = capture
            .append(MessageCategory::Control, b"torn")
            .expect_err("the disk fills mid-record");

        assert!(matches!(stop, CaptureStop::Write(_)), "{stop}");
        let mut want = existing;
        want.extend(encode(MessageCategory::Control, b"ok").expect("encodes"));
        assert_eq!(capture.file.bytes, want, "the partial record is gone");
    }

    /// When the cut-back itself fails the file may be torn, and the stop says
    /// so rather than claiming a clean file.
    #[test]
    fn a_failed_cut_back_is_reported_as_a_torn_file() {
        let mut capture = Capture::new(FullDisk {
            bytes: Vec::new(),
            room: 3,
            truncate_fails: true,
        })
        .expect("opens");

        let stop = capture
            .append(MessageCategory::Control, b"payload")
            .expect_err("the disk fills mid-record");
        assert!(matches!(stop, CaptureStop::Torn { .. }), "{stop}");
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
