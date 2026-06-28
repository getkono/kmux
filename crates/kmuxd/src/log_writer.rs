//! A tracing log writer that survives a full disk.
//!
//! The daemon logs to a file behind a `Mutex`. With the stock
//! `with_writer(std::sync::Mutex::new(file))`, a write that fails (ENOSPC on a
//! full disk) followed by a panic while the guard is held *poisons* the mutex;
//! every subsequent log call then panics on `lock().expect("poisoned")`,
//! cascading across every tokio worker and taking the whole daemon down. This is
//! the observed root cause of `kmux daemon restart` failing on a near-full disk:
//! the successor boots, can't write its log, and dies in a storm of
//! `lock poisoned` panics.
//!
//! Logging is best-effort infrastructure — it must never be fatal.
//! [`ResilientWriter`] recovers from a poisoned lock (`into_inner`) and swallows
//! write errors, so a transient or permanent I/O failure degrades to "no logs"
//! rather than a crash.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// A clonable, poison- and error-tolerant writer for `tracing`'s
/// `with_writer`. Generic over the inner writer so the failure path is unit
/// testable; production wraps a `std::fs::File`.
pub struct ResilientWriter<W> {
    inner: Arc<Mutex<W>>,
}

impl<W> ResilientWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }
}

// Manual `Clone` so we don't require `W: Clone` — we only clone the `Arc`.
impl<W> Clone for ResilientWriter<W> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// The per-event handle `MakeWriter` hands to the subscriber.
pub struct ResilientGuard<W> {
    inner: Arc<Mutex<W>>,
}

impl<W: Write> Write for ResilientGuard<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Recover from a poisoned lock instead of panicking on it.
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Best-effort: a write failure (e.g. ENOSPC) must neither propagate as a
        // panic nor poison the lock. Report the bytes as "written" so the
        // subscriber treats the event as handled and moves on.
        let _ = guard.write_all(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let _ = guard.flush();
        Ok(())
    }
}

impl<'a, W: Write + 'a> MakeWriter<'a> for ResilientWriter<W> {
    type Writer = ResilientGuard<W>;

    fn make_writer(&'a self) -> Self::Writer {
        ResilientGuard {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that always fails — stands in for a file on a full disk.
    struct AlwaysFails;
    impl Write for AlwaysFails {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("No space left on device"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("No space left on device"))
        }
    }

    #[test]
    fn swallows_write_errors_and_never_poisons() {
        let writer = ResilientWriter::new(AlwaysFails);

        // Hammer the writer from several threads. If a failed write panicked
        // while holding the lock (the old `Mutex<File>` behaviour), the lock
        // would poison and a later `lock().unwrap()` would panic — the cascade
        // that killed the daemon. Here every call must stay `Ok`.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let w = writer.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    let mut g = w.make_writer();
                    assert!(g.write(b"event line\n").is_ok());
                    assert!(g.flush().is_ok());
                }
            }));
        }
        for h in handles {
            h.join().expect("writer thread must not panic");
        }

        // The lock is still usable after all those failed writes.
        let mut g = writer.make_writer();
        assert!(g.write(b"still alive\n").is_ok());
    }

    #[test]
    fn writes_reach_the_inner_writer_on_success() {
        let writer = ResilientWriter::new(Vec::<u8>::new());
        {
            let mut g = writer.make_writer();
            g.write_all(b"hello").unwrap();
        }
        let inner = writer.inner.lock().unwrap();
        assert_eq!(&inner[..], b"hello");
    }
}
