//! Outgoing-daemon (O) side of a graceful handoff: spawn the successor, stream
//! each live PTY master fd to it, write the final checkpoint, commit, and exit.
//!
//! `run` returns `Ok(())` once the handoff has *committed* (the successor holds
//! every live fd, or declined and will snapshot-restore from our final
//! checkpoint). The caller then tears down its listeners and exits. On `Err`
//! the handoff was rolled back before its commit point: the PTY readers run
//! again, the checkpoint accepts writes again, the successor was told to stand
//! down, and the caller simply resumes serving.
//!
//! Everything fallible happens before the commit point (issue #207): each frame
//! is bounded by [`STEP_TIMEOUT`], and the final checkpoint is written and
//! `fsync`ed before `Complete` is sent. The commit point is receiving the
//! successor's `Ack`; what follows it (keep-alive, stopping the readers,
//! `Released`) cannot fail in a way that undoes the handoff.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use kmux_protocol::control_rpc::{
    DAEMON_BOOT_ARGS, HANDOFF_PROTOCOL_VERSION, HandoffMessage, HandoffPaneMeta,
};
use tokio::net::{UnixListener, UnixStream};
use tracing::{info, warn};

use crate::app::ServerApp;
use crate::persist::checkpoint::{Checkpointer, SealedCheckpoint};

use super::{PathGuard, STEP_TIMEOUT, read_frame_within, write_frame_within};

/// Maximum time to wait for the successor daemon to connect to the handoff
/// socket. It must start, daemonize, restore-read, and connect within this.
const SUCCESSOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Longest the PTY readers may take to park for the final checkpoint.
const HOLD_TIMEOUT: Duration = Duration::from_secs(5);

/// Drive a graceful handoff to a freshly-spawned successor daemon.
///
/// On `Ok(())` the handoff committed and `app` must not serve further (the
/// caller releases sockets and exits). On `Err(_)` the handoff was rolled back
/// and the daemon should resume normal operation. `committed` is set the
/// moment the successor's `Ack` arrives, so a caller that abandons this future
/// (a SIGTERM mid-handoff) can still tell whether the successor owns the
/// sessions.
///
/// The final checkpoint consumes `checkpointer` (left `None`) once the
/// handoff commits or is declined; a rollback puts it back, unsealed.
pub async fn run(
    app: &Arc<ServerApp>,
    checkpointer: &mut Option<Checkpointer>,
    committed: &AtomicBool,
) -> anyhow::Result<()> {
    // Without a checkpoint the successor has nothing to rebuild the panes
    // from; refuse before anything has started.
    if checkpointer.is_none() {
        bail!("no checkpoint path; cannot hand off");
    }
    let path = kmux_sys::dirs::handoff_socket_path()?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding handoff socket {}", path.display()))?;
    let _guard = PathGuard(path.clone());

    spawn_successor().context("spawning successor daemon")?;
    info!(
        "handoff: spawned successor; awaiting its connection within {SUCCESSOR_CONNECT_TIMEOUT:?}"
    );

    let (stream, _) = match tokio::time::timeout(SUCCESSOR_CONNECT_TIMEOUT, listener.accept()).await
    {
        Err(_) => {
            // The successor never connected — almost always a boot failure
            // (full disk, panic during restore). Its output is in the boot log.
            // Nothing destructive has happened yet, so the caller rolls back and
            // keeps serving; surface why so the operator can act.
            warn!(
                "handoff: successor did not connect within {SUCCESSOR_CONNECT_TIMEOUT:?} \
                 (check kmuxd-boot.log); rolling back and continuing to serve"
            );
            return Err(anyhow!(
                "successor did not connect within {SUCCESSOR_CONNECT_TIMEOUT:?}"
            ));
        }
        Ok(accepted) => accepted.context("accepting successor handoff connection")?,
    };

    drive(app, &stream, checkpointer, committed, STEP_TIMEOUT).await
}

/// How a handoff that reached its end ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The successor acknowledged every fd: it takes over the live PTYs.
    Committed,
    /// The successor declined; it restores from our final checkpoint.
    Declined,
}

/// Run the handoff protocol over `stream`, connected to the successor, with
/// every frame bounded by `step`. Rolls back on any failure before the commit
/// point (see the module docs).
async fn drive(
    app: &ServerApp,
    stream: &UnixStream,
    checkpointer: &mut Option<Checkpointer>,
    committed: &AtomicBool,
    step: Duration,
) -> anyhow::Result<()> {
    let mut final_checkpoint = FinalCheckpoint {
        slot: checkpointer,
        sealed: None,
    };
    match transfer(app, stream, &mut final_checkpoint, committed, step).await {
        Ok(ending) => {
            final_checkpoint.keep_sealed();
            finish(app, stream, ending, step).await;
            Ok(())
        }
        Err(e) => {
            drop(final_checkpoint);
            roll_back(app, stream, &e, step).await;
            Err(e)
        }
    }
}

/// The final checkpoint of a handoff that has not ended yet. A handoff that
/// rolls back after writing it — or whose sender panics — must leave the
/// checkpoint accepting writes again, so dropping this unseals it and puts the
/// [`Checkpointer`] back in its slot. Only [`Self::keep_sealed`], on commit or
/// decline, keeps it sealed.
struct FinalCheckpoint<'a> {
    slot: &'a mut Option<Checkpointer>,
    sealed: Option<SealedCheckpoint>,
}

impl FinalCheckpoint<'_> {
    /// Write the final checkpoint the successor restores from, durably and
    /// off the runtime, sealing it against later periodic writes.
    async fn write(&mut self, app: &ServerApp) -> anyhow::Result<()> {
        let state = app.checkpoint_state().await;
        let sealed = Checkpointer::write_final_from(self.slot, state)
            .await
            .context("writing the final checkpoint")?;
        self.sealed = Some(sealed);
        Ok(())
    }

    /// The handoff ended: the checkpoint stays sealed for good.
    fn keep_sealed(mut self) {
        self.sealed = None;
    }
}

impl Drop for FinalCheckpoint<'_> {
    fn drop(&mut self) {
        if let Some(sealed) = self.sealed.take() {
            *self.slot = Some(sealed.unseal());
        }
    }
}

/// Everything up to and including the commit point: advertise the panes,
/// stream the live fds, stop the readers, write the final checkpoint, and
/// exchange `Complete` for `Ack`. Any error leaves the handoff uncommitted.
async fn transfer(
    app: &ServerApp,
    stream: &UnixStream,
    final_checkpoint: &mut FinalCheckpoint<'_>,
    committed: &AtomicBool,
    step: Duration,
) -> anyhow::Result<Ending> {
    let panes = app.collect_handoff_panes().await;
    let live = panes.iter().filter(|p| p.has_live_fd).count();
    info!(
        total = panes.len(),
        live, "handoff: advertising panes to successor"
    );

    let hello = HandoffMessage::Hello {
        version: HANDOFF_PROTOCOL_VERSION,
        token: app.auth_token.clone(),
        panes: panes.clone(),
    };
    write_frame_within(stream, &hello, None, step).await?;

    match read_frame_within(stream, step).await?.0 {
        HandoffMessage::Accept => {}
        HandoffMessage::Decline { reason } => {
            warn!(
                "handoff: successor declined live migration ({reason}); it will snapshot-restore"
            );
            // The successor will respawn from the checkpoint, so make sure a
            // fresh one is on disk. Children are NOT kept alive — this degrades
            // to today's restart behavior.
            final_checkpoint.write(app).await?;
            return Ok(Ending::Declined);
        }
        other => bail!("handoff: expected Accept/Decline, got {other:?}"),
    }

    stream_fds(app, stream, &panes, step).await?;

    // Park every PTY reader, then snapshot: the successor seeds from exactly
    // what we consumed, and anything after sits in the kernel buffer for it.
    // Both happen before `Complete`, so a failure still rolls back.
    if !app.hold_relays(HOLD_TIMEOUT).await {
        bail!("handoff: PTY readers did not park within {HOLD_TIMEOUT:?}");
    }
    final_checkpoint.write(app).await?;

    write_frame_within(stream, &HandoffMessage::Complete, None, step).await?;
    match read_frame_within(stream, step).await?.0 {
        HandoffMessage::Ack => {
            // Commit point: the successor holds every live fd and the
            // checkpoint it restores from is on disk.
            committed.store(true, Ordering::SeqCst);
            Ok(Ending::Committed)
        }
        other => bail!("handoff: expected Ack, got {other:?}"),
    }
}

/// Stream each live pane's master fd, lock-step (one in flight at a time), so
/// each frame is delivered to the successor with exactly its own fd.
async fn stream_fds(
    app: &ServerApp,
    stream: &UnixStream,
    panes: &[HandoffPaneMeta],
    step: Duration,
) -> anyhow::Result<()> {
    for meta in panes.iter().filter(|p| p.has_live_fd) {
        let fd = app
            .manager
            .dup_master_fd(&meta.pane_id)
            .await
            .with_context(|| format!("duplicating master fd for {}", meta.pane_id))?;
        let pane_fd = HandoffMessage::PaneFd {
            pane_id: meta.pane_id.clone(),
        };
        write_frame_within(stream, &pane_fd, Some(fd.as_raw_fd()), step).await?;
        // Our dup has been copied into the successor; close ours. The child stays
        // alive: the successor holds a dup, and our original master fds are still
        // open until we exit.
        drop(fd);
        match read_frame_within(stream, step).await?.0 {
            HandoffMessage::PaneFdAck => {}
            other => bail!(
                "handoff: expected PaneFdAck for {}, got {other:?}",
                meta.pane_id
            ),
        }
    }
    Ok(())
}

/// After the commit point. Nothing here can undo the handoff: a failure is
/// logged and the daemon exits all the same.
async fn finish(app: &ServerApp, stream: &UnixStream, ending: Ending, step: Duration) {
    match ending {
        Ending::Committed => {
            // Suppress SIGKILL on our PTY children and stop the parked readers.
            app.manager.set_all_keep_alive(true).await;
            app.quiesce_relays().await;
        }
        // The successor respawns from the checkpoint; our children end with us.
        Ending::Declined => {}
    }
    // Tell the successor it may bind the control/data sockets; then we exit.
    if let Err(e) = write_frame_within(stream, &HandoffMessage::Released, None, step).await {
        warn!("handoff: could not send Released ({e}); the successor proceeds once we exit");
    }
    info!(?ending, "handoff: committed; releasing sockets and exiting");
}

/// Undo everything [`transfer`] did before failing, and tell the successor to
/// stand down: the readers run again. (The checkpoint was unsealed as the
/// [`FinalCheckpoint`] dropped.)
async fn roll_back(app: &ServerApp, stream: &UnixStream, cause: &anyhow::Error, step: Duration) {
    warn!("handoff: rolling back before the commit point: {cause:#}");
    app.release_relays().await;
    let abort = HandoffMessage::Abort {
        reason: format!("{cause:#}"),
    };
    // Best effort: a successor that cannot be told sees the socket close and
    // stands down on finding us alive.
    let _ = write_frame_within(stream, &abort, None, step).await;
}

/// Spawn the successor daemon: re-exec this binary with the standard boot args
/// plus `--handoff`. The binary path is resolved by [`resolve_successor_exe`] so a
/// live upgrade (the new binary swapped in over our path) re-execs the *new* code.
fn spawn_successor() -> anyhow::Result<()> {
    let exe = resolve_successor_exe(
        std::env::current_exe().context("resolving current executable")?,
        std::path::Path::exists,
    )?;
    let mut args: Vec<&str> = DAEMON_BOOT_ARGS.to_vec();
    args.push("--handoff");
    // Capture the successor's pre-daemonize stdout+stderr in the boot log so a
    // boot failure (full disk, panic during restore) is visible to `kmux daemon
    // restart` instead of silently timing out the handoff.
    let (out, err) = crate::boot_log_stdio();
    std::process::Command::new(&exe)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;
    Ok(())
}

/// Resolve the path to re-exec for the successor daemon, accounting for an
/// in-place upgrade (`mise run upgrade-daemon`: `cargo install` atomically replaces the
/// binary, then `kmux daemon restart` triggers this handoff).
///
/// `current_exe()` behaves differently across platforms once the running binary is
/// replaced on disk:
///   - **macOS** keeps the original path, which now resolves to the freshly
///     installed inode — re-execing it runs the new binary, so we use it as-is.
///   - **Linux** unlinks the running inode (the atomic rename), so `/proc/self/exe`
///     reads back as `"<path> (deleted)"`. Re-execing that literal path would
///     `ENOENT` and the handoff would roll back onto the *old* in-memory code — the
///     upgrade would silently no-op. We strip the marker and prefer the de-suffixed
///     path when the replacement now exists there.
///
/// Returns an error when neither candidate exists on disk, so `spawn_successor`
/// fails before the commit point and the daemon keeps serving (no session loss)
/// rather than spawning nothing.
fn resolve_successor_exe(
    exe: std::path::PathBuf,
    exists: impl Fn(&std::path::Path) -> bool,
) -> anyhow::Result<std::path::PathBuf> {
    // Linux marks the unlinked original as "<path> (deleted)"; prefer the
    // replacement sitting at the same (un-suffixed) path when it is present.
    if let Some(stripped) = exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        let candidate = std::path::PathBuf::from(stripped);
        if exists(&candidate) {
            return Ok(candidate);
        }
    }
    if exists(&exe) {
        return Ok(exe);
    }
    bail!(
        "cannot locate the daemon binary to re-exec ({}); was it removed mid-upgrade?",
        exe.display()
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use kmux_protocol::control_rpc::HandoffMessage;
    use kmux_protocol::messages::{ClientCapabilities, TermSize};
    use tokio::net::UnixStream;

    use super::{drive, resolve_successor_exe};
    use crate::app::ServerApp;
    use crate::handoff::{read_frame, write_frame};
    use crate::persist::checkpoint::{Checkpointer, Written};

    /// The bound on each handoff step in these tests.
    const STEP: Duration = Duration::from_secs(30);

    /// What a scripted successor does once it has read `Hello`.
    #[derive(Clone, Copy)]
    enum Successor {
        /// Accept, acknowledge every fd and `Complete`, like a real one.
        Cooperates,
        /// Accept, then answer nothing more.
        StallsAfterAccept,
        /// Accept and acknowledge every fd, but never `Complete`.
        StallsAtComplete,
        /// Decline the live transfer.
        Declines,
    }

    /// Play `script` against the predecessor at the other end of `stream`,
    /// and return every frame it sent after `Hello`, until it closed the
    /// socket, with whether each carried an fd.
    async fn successor(stream: UnixStream, script: Successor) -> Vec<(HandoffMessage, bool)> {
        let (hello, _) = read_frame(&stream).await.expect("Hello");
        assert!(matches!(hello, HandoffMessage::Hello { .. }), "{hello:?}");
        let answer = match script {
            Successor::Declines => HandoffMessage::Decline {
                reason: "test".into(),
            },
            Successor::Cooperates | Successor::StallsAfterAccept | Successor::StallsAtComplete => {
                HandoffMessage::Accept
            }
        };
        write_frame(&stream, &answer, None)
            .await
            .expect("answer Hello");

        let mut seen = Vec::new();
        while let Ok((msg, fd)) = read_frame(&stream).await {
            let reply = match (&msg, script) {
                (
                    HandoffMessage::PaneFd { .. },
                    Successor::Cooperates | Successor::StallsAtComplete,
                ) => Some(HandoffMessage::PaneFdAck),
                (HandoffMessage::Complete, Successor::Cooperates) => Some(HandoffMessage::Ack),
                _ => None,
            };
            seen.push((msg, fd.is_some()));
            if let Some(reply) = reply {
                write_frame(&stream, &reply, None).await.expect("reply");
            }
        }
        seen
    }

    /// A predecessor with one live pane (`cat`), and its checkpoint writer.
    async fn fixture_predecessor(checkpoint_dir: &Path) -> (ServerApp, Option<Checkpointer>) {
        let app = crate::fixtures::fixture_app();
        let size = TermSize {
            rows: 4,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        };
        app.create_session(
            None,
            None,
            Some("/bin/cat".into()),
            vec![],
            size,
            &ClientCapabilities::default(),
        )
        .await
        .expect("a session");
        let checkpointer = Checkpointer::new(checkpoint_dir.join("state.bin"));
        (app, Some(checkpointer))
    }

    fn names(frames: &[(HandoffMessage, bool)]) -> Vec<String> {
        frames
            .iter()
            .map(|(msg, _)| {
                format!("{msg:?}")
                    .split([' ', '{'])
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// A cooperating successor gets the live fd, then `Complete` once the
    /// final checkpoint is on disk, and `Released` after its `Ack`: the
    /// handoff commits, and the sealed checkpoint cannot be overwritten.
    #[tokio::test]
    async fn a_cooperating_successor_commits_the_handoff() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, mut checkpointer) = fixture_predecessor(tmp.path()).await;
        let periodic = checkpointer.as_ref().unwrap().periodic();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let successor = tokio::spawn(successor(theirs, Successor::Cooperates));
        let committed = AtomicBool::new(false);

        drive(&app, &ours, &mut checkpointer, &committed, STEP)
            .await
            .expect("committed");
        assert!(checkpointer.is_none(), "the final write consumed it");
        drop(ours);
        let seen = successor.await.unwrap();

        assert!(committed.load(Ordering::SeqCst));
        assert_eq!(names(&seen), ["PaneFd", "Complete", "Released"]);
        assert!(seen[0].1, "the PaneFd carries the master");
        let state = crate::persist::restore::read_checkpoint(&tmp.path().join("state.bin"))
            .expect("the final checkpoint is on disk");
        assert_eq!(state.sessions.len(), 1);
        let later = periodic.write(&state).unwrap();
        assert_eq!(later, Written::Sealed);
    }

    /// A successor that stops answering after `Accept` makes the handoff
    /// time out instead of wedging this daemon, which rolls back: it tells
    /// the successor to stand down and accepts checkpoints again (issue #207).
    #[tokio::test(start_paused = true)]
    async fn a_successor_that_stalls_after_accept_times_out_and_rolls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, mut checkpointer) = fixture_predecessor(tmp.path()).await;
        let periodic = checkpointer.as_ref().unwrap().periodic();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let successor = tokio::spawn(successor(theirs, Successor::StallsAfterAccept));
        let committed = AtomicBool::new(false);
        let started = tokio::time::Instant::now();

        let err = drive(&app, &ours, &mut checkpointer, &committed, STEP)
            .await
            .expect_err("times out");
        drop(ours);
        let seen = successor.await.unwrap();

        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert!(started.elapsed() >= STEP);
        assert!(!committed.load(Ordering::SeqCst));
        assert_eq!(names(&seen), ["PaneFd", "Abort"]);
        let state = app.checkpoint_state().await;
        assert_ne!(periodic.write(&state).unwrap(), Written::Sealed);
        assert!(checkpointer.is_some());
    }

    /// A successor that never acknowledges `Complete` makes the handoff roll
    /// back after its final checkpoint was written: the checkpointer is handed
    /// back unsealed, so periodic writes resume and a later shutdown can make
    /// its own final write.
    #[tokio::test(start_paused = true)]
    async fn a_rollback_after_the_final_checkpoint_unseals_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, mut checkpointer) = fixture_predecessor(tmp.path()).await;
        let periodic = checkpointer.as_ref().unwrap().periodic();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let successor = tokio::spawn(successor(theirs, Successor::StallsAtComplete));
        let committed = AtomicBool::new(false);

        let err = drive(&app, &ours, &mut checkpointer, &committed, STEP)
            .await
            .expect_err("times out waiting for Ack");
        drop(ours);
        let seen = successor.await.unwrap();

        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert_eq!(names(&seen), ["PaneFd", "Complete", "Abort"]);
        let returned = checkpointer.take().expect("handed back");
        let mut state = app.checkpoint_state().await;
        state.session_index_counter += 1;
        assert_eq!(periodic.write(&state).unwrap(), Written::Wrote);
        returned
            .write_final_in_background(state)
            .await
            .expect("a later final write");
    }

    /// A final checkpoint that cannot be written fails the handoff before
    /// `Complete` is sent: nothing fallible is left after the commit point,
    /// and the successor is told to stand down.
    #[tokio::test]
    async fn a_failed_final_checkpoint_fails_before_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _) = fixture_predecessor(tmp.path()).await;
        let mut unwritable = Some(Checkpointer::new(tmp.path().join("missing/state.bin")));
        let (ours, theirs) = UnixStream::pair().unwrap();
        let successor = tokio::spawn(successor(theirs, Successor::Cooperates));
        let committed = AtomicBool::new(false);

        let err = drive(&app, &ours, &mut unwritable, &committed, STEP)
            .await
            .expect_err("the checkpoint fails");
        drop(ours);
        let seen = successor.await.unwrap();

        assert!(format!("{err:#}").contains("final checkpoint"), "{err:#}");
        assert!(!committed.load(Ordering::SeqCst));
        assert_eq!(names(&seen), ["PaneFd", "Abort"], "no Complete was sent");
        assert!(unwritable.is_some(), "a failed final write hands it back");
    }

    /// A successor that declines gets the final checkpoint written for its
    /// snapshot restore, then `Released`; the live PTYs are not committed.
    #[tokio::test]
    async fn a_declined_handoff_writes_the_checkpoint_then_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, mut checkpointer) = fixture_predecessor(tmp.path()).await;
        let (ours, theirs) = UnixStream::pair().unwrap();
        let successor = tokio::spawn(successor(theirs, Successor::Declines));
        let committed = AtomicBool::new(false);

        drive(&app, &ours, &mut checkpointer, &committed, STEP)
            .await
            .expect("declined is an end, not a failure");
        drop(ours);
        let seen = successor.await.unwrap();

        assert!(!committed.load(Ordering::SeqCst));
        assert_eq!(names(&seen), ["Released"]);
        let state = crate::persist::restore::read_checkpoint(&tmp.path().join("state.bin"))
            .expect("the final checkpoint is on disk");
        assert_eq!(state.sessions.len(), 1);
    }

    #[test]
    fn resolves_unmodified_path_when_present() {
        // The common case (no upgrade, or macOS post-upgrade): current_exe() points
        // at a real file, so we re-exec it verbatim.
        let exe = PathBuf::from("/usr/local/bin/kmuxd");
        let got = resolve_successor_exe(exe.clone(), |p| p == Path::new("/usr/local/bin/kmuxd"))
            .expect("should resolve");
        assert_eq!(got, exe);
    }

    #[test]
    fn strips_deleted_marker_and_prefers_replacement() {
        // Linux after an in-place `cargo install`: the running inode is unlinked, so
        // current_exe() returns the " (deleted)" marker while the *new* binary sits
        // at the un-suffixed path. We must re-exec the replacement (the new code).
        let exe = PathBuf::from("/home/u/.cargo/bin/kmuxd (deleted)");
        let got = resolve_successor_exe(exe, |p| p == Path::new("/home/u/.cargo/bin/kmuxd"))
            .expect("should resolve to the replacement");
        assert_eq!(got, PathBuf::from("/home/u/.cargo/bin/kmuxd"));
    }

    #[test]
    fn errors_when_neither_candidate_exists() {
        // Marker present but no replacement has landed (and the literal path is gone
        // too): fail so the handoff rolls back rather than spawning nothing.
        let exe = PathBuf::from("/home/u/.cargo/bin/kmuxd (deleted)");
        let err = resolve_successor_exe(exe, |_| false).expect_err("should error");
        assert!(err.to_string().contains("cannot locate"), "{err}");
    }

    #[test]
    fn falls_back_to_literal_marker_path_when_it_really_exists() {
        // Defensive: a real file literally named "... (deleted)" with no replacement
        // — re-exec it rather than erroring.
        let exe = PathBuf::from("/weird/kmuxd (deleted)");
        let got = resolve_successor_exe(exe.clone(), |p| p == Path::new("/weird/kmuxd (deleted)"))
            .expect("should fall back to the literal path");
        assert_eq!(got, exe);
    }
}
