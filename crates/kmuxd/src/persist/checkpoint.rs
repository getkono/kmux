//! Checkpoint writing: serialize daemon state to disk durably, only when it
//! changed, one writer at a time, and off the async runtime (issue #207).
//!
//! A daemon that runs for months rewrites its checkpoint every 30 s. Three
//! things matter over that span:
//!
//! - **Durability.** The state is written to a `.tmp` sibling, `fsync`ed,
//!   renamed into place, and the directory is `fsync`ed. Without the two
//!   `fsync`s a power loss can keep the rename and lose the data, leaving an
//!   empty or torn checkpoint where the last good one was. Some filesystems
//!   refuse to `fsync` a directory; once the rename has happened the new state
//!   is in place, so that refusal is logged once and the write still counts.
//! - **No rewrite of what is already there.** An idle daemon's state does not
//!   change, and [`PeriodicCheckpointer::write`] skips a write whose bytes
//!   match the last one that succeeded.
//! - **One writer.** The periodic loop, the shutdown path and a graceful
//!   handoff all write the same file through one [`Checkpointer`], whose lock
//!   serialises them. The final write of a shutdown or handoff *consumes* the
//!   [`Checkpointer`], so its owner cannot write again (a compile error), and
//!   *seals* the file, so a [`PeriodicCheckpointer`] tick that lands
//!   afterwards cannot replace the state the next daemon reads.

use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use tracing::warn;

use super::PersistedDaemonState;

/// What a periodic checkpoint write did.
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

/// `fsync` a directory, so a rename inside it is on disk.
type SyncDir = fn(&Path) -> std::io::Result<()>;

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[derive(Debug)]
struct WriterState {
    /// The last state written successfully.
    last: Option<Fingerprint>,
    /// Set by a final write: nothing may replace it.
    sealed: bool,
    /// How the directory is `fsync`ed after the rename.
    sync_dir: SyncDir,
    /// Whether a refused directory `fsync` has been logged already.
    dir_sync_warned: bool,
}

impl WriterState {
    fn new(sync_dir: SyncDir) -> Self {
        Self {
            last: None,
            sealed: false,
            sync_dir,
            dir_sync_warned: false,
        }
    }

    /// [`PeriodicCheckpointer::write`], under the writer's lock.
    fn write(&mut self, path: &Path, state: &PersistedDaemonState) -> anyhow::Result<Written> {
        if self.sealed {
            return Ok(Written::Sealed);
        }
        let bytes = serialize(state)?;
        let print = fingerprint(&bytes);
        if self.last == Some(print) {
            return Ok(Written::Unchanged);
        }
        self.write_durable(path, &bytes)?;
        self.last = Some(print);
        Ok(Written::Wrote)
    }

    /// [`Checkpointer::write_final_in_background`], under the writer's lock.
    fn write_final(&mut self, path: &Path, state: &PersistedDaemonState) -> anyhow::Result<()> {
        let bytes = serialize(state)?;
        self.write_durable(path, &bytes)?;
        self.last = Some(fingerprint(&bytes));
        self.sealed = true;
        Ok(())
    }

    /// [`write_durable`], logging a refused directory `fsync` the first time
    /// only: it is a property of the filesystem, so it would recur every tick.
    fn write_durable(&mut self, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        if let Err(e) = write_durable(path, bytes, self.sync_dir)?
            && self.first_dir_sync_failure()
        {
            warn!(
                "checkpoint written, but the directory of {} could not be fsynced \
                 ({e}); a power loss may undo the latest checkpoint. Not reported again.",
                path.display()
            );
        }
        Ok(())
    }

    /// Record a refused directory `fsync`; true only the first time.
    fn first_dir_sync_failure(&mut self) -> bool {
        !std::mem::replace(&mut self.dir_sync_warned, true)
    }
}

/// The file and lock every handle on one checkpoint shares.
#[derive(Debug)]
struct Shared {
    path: PathBuf,
    state: Mutex<WriterState>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, WriterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The owner of the daemon's checkpoint file: the only handle that can make
/// the final write, which consumes it. Not `Clone`. See the module docs.
#[derive(Debug)]
pub struct Checkpointer {
    shared: Arc<Shared>,
}

/// A handle for the periodic writes, from [`Checkpointer::periodic`]. Its
/// writes are refused once the owner has made the final write.
#[derive(Debug, Clone)]
pub struct PeriodicCheckpointer {
    shared: Arc<Shared>,
}

/// A final write that failed: the error, and the [`Checkpointer`] back,
/// unsealed, so its owner can carry on or try again.
#[derive(Debug)]
pub struct FinalWriteFailed {
    pub checkpointer: Checkpointer,
    pub error: anyhow::Error,
}

impl Checkpointer {
    /// A writer for the checkpoint at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self::with_dir_sync(path, sync_dir)
    }

    /// [`Self::new`] with the directory `fsync` given, so a test can refuse it.
    fn with_dir_sync(path: PathBuf, sync_dir: SyncDir) -> Self {
        Self {
            shared: Arc::new(Shared {
                path,
                state: Mutex::new(WriterState::new(sync_dir)),
            }),
        }
    }

    /// A handle for the periodic writes, sharing this checkpoint's lock.
    pub fn periodic(&self) -> PeriodicCheckpointer {
        PeriodicCheckpointer {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Write `state` whether or not it changed, then seal the checkpoint so
    /// no later [`PeriodicCheckpointer::write`] replaces it. For the last
    /// write of a daemon that is shutting down or handing off. Consumes the
    /// checkpointer, so there is no later final write. Runs on the blocking
    /// pool, so the `fsync`s never stall a runtime thread.
    ///
    /// # Errors
    ///
    /// When `state` cannot be serialized or written, or the blocking task
    /// panicked; the checkpoint is then left unsealed and the checkpointer
    /// handed back.
    pub async fn write_final_in_background(
        self,
        state: PersistedDaemonState,
    ) -> Result<(), FinalWriteFailed> {
        let shared = Arc::clone(&self.shared);
        let result =
            tokio::task::spawn_blocking(move || shared.lock().write_final(&shared.path, &state))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|written| written);
        result.map_err(|error| FinalWriteFailed {
            checkpointer: self,
            error,
        })
    }

    /// Make the final write with the checkpointer in `slot`, leaving the slot
    /// empty once it succeeded and the checkpointer back in it when it failed.
    ///
    /// # Errors
    ///
    /// When `slot` is empty (the final write was already made), or as
    /// [`Self::write_final_in_background`].
    pub async fn write_final_from(
        slot: &mut Option<Self>,
        state: PersistedDaemonState,
    ) -> anyhow::Result<()> {
        let checkpointer = slot
            .take()
            .ok_or_else(|| anyhow::anyhow!("the final checkpoint was already written"))?;
        checkpointer
            .write_final_in_background(state)
            .await
            .map_err(|failed| {
                *slot = Some(failed.checkpointer);
                failed.error
            })
    }
}

impl PeriodicCheckpointer {
    /// Write `state` unless it matches the last successful write or a final
    /// checkpoint has been written. Blocking: see
    /// [`Self::write_in_background`].
    ///
    /// # Errors
    ///
    /// When `state` cannot be serialized or written. Nothing is recorded, so
    /// the next write tries again.
    pub fn write(&self, state: &PersistedDaemonState) -> anyhow::Result<Written> {
        self.shared.lock().write(&self.shared.path, state)
    }

    /// [`Self::write`] on the blocking pool, so the `fsync`s never stall a
    /// runtime thread.
    ///
    /// # Errors
    ///
    /// As [`Self::write`], or when the blocking task panicked.
    pub async fn write_in_background(
        &self,
        state: PersistedDaemonState,
    ) -> anyhow::Result<Written> {
        let writer = self.clone();
        tokio::task::spawn_blocking(move || writer.write(&state)).await?
    }
}

fn serialize(state: &PersistedDaemonState) -> anyhow::Result<Vec<u8>> {
    postcard::to_allocvec(state)
        .map_err(|e| anyhow::anyhow!("checkpoint serialization failed: {e}"))
}

/// Replace the file at `path` with `bytes` so that after a crash or power loss
/// it holds either the old contents or the new ones, never a mix or nothing:
/// write a `.tmp` sibling and `fsync` it, rename it into place, then `fsync`
/// the directory with `sync_dir` so the rename itself is on disk.
///
/// # Errors
///
/// The outer error: the new contents are not in place. The inner one: they
/// are, but the directory `fsync` was refused.
fn write_durable(
    path: &Path,
    bytes: &[u8],
    sync_dir: SyncDir,
) -> anyhow::Result<std::io::Result<()>> {
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
    Ok(sync_dir(path.parent().unwrap_or(path)))
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

    fn refuse_dir_sync(_: &Path) -> std::io::Result<()> {
        Err(std::io::Error::other("directory fsync not supported"))
    }

    #[test]
    fn write_and_read_roundtrip() {
        let state = one_session_state();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");

        let written = Checkpointer::new(path.clone())
            .periodic()
            .write(&state)
            .unwrap();
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
            .periodic()
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
        let checkpointer = Checkpointer::new(path.clone()).periodic();
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
        let checkpointer = Checkpointer::new(path.clone()).periodic();
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
        let checkpointer = Checkpointer::new(path.clone()).periodic();

        assert!(checkpointer.write(&one_session_state()).is_err());
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(
            checkpointer.write(&one_session_state()).unwrap(),
            Written::Wrote
        );
        assert_eq!(read_back(&path).used_words, vec!["eagle"]);
    }

    /// A refused directory `fsync` after the rename does not fail the write:
    /// the state is in place and recorded, so the same state is not written
    /// again on the next tick.
    #[tokio::test]
    async fn a_refused_directory_fsync_still_counts_as_written() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");
        let owner = Checkpointer::with_dir_sync(path.clone(), refuse_dir_sync);
        let checkpointer = owner.periodic();

        assert_eq!(
            checkpointer.write(&one_session_state()).unwrap(),
            Written::Wrote
        );
        assert_eq!(read_back(&path).used_words, vec!["eagle"]);
        assert_eq!(
            checkpointer.write(&one_session_state()).unwrap(),
            Written::Unchanged
        );
        owner
            .write_final_in_background(empty_state())
            .await
            .unwrap();
        assert!(read_back(&path).used_words.is_empty());
    }

    /// The real directory `fsync` succeeds on a directory and reports a
    /// failure, rather than swallowing it, for one that is not there.
    #[test]
    fn sync_dir_reports_a_missing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        sync_dir(tmp.path()).unwrap();
        assert!(sync_dir(&tmp.path().join("missing")).is_err());
    }

    /// A refused directory `fsync` is reported the first time only.
    #[test]
    fn a_refused_directory_fsync_is_reported_once() {
        let mut state = WriterState::new(refuse_dir_sync);
        assert!(state.first_dir_sync_failure());
        assert!(!state.first_dir_sync_failure());
        assert!(!state.first_dir_sync_failure());
    }

    /// A final write goes to disk even when nothing changed, and seals the
    /// checkpoint: a later periodic write is refused.
    #[tokio::test]
    async fn a_final_write_always_writes_and_seals() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");
        let owner = Checkpointer::new(path.clone());
        let checkpointer = owner.periodic();
        checkpointer.write(&one_session_state()).unwrap();
        std::fs::remove_file(&path).unwrap();

        owner
            .write_final_in_background(one_session_state())
            .await
            .unwrap();
        assert_eq!(read_back(&path).used_words, vec!["eagle"]);

        assert_eq!(checkpointer.write(&empty_state()).unwrap(), Written::Sealed);
        assert_eq!(read_back(&path).used_words, vec!["eagle"], "sealed");
    }

    /// A final write that fails hands the checkpointer back unsealed: the
    /// periodic writes carry on, and the final write can be made again.
    #[tokio::test]
    async fn a_failed_final_write_does_not_seal() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("not-yet");
        let path = dir.join("state.bin");
        let owner = Checkpointer::new(path.clone());
        let checkpointer = owner.periodic();

        let failed = owner
            .write_final_in_background(empty_state())
            .await
            .unwrap_err();
        assert!(
            failed.error.to_string().contains("create"),
            "{}",
            failed.error
        );
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(checkpointer.write(&empty_state()).unwrap(), Written::Wrote);

        failed
            .checkpointer
            .write_final_in_background(one_session_state())
            .await
            .unwrap();
        assert_eq!(read_back(&path).used_words, vec!["eagle"]);
    }

    /// `write_final_from` empties the slot on success, refuses an empty
    /// slot, and puts the checkpointer back when the write fails.
    #[tokio::test]
    async fn the_final_write_from_a_slot_takes_it_only_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("not-yet");
        let path = dir.join("state.bin");
        let mut slot = Some(Checkpointer::new(path.clone()));

        let failed = Checkpointer::write_final_from(&mut slot, empty_state()).await;
        assert!(failed.is_err());
        assert!(slot.is_some(), "handed back after a failed write");

        std::fs::create_dir(&dir).unwrap();
        Checkpointer::write_final_from(&mut slot, one_session_state())
            .await
            .unwrap();
        assert!(slot.is_none(), "consumed by the final write");
        assert_eq!(read_back(&path).used_words, vec!["eagle"]);

        let again = Checkpointer::write_final_from(&mut slot, empty_state()).await;
        assert!(again.unwrap_err().to_string().contains("already written"));
        assert_eq!(read_back(&path).used_words, vec!["eagle"]);
    }

    /// The background variants write through the same writer: the second,
    /// identical state is skipped, and the final one seals.
    #[tokio::test]
    async fn background_writes_share_the_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.bin");
        let owner = Checkpointer::new(path.clone());
        let checkpointer = owner.periodic();

        let first = checkpointer.write_in_background(empty_state()).await;
        let second = checkpointer.write_in_background(empty_state()).await;
        assert_eq!(first.unwrap(), Written::Wrote);
        assert_eq!(second.unwrap(), Written::Unchanged);

        owner
            .write_final_in_background(one_session_state())
            .await
            .unwrap();
        assert_eq!(read_back(&path).used_words, vec!["eagle"]);
        let after = checkpointer.write_in_background(empty_state()).await;
        assert_eq!(after.unwrap(), Written::Sealed);
    }
}
