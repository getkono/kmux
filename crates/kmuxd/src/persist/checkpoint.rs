//! Checkpoint writing: serialize daemon state to disk durably, only when it
//! changed, one writer at a time, and off the async runtime (issue #207).
//!
//! A daemon that runs for months rewrites its checkpoint every 30 s. Three
//! things matter over that span:
//!
//! - **Durability.** The state is written to a `.tmp` sibling, `fsync`ed,
//!   renamed into place, and the directory is `fsync`ed. Without the two
//!   `fsync`s a power loss can keep the rename and lose the data, leaving an
//!   empty or torn checkpoint where the last good one was.
//! - **No rewrite of what is already there.** An idle daemon's state does not
//!   change, and [`Checkpointer::write`] skips a write whose bytes match the
//!   last one that succeeded.
//! - **One writer.** The periodic loop, the shutdown path and a graceful
//!   handoff all write the same file through one [`Checkpointer`], whose lock
//!   serialises them. The final write of a shutdown or handoff *seals* it, so a
//!   periodic tick that lands afterwards cannot replace the state the next
//!   daemon reads.

use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use super::PersistedDaemonState;

/// What a checkpoint write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    /// The state was written and is on disk.
    Wrote,
    /// The state matched the last successful write, so nothing was written.
    Unchanged,
    /// A final checkpoint was already written; this one was refused.
    Sealed,
}

/// Identifies a serialized state well enough to tell whether it changed: its
/// length and a 64-bit hash. A collision only costs one skipped periodic write,
/// which the next change makes good; final writes never skip.
type Fingerprint = (usize, u64);

fn fingerprint(bytes: &[u8]) -> Fingerprint {
    let mut hasher = std::hash::DefaultHasher::new();
    bytes.hash(&mut hasher);
    (bytes.len(), hasher.finish())
}

#[derive(Debug, Default)]
struct WriterState {
    /// The last state written successfully.
    last: Option<Fingerprint>,
    /// Set by a final write: nothing may replace it.
    sealed: bool,
}

impl WriterState {
    /// [`Checkpointer::write`], under the writer's lock.
    fn write(&mut self, path: &Path, state: &PersistedDaemonState) -> anyhow::Result<Written> {
        if self.sealed {
            return Ok(Written::Sealed);
        }
        let bytes = serialize(state)?;
        let print = fingerprint(&bytes);
        if self.last == Some(print) {
            return Ok(Written::Unchanged);
        }
        write_durable(path, &bytes)?;
        self.last = Some(print);
        Ok(Written::Wrote)
    }

    /// [`Checkpointer::write_final`], under the writer's lock.
    fn write_final(&mut self, path: &Path, state: &PersistedDaemonState) -> anyhow::Result<()> {
        let bytes = serialize(state)?;
        write_durable(path, &bytes)?;
        self.last = Some(fingerprint(&bytes));
        self.sealed = true;
        Ok(())
    }
}

/// The one writer of the daemon's checkpoint file. See the module docs.
#[derive(Debug)]
pub struct Checkpointer {
    path: PathBuf,
    state: Mutex<WriterState>,
}

impl Checkpointer {
    /// A writer for the checkpoint at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(WriterState::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WriterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Write `state` unless it matches the last successful write or a final
    /// checkpoint has been written. Blocking: see
    /// [`Self::write_in_background`].
    ///
    /// # Errors
    ///
    /// When `state` cannot be serialized or written. Nothing is recorded, so
    /// the next write tries again.
    pub fn write(&self, state: &PersistedDaemonState) -> anyhow::Result<Written> {
        self.lock().write(&self.path, state)
    }

    /// Write `state` whether or not it changed, then seal the checkpoint so
    /// no later [`Self::write`] replaces it. For the last write of a daemon
    /// that is shutting down or handing off. Blocking.
    ///
    /// # Errors
    ///
    /// When `state` cannot be serialized or written; the checkpoint is then
    /// left unsealed.
    pub fn write_final(&self, state: &PersistedDaemonState) -> anyhow::Result<()> {
        self.lock().write_final(&self.path, state)
    }

    /// [`Self::write`] on the blocking pool, so the `fsync`s never stall a
    /// runtime thread.
    ///
    /// # Errors
    ///
    /// As [`Self::write`], or when the blocking task panicked.
    pub async fn write_in_background(
        self: &Arc<Self>,
        state: PersistedDaemonState,
    ) -> anyhow::Result<Written> {
        let writer = Arc::clone(self);
        tokio::task::spawn_blocking(move || writer.write(&state)).await?
    }

    /// [`Self::write_final`] on the blocking pool.
    ///
    /// # Errors
    ///
    /// As [`Self::write_final`], or when the blocking task panicked.
    pub async fn write_final_in_background(
        self: &Arc<Self>,
        state: PersistedDaemonState,
    ) -> anyhow::Result<()> {
        let writer = Arc::clone(self);
        tokio::task::spawn_blocking(move || writer.write_final(&state)).await?
    }
}

fn serialize(state: &PersistedDaemonState) -> anyhow::Result<Vec<u8>> {
    postcard::to_allocvec(state)
        .map_err(|e| anyhow::anyhow!("checkpoint serialization failed: {e}"))
}

/// Replace the file at `path` with `bytes` so that after a crash or power loss
/// it holds either the old contents or the new ones, never a mix or nothing:
/// write a `.tmp` sibling and `fsync` it, rename it into place, then `fsync`
/// the directory so the rename itself is on disk.
fn write_durable(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp_path = path.with_extension("bin.tmp");
    let failed = |what: &str, e: std::io::Error| {
        anyhow::anyhow!("checkpoint {what} of {} failed: {e}", tmp_path.display())
    };
    let mut tmp = std::fs::File::create(&tmp_path).map_err(|e| failed("create", e))?;
    tmp.write_all(bytes).map_err(|e| failed("write", e))?;
    tmp.sync_all().map_err(|e| failed("fsync", e))?;
    drop(tmp);

    std::fs::rename(&tmp_path, path)
        .map_err(|e| anyhow::anyhow!("failed to rename checkpoint file {}: {e}", path.display()))?;

    // The checkpoint path is always absolute, so it has a directory.
    let dir = path.parent().unwrap_or(path);
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| anyhow::anyhow!("fsync of checkpoint dir {} failed: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::sample_persisted_session;
    use crate::persist::STATE_VERSION;

    fn empty_state() -> PersistedDaemonState {
        PersistedDaemonState {
            version: STATE_VERSION,
            session_index_counter: 0,
            used_words: vec![],
            sessions: vec![],
        }
    }

    fn one_session_state() -> PersistedDaemonState {
        PersistedDaemonState {
            version: STATE_VERSION,
            session_index_counter: 1,
            used_words: vec!["eagle".to_string()],
            sessions: vec![sample_persisted_session("eagle", "test", 0)],
        }
    }

    fn read_back(path: &Path) -> PersistedDaemonState {
        crate::persist::restore::read_checkpoint(path).expect("a readable checkpoint")
    }

    #[test]
    fn write_and_read_roundtrip() {
        let state = one_session_state();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");

        let written = Checkpointer::new(path.clone()).write(&state).unwrap();
        assert_eq!(written, Written::Wrote);

        let decoded = read_back(&path);
        assert_eq!(decoded.version, STATE_VERSION);
        assert_eq!(decoded.used_words, vec!["eagle"]);
        assert_eq!(decoded.sessions[0].meta.word_id, "eagle");
    }

    #[test]
    fn atomic_write_tmp_gone_after_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");

        assert!(!path.exists());
        Checkpointer::new(path.clone())
            .write(&empty_state())
            .unwrap();

        // state.bin exists, .tmp is gone.
        assert!(path.exists());
        assert!(!tmp.path().join("state.bin.tmp").exists());
    }

    #[test]
    fn a_changed_state_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");
        let checkpointer = Checkpointer::new(path.clone());
        assert_eq!(checkpointer.write(&empty_state()).unwrap(), Written::Wrote);

        let mut changed = one_session_state();
        changed.session_index_counter = 99;
        assert_eq!(checkpointer.write(&changed).unwrap(), Written::Wrote);
        assert_eq!(read_back(&path).session_index_counter, 99);
    }

    /// An identical state is not written again: the file is left as it was,
    /// down to its inode (a rewrite renames a new file into place).
    #[test]
    fn an_unchanged_state_is_not_written_again() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");
        let checkpointer = Checkpointer::new(path.clone());
        checkpointer.write(&one_session_state()).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();

        assert_eq!(
            checkpointer.write(&one_session_state()).unwrap(),
            Written::Unchanged
        );
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    }

    /// A failed write records nothing: once the disk is writable again the
    /// same state is written, not skipped as unchanged.
    #[test]
    fn a_failed_write_is_retried_with_the_same_state() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("not-yet");
        let path = dir.join("state.bin");
        let checkpointer = Checkpointer::new(path.clone());

        assert!(checkpointer.write(&one_session_state()).is_err());
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(
            checkpointer.write(&one_session_state()).unwrap(),
            Written::Wrote
        );
        assert_eq!(read_back(&path).used_words, vec!["eagle"]);
    }

    /// A final write goes to disk even when nothing changed, and seals the
    /// checkpoint: a later periodic write is refused.
    #[test]
    fn a_final_write_always_writes_and_seals() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");
        let checkpointer = Checkpointer::new(path.clone());
        checkpointer.write(&one_session_state()).unwrap();
        std::fs::remove_file(&path).unwrap();

        checkpointer.write_final(&one_session_state()).unwrap();
        assert_eq!(read_back(&path).used_words, vec!["eagle"]);

        assert_eq!(checkpointer.write(&empty_state()).unwrap(), Written::Sealed);
        assert_eq!(read_back(&path).used_words, vec!["eagle"], "sealed");
    }

    /// A final write that fails leaves the checkpoint unsealed.
    #[test]
    fn a_failed_final_write_does_not_seal() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("not-yet");
        let checkpointer = Checkpointer::new(dir.join("state.bin"));

        assert!(checkpointer.write_final(&empty_state()).is_err());
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(checkpointer.write(&empty_state()).unwrap(), Written::Wrote);
    }

    /// The background variants write through the same writer: the second,
    /// identical state is skipped, and the final one seals.
    #[tokio::test]
    async fn background_writes_share_the_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");
        let checkpointer = Arc::new(Checkpointer::new(path.clone()));

        let first = checkpointer.write_in_background(empty_state()).await;
        let second = checkpointer.write_in_background(empty_state()).await;
        assert_eq!(first.unwrap(), Written::Wrote);
        assert_eq!(second.unwrap(), Written::Unchanged);

        checkpointer
            .write_final_in_background(one_session_state())
            .await
            .unwrap();
        assert_eq!(read_back(&path).used_words, vec!["eagle"]);
        let after = checkpointer.write_in_background(empty_state()).await;
        assert_eq!(after.unwrap(), Written::Sealed);
    }
}
