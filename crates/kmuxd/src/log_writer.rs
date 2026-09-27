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
//!
//! [`RotatingFile`] is the file under it: a daemon that runs for months must
//! not grow its log without bound (issue #207), so the file is rolled over by
//! size, keeping a fixed number of older files beside it.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
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
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Best-effort: a write failure (e.g. ENOSPC) must neither propagate as a
        // panic nor poison the lock. Report the bytes as "written" so the
        // subscriber treats the event as handled and moves on.
        let _ = guard.write_all(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// When the daemon log rolls over: once it would pass `max_bytes`, it is moved
/// to `daemon.log.1` (the older ones shift up to `daemon.log.<keep>`, the
/// oldest dropped) and a fresh file is started. `max_bytes == 0` never rolls
/// over; `keep == 0` drops the old file instead of keeping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotation {
    pub max_bytes: u64,
    pub keep: u32,
}

/// An append-only log file rolled over by size ([`Rotation`]).
///
/// Rotation happens between two whole writes, never inside one, and
/// `tracing` hands each event over in one write, so a line is never split
/// across files and none is lost at the boundary. A failed rotation keeps
/// writing to the file it has and retries once another `max_bytes` has been
/// written. Before each write the file at the path is checked to still be the
/// one being written: another process writing the same log (a graceful-restart
/// predecessor) may have rotated it, or an operator removed it, and either way
/// the next line goes to the file now at the path.
pub struct RotatingFile {
    path: PathBuf,
    file: File,
    /// Bytes counted against `max_bytes` since the file was opened or rotated.
    len: u64,
    rotation: Rotation,
}

impl RotatingFile {
    /// Open (or create) the log at `path` for appending.
    ///
    /// # Errors
    ///
    /// When the file cannot be opened or inspected.
    pub fn open(path: PathBuf, rotation: Rotation) -> io::Result<Self> {
        let (file, len) = open_append(&path)?;
        Ok(Self {
            path,
            file,
            len,
            rotation,
        })
    }

    /// Point at whatever is now at the path, creating it if need be.
    fn reopen(&mut self) -> io::Result<()> {
        (self.file, self.len) = open_append(&self.path)?;
        Ok(())
    }

    /// Get ready to write `incoming` bytes: follow a rotation done elsewhere,
    /// then roll over if they would take the file past its cap.
    fn prepare(&mut self, incoming: usize) {
        let ours = self
            .file
            .metadata()
            .is_ok_and(|open| kmux_sys::log_tail::names_same_file(&self.path, &open));
        if !ours {
            let _ = self.reopen();
        }
        let max = self.rotation.max_bytes;
        if max > 0 && self.len > 0 && self.len + incoming as u64 > max && self.rotate().is_err() {
            // Keep the file we have; try again after another `max_bytes`.
            self.len = 0;
        }
    }

    /// Shift the older files up one, move the current file to `.1`, start a
    /// new one.
    fn rotate(&mut self) -> io::Result<()> {
        let keep = self.rotation.keep;
        if keep == 0 {
            std::fs::remove_file(&self.path)?;
        } else {
            for n in (1..keep).rev() {
                rename_if_present(
                    &rotated_path(&self.path, n),
                    &rotated_path(&self.path, n + 1),
                )?;
            }
            std::fs::rename(&self.path, rotated_path(&self.path, 1))?;
        }
        self.reopen()
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.prepare(buf.len());
        let n = self.file.write(buf)?;
        self.len += n as u64;
        Ok(n)
    }

    /// One whole event, so a rotation can only fall before it.
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.prepare(buf.len());
        self.file.write_all(buf)?;
        self.len += buf.len() as u64;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// `path` opened for appending, and its current length.
fn open_append(path: &Path) -> io::Result<(File, u64)> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let len = file.metadata()?.len();
    Ok((file, len))
}

/// `daemon.log` → `daemon.log.<n>`.
fn rotated_path(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Rename `from` to `to`; a `from` that does not exist yet is not an error.
fn rename_if_present(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
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

    /// A fresh log in its own state dir, as the daemon opens it.
    fn fixture_log(rotation: Rotation) -> (tempfile::TempDir, PathBuf, RotatingFile) {
        let root = tempfile::tempdir().unwrap();
        let path = kmux_sys::dirs::Dirs::rooted(root.path())
            .daemon_log_path()
            .unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let log = RotatingFile::open(path.clone(), rotation).unwrap();
        (root, path, log)
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Writing past the cap rolls the file over and keeps `keep` older ones.
    /// Every line lands whole in exactly one file, the newest line is in the
    /// current file, and the files beyond `keep` are gone.
    #[test]
    fn a_full_log_rolls_over_and_keeps_only_the_newest_files() {
        let (_root, path, mut log) = fixture_log(Rotation {
            max_bytes: 10,
            keep: 2,
        });
        // Each line is 6 bytes, so each file holds one: "line0\n" .. "line4\n".
        for i in 0..5 {
            log.write_all(format!("line{i}\n").as_bytes()).unwrap();
        }

        assert_eq!(read(&path), "line4\n", "the newest line is current");
        assert_eq!(read(&rotated_path(&path, 1)), "line3\n");
        assert_eq!(read(&rotated_path(&path, 2)), "line2\n");
        assert!(
            !rotated_path(&path, 3).exists(),
            "only `keep` older files are kept"
        );
    }

    /// Lines fill a file up to the cap before it rolls over; one that would
    /// pass the cap starts the next file instead of being split.
    #[test]
    fn a_line_that_would_pass_the_cap_starts_the_next_file() {
        let (_root, path, mut log) = fixture_log(Rotation {
            max_bytes: 12,
            keep: 1,
        });
        log.write_all(b"aaaaa\n").unwrap();
        log.write_all(b"bbbbb\n").unwrap(); // exactly 12 bytes: still fits
        log.write_all(b"ccccc\n").unwrap();

        assert_eq!(read(&rotated_path(&path, 1)), "aaaaa\nbbbbb\n");
        assert_eq!(read(&path), "ccccc\n");
    }

    /// A single line longer than the cap goes whole into an empty file rather
    /// than rolling an empty file over.
    #[test]
    fn a_line_longer_than_the_cap_is_written_whole_to_an_empty_file() {
        let (_root, path, mut log) = fixture_log(Rotation {
            max_bytes: 4,
            keep: 1,
        });
        log.write_all(b"a long line\n").unwrap();

        assert_eq!(read(&path), "a long line\n");
        assert!(!rotated_path(&path, 1).exists());
    }

    /// The size already on disk counts: a daemon restarted onto a nearly full
    /// log rolls it over on its first line.
    #[test]
    fn an_existing_log_counts_against_the_cap() {
        let rotation = Rotation {
            max_bytes: 8,
            keep: 1,
        };
        let (_root, path, log) = fixture_log(rotation);
        drop(log);
        std::fs::write(&path, b"old old\n").unwrap();
        let mut log = RotatingFile::open(path.clone(), rotation).unwrap();
        log.write_all(b"new\n").unwrap();

        assert_eq!(read(&rotated_path(&path, 1)), "old old\n");
        assert_eq!(read(&path), "new\n");
    }

    /// `keep = 0` drops the full file; `max_bytes = 0` never rolls over.
    #[test]
    fn keep_zero_drops_the_old_file_and_max_zero_never_rotates() {
        let (_root, path, mut log) = fixture_log(Rotation {
            max_bytes: 4,
            keep: 0,
        });
        log.write_all(b"one\n").unwrap();
        log.write_all(b"two\n").unwrap();
        assert_eq!(read(&path), "two\n");
        assert!(!rotated_path(&path, 1).exists());

        let (_root, path, mut log) = fixture_log(Rotation {
            max_bytes: 0,
            keep: 3,
        });
        for _ in 0..3 {
            log.write_all(b"line\n").unwrap();
        }
        assert_eq!(read(&path), "line\nline\nline\n");
    }

    /// A rotation that cannot move the file keeps writing to it, losing
    /// nothing, and tries again only once another `max_bytes` has been
    /// written. Written through `write`, so its byte count is what times the
    /// retry.
    #[test]
    fn a_failed_rotation_keeps_writing_and_retries_after_another_cap() {
        let (_root, path, mut log) = fixture_log(Rotation {
            max_bytes: 10,
            keep: 1,
        });
        // A non-empty directory where `.1` should go makes the rename fail.
        let blocker = rotated_path(&path, 1);
        std::fs::create_dir_all(blocker.join("blocker")).unwrap();
        for line in [b"one\n", b"two\n", b"six\n"] {
            assert_eq!(log.write(line).unwrap(), 4);
        }
        assert_eq!(
            read(&path),
            "one\ntwo\nsix\n",
            "the third line failed to rotate"
        );

        // The blocker is gone, but the retry waits for another 10 bytes.
        std::fs::remove_dir_all(&blocker).unwrap();
        assert_eq!(log.write(b"ten\n").unwrap(), 4);
        assert!(!blocker.exists(), "no rotation yet");
        assert_eq!(log.write(b"end\n").unwrap(), 4);
        assert_eq!(read(&blocker), "one\ntwo\nsix\nten\n");
        assert_eq!(read(&path), "end\n");
    }

    /// An older file that cannot be shifted up fails the rotation as a
    /// whole, rather than letting the current file overwrite `.1`.
    #[test]
    fn a_rotation_that_cannot_shift_the_older_files_keeps_them() {
        let (_root, path, mut log) = fixture_log(Rotation {
            max_bytes: 6,
            keep: 2,
        });
        log.write_all(b"line0\n").unwrap();
        log.write_all(b"line1\n").unwrap(); // line0 → .1
        std::fs::create_dir_all(rotated_path(&path, 2).join("blocker")).unwrap();
        log.write_all(b"line2\n").unwrap(); // .1 → .2 fails

        assert_eq!(read(&rotated_path(&path, 1)), "line0\n", ".1 kept");
        assert_eq!(read(&path), "line1\nline2\n");
    }

    /// When another writer rotated the log, or it was removed, the next line
    /// goes to the file now at the path.
    #[test]
    fn a_log_moved_away_by_someone_else_is_reopened() {
        let (_root, path, mut log) = fixture_log(Rotation {
            max_bytes: 0,
            keep: 1,
        });
        log.write_all(b"before\n").unwrap();
        std::fs::rename(&path, rotated_path(&path, 1)).unwrap();
        log.write_all(b"after\n").unwrap();
        assert_eq!(read(&path), "after\n");

        std::fs::remove_file(&path).unwrap();
        assert_eq!(log.write(b"again\n").unwrap(), 6);
        log.flush().unwrap();
        assert_eq!(read(&path), "again\n");
        assert_eq!(read(&rotated_path(&path, 1)), "before\n");
    }
}
