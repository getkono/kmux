//! Outgoing-daemon (O) side of a graceful handoff: spawn the successor, stream
//! each live PTY master fd to it, write the final checkpoint, commit, and exit.
//!
//! `run` returns `Ok(())` once the handoff has *committed* (the successor holds
//! every live fd, or declined and will snapshot-restore from our final
//! checkpoint). The caller then tears down its listeners and exits. On `Err`
//! the handoff was rolled back before its commit point: the PTY readers run
//! again, the checkpoint accepts writes again, pane creation is admitted
//! again, the successor was told to stand down and has exited (or was
//! killed), and the caller simply resumes serving — or, when the rollback was
//! a shutdown's, shuts down.
//!
//! Everything fallible happens before the commit point (issue #207): each frame
//! is bounded (see [`handoff_timeouts`]), and the final checkpoint is written
//! and `fsync`ed before `Complete` is sent. The commit point is receiving the
//! successor's `Ack`; what follows it (keep-alive, stopping the readers,
//! `Released`) cannot fail in a way that undoes the handoff.
//!
//! The successor is this daemon's direct child, so it can always be stopped:
//! one that does not connect within [`SUCCESSOR_CONNECT`] is killed, and one
//! told to stand down that has not exited within [`SUCCESSOR_STAND_DOWN_GRACE`]
//! is killed too. Two daemons never serve at once.

use std::os::fd::AsRawFd;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use kmux_protocol::control_rpc::handoff_timeouts::{
    self, FROZEN, HOLD, STEP, SUCCESSOR_CONNECT, SUCCESSOR_STAND_DOWN_GRACE,
};
use kmux_protocol::control_rpc::{
    DAEMON_BOOT_ARGS, HANDOFF_PROTOCOL_VERSION, HANDOFF_STOOD_DOWN_EXIT_CODE, HandoffMessage,
    HandoffPaneMeta,
};
use nix::unistd::Pid;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Child;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::app::ServerApp;
use crate::persist::checkpoint::{Checkpointer, SealedCheckpoint};

use super::{Cancel, PathGuard, peer_pid, read_frame_within, write_frame_within};

/// Drive a graceful handoff to a freshly-spawned successor daemon.
///
/// On `Ok(())` the handoff committed and `app` must not serve further (the
/// caller releases sockets and exits). On `Err(_)` the handoff was rolled back
/// and the successor stopped; the error says why, and how the successor
/// ended. `committed` is set the moment the successor's `Ack` arrives.
/// `cancel` (a shutdown signal) rolls back a handoff that has not committed
/// yet, telling the successor to stand down, so that both daemons stop.
///
/// The final checkpoint consumes `checkpointer` (left `None`) once the
/// handoff commits or is declined; a rollback puts it back, unsealed.
pub async fn run(
    app: &Arc<ServerApp>,
    checkpointer: &mut Option<Checkpointer>,
    committed: &AtomicBool,
    cancel: Cancel,
) -> anyhow::Result<()> {
    // Without a checkpoint the successor has nothing to rebuild the panes
    // from; refuse before anything has started.
    if checkpointer.is_none() {
        bail!("no checkpoint path; cannot hand off");
    }
    // No pane may be created from here on: one made after the panes are
    // advertised would be neither transferred nor frozen.
    let gate = app.close_pane_creation().await;
    let result = hand_off(app, checkpointer, committed, cancel).await;
    if result.is_ok() {
        // This daemon exits: it creates no pane again.
        app.keep_pane_creation_closed(gate);
    } else {
        drop(gate);
    }
    result
}

/// [`run`], with pane creation closed.
async fn hand_off(
    app: &Arc<ServerApp>,
    checkpointer: &mut Option<Checkpointer>,
    committed: &AtomicBool,
    cancel: Cancel,
) -> anyhow::Result<()> {
    let path = kmux_sys::dirs::handoff_socket_path()?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding handoff socket {}", path.display()))?;
    let _guard = PathGuard(path.clone());

    let child = spawn_successor().context("spawning successor daemon")?;
    info!("handoff: spawned successor; awaiting its connection within {SUCCESSOR_CONNECT:?}");
    let mut successor = Successor { child, peer: None };

    let stream = match await_successor(&listener, &mut successor, SUCCESSOR_CONNECT, &cancel).await
    {
        Ok(stream) => stream,
        Err(e) => {
            let ended = successor.stop(SUCCESSOR_STAND_DOWN_GRACE).await;
            return Err(e.context(ended));
        }
    };
    let link = Link {
        stream: &stream,
        step: STEP,
        cancel,
    };
    match drive(app, &link, checkpointer, committed).await {
        Ok(()) => Ok(()),
        Err(e) => {
            drop(stream);
            let ended = successor.stop(SUCCESSOR_STAND_DOWN_GRACE).await;
            Err(e.context(ended))
        }
    }
}

/// Accept the successor's connection within `timeout`, and learn its pid from
/// the socket's peer credentials.
async fn await_successor(
    listener: &UnixListener,
    successor: &mut Successor,
    timeout: Duration,
    cancel: &Cancel,
) -> anyhow::Result<UnixStream> {
    let accepted = tokio::select! {
        biased;
        () = cancel.cancelled() => bail!("the daemon is shutting down"),
        accepted = tokio::time::timeout(timeout, listener.accept()) => accepted,
    };
    let (stream, _) = match accepted {
        // Almost always a boot failure (full disk, panic during restore); its
        // output is in the boot log. The caller kills the successor, so it
        // cannot connect — and serve — later.
        Err(_) => bail!(
            "successor did not connect within {timeout:?} (check kmuxd-boot.log); it was stopped"
        ),
        Ok(accepted) => accepted.context("accepting successor handoff connection")?,
    };
    successor.peer = Some(peer_pid(&stream)?);
    Ok(stream)
}

/// The successor daemon this one spawned. Its direct child — unless it is a
/// build from before handoff version 3, which daemonizes itself, and is then
/// known by its connection's peer credentials alone.
struct Successor {
    child: Child,
    /// Its pid by the handoff socket's peer credentials, once it connected.
    peer: Option<Pid>,
}

impl Successor {
    /// Make sure the successor is gone: give it `grace` to exit on its own
    /// (it stands down on `Abort` or a closed socket), then kill it. Says how
    /// it ended, for the rollback's report.
    async fn stop(mut self, grace: Duration) -> String {
        // Read before waiting: a reaped child has no pid.
        let own_pid = self.child.id().map(u32::cast_signed);
        let deadline = Instant::now() + grace;
        let exited = tokio::time::timeout_at(deadline, self.child.wait()).await;
        // A self-daemonizing successor's child is only its launcher; the
        // daemon itself is the peer.
        let detached = self
            .peer
            .filter(|peer| Some(peer.as_raw()) != own_pid && *peer != Pid::this());
        let detached_gone = match detached {
            Some(peer) => exits_by(peer, deadline).await,
            None => true,
        };
        match (exited, detached_gone) {
            (Ok(Ok(status)), true) => describe_exit(status),
            (Ok(Err(e)), true) => format!("the successor could not be waited on ({e})"),
            (_, _) => {
                let _ = self.child.start_kill();
                if let Some(peer) = detached {
                    let _ = nix::sys::signal::kill(peer, nix::sys::signal::Signal::SIGKILL);
                }
                let _ = self.child.wait().await;
                warn!("handoff: the successor did not exit within {grace:?}; killed it");
                format!("the successor did not exit within {grace:?} and was killed")
            }
        }
    }
}

/// Whether `pid` has exited by `deadline`, polled.
async fn exits_by(pid: Pid, deadline: Instant) -> bool {
    while nix::sys::signal::kill(pid, None).is_ok() {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(handoff_timeouts::PREDECESSOR_POLL).await;
    }
    true
}

/// How a successor that was told to stand down (or never connected) ended.
fn describe_exit(status: ExitStatus) -> String {
    if status.code() == Some(HANDOFF_STOOD_DOWN_EXIT_CODE) {
        "the successor stood down".to_string()
    } else {
        format!("the successor exited ({status})")
    }
}

/// The connection to the successor: every frame bounded, and every step
/// abandoned for a shutdown.
struct Link<'a> {
    stream: &'a UnixStream,
    /// The bound on one ordinary step.
    step: Duration,
    cancel: Cancel,
}

impl Link<'_> {
    /// The deadline of a step starting now.
    fn next_step(&self) -> Instant {
        Instant::now() + self.step
    }

    /// Send `msg` (with `fd`) by `deadline`, unless the daemon shuts down.
    async fn send(
        &self,
        msg: &HandoffMessage,
        fd: Option<std::os::fd::RawFd>,
        deadline: Instant,
    ) -> anyhow::Result<()> {
        let within = deadline.saturating_duration_since(Instant::now());
        self.unless_cancelled(write_frame_within(self.stream, msg, fd, within))
            .await?
            .map_err(Into::into)
    }

    /// Receive a frame by `deadline`, unless the daemon shuts down.
    async fn recv(&self, deadline: Instant) -> anyhow::Result<HandoffMessage> {
        let within = deadline.saturating_duration_since(Instant::now());
        let (msg, _) = self
            .unless_cancelled(read_frame_within(self.stream, within))
            .await??;
        Ok(msg)
    }

    /// `step`, or an error once the daemon is shutting down. A shutdown wins
    /// a tie, so an `Ack` still unread when it lands does not commit.
    async fn unless_cancelled<T>(&self, step: impl Future<Output = T>) -> anyhow::Result<T> {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => bail!("the daemon is shutting down"),
            done = step => Ok(done),
        }
    }
}

/// How a handoff that reached its end ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The successor acknowledged every fd: it takes over the live PTYs.
    Committed,
    /// The successor declined; it restores from our final checkpoint.
    Declined,
}

/// Run the handoff protocol over `link`, connected to the successor. Rolls
/// back on any failure before the commit point (see the module docs).
async fn drive(
    app: &ServerApp,
    link: &Link<'_>,
    checkpointer: &mut Option<Checkpointer>,
    committed: &AtomicBool,
) -> anyhow::Result<()> {
    let mut final_checkpoint = FinalCheckpoint {
        slot: checkpointer,
        sealed: None,
    };
    match transfer(app, link, &mut final_checkpoint, committed).await {
        Ok(ending) => {
            final_checkpoint.keep_sealed();
            finish(app, link, ending).await;
            Ok(())
        }
        Err(e) => {
            drop(final_checkpoint);
            roll_back(app, link, &e).await;
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
    /// off the runtime, sealing it against later periodic writes. Never
    /// abandoned half-way: the write owns the checkpointer while it runs.
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
/// stream the live fds, hold the readers, write the final checkpoint, and
/// exchange `Complete` for `Ack`. Any error leaves the handoff uncommitted.
async fn transfer(
    app: &ServerApp,
    link: &Link<'_>,
    final_checkpoint: &mut FinalCheckpoint<'_>,
    committed: &AtomicBool,
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
        pid: std::process::id(),
    };
    link.send(&hello, None, link.next_step()).await?;

    match link.recv(link.next_step()).await? {
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

    stream_fds(app, link, &panes).await?;

    // Park every PTY reader, then snapshot: the successor seeds from exactly
    // what we consumed, and anything after sits in the kernel buffer for it.
    // Both happen before `Complete`, so a failure still rolls back.
    if !link.unless_cancelled(app.hold_relays(HOLD)).await? {
        bail!("handoff: PTY readers did not park within {HOLD:?}");
    }
    final_checkpoint.write(app).await?;

    // The panes are frozen from here until the commit or a rollback: the
    // rest of the exchange shares one short bound.
    let frozen_until = Instant::now() + FROZEN;
    link.send(&HandoffMessage::Complete, None, frozen_until)
        .await?;
    match link.recv(frozen_until).await? {
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
    link: &Link<'_>,
    panes: &[HandoffPaneMeta],
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
        link.send(&pane_fd, Some(fd.as_raw_fd()), link.next_step())
            .await?;
        // Our dup has been copied into the successor; close ours. The child stays
        // alive: the successor holds a dup, and our original master fds are still
        // open until we exit.
        drop(fd);
        match link.recv(link.next_step()).await? {
            HandoffMessage::PaneFdAck => {}
            other => bail!(
                "handoff: expected PaneFdAck for {}, got {other:?}",
                meta.pane_id
            ),
        }
    }
    Ok(())
}

/// After the commit point. Nothing here can undo the handoff — not even a
/// shutdown signal: a failure is logged and the daemon exits all the same.
async fn finish(app: &ServerApp, link: &Link<'_>, ending: Ending) {
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
    if let Err(e) =
        write_frame_within(link.stream, &HandoffMessage::Released, None, link.step).await
    {
        warn!("handoff: could not send Released ({e}); the successor proceeds once we exit");
    }
    info!(?ending, "handoff: committed; releasing sockets and exiting");
}

/// Undo everything [`transfer`] did before failing, and tell the successor to
/// stand down: the readers run again. (The checkpoint was unsealed as the
/// [`FinalCheckpoint`] dropped.) Not abandoned for a shutdown: the `Abort` is
/// what stops the successor too, and it is bounded by
/// [`SUCCESSOR_STAND_DOWN_GRACE`].
async fn roll_back(app: &ServerApp, link: &Link<'_>, cause: &anyhow::Error) {
    warn!("handoff: rolling back before the commit point: {cause:#}");
    app.release_relays().await;
    let abort = HandoffMessage::Abort {
        reason: format!("{cause:#}"),
    };
    // Best effort: a successor that cannot be told sees the socket close and
    // stands down on finding us alive — or is killed, if it lingers.
    let _ = write_frame_within(link.stream, &abort, None, SUCCESSOR_STAND_DOWN_GRACE).await;
}

/// Spawn the successor daemon: re-exec this binary with the standard boot args
/// plus `--handoff`, as a direct child in a process group of its own (it does
/// not daemonize, so this daemon can stop it and learn how it exited). The
/// binary path is resolved by [`resolve_successor_exe`] so a live upgrade (the
/// new binary swapped in over our path) re-execs the *new* code.
fn spawn_successor() -> anyhow::Result<Child> {
    let exe = resolve_successor_exe(
        std::env::current_exe().context("resolving current executable")?,
        std::path::Path::exists,
    )?;
    let mut args: Vec<&str> = DAEMON_BOOT_ARGS.to_vec();
    args.push("--handoff");
    // Capture the successor's stdout+stderr (until its log is up) in the boot
    // log so a boot failure (full disk, panic during restore) is visible to
    // `kmux daemon restart` instead of silently timing out the handoff.
    let (out, err) = crate::boot_log_stdio();
    tokio::process::Command::new(&exe)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        // Where a daemon runs: it does not `daemonize` to get there.
        .current_dir("/")
        // A signal to this daemon's process group must not reach it.
        .process_group(0)
        .spawn()
        .with_context(|| format!("spawning {}", exe.display()))
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
    use kmux_protocol::control_rpc::handoff_timeouts::FROZEN;
    use kmux_protocol::messages::{ClientCapabilities, TermSize};
    use tokio::net::UnixStream;
    use tokio::time::Instant;

    use super::{Link, Successor, await_successor, drive, resolve_successor_exe};
    use crate::app::ServerApp;
    use crate::handoff::{Cancel, read_frame, write_frame};
    use crate::persist::checkpoint::{Checkpointer, Written};

    /// The bound on each handoff step in these tests.
    const STEP: Duration = Duration::from_secs(30);

    /// What a scripted successor does once it has read `Hello`.
    #[derive(Clone, Copy)]
    enum Script {
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
    /// socket, with whether each carried an fd. `on_complete` runs as
    /// `Complete` arrives, before it is answered.
    async fn successor(
        stream: UnixStream,
        script: Script,
        on_complete: impl FnOnce() + Send,
    ) -> Vec<(HandoffMessage, bool)> {
        let (hello, _) = read_frame(&stream).await.expect("Hello");
        let HandoffMessage::Hello { pid, .. } = hello else {
            panic!("expected Hello, got {hello:?}");
        };
        assert_eq!(pid, std::process::id(), "the Hello names the predecessor");
        let answer = match script {
            Script::Declines => HandoffMessage::Decline {
                reason: "test".into(),
            },
            Script::Cooperates | Script::StallsAfterAccept | Script::StallsAtComplete => {
                HandoffMessage::Accept
            }
        };
        write_frame(&stream, &answer, None)
            .await
            .expect("answer Hello");

        let mut on_complete = Some(on_complete);
        let mut seen = Vec::new();
        while let Ok((msg, fd)) = read_frame(&stream).await {
            let reply = match (&msg, script) {
                (HandoffMessage::PaneFd { .. }, Script::Cooperates | Script::StallsAtComplete) => {
                    Some(HandoffMessage::PaneFdAck)
                }
                (HandoffMessage::Complete, Script::Cooperates) => {
                    if let Some(hook) = on_complete.take() {
                        hook();
                    }
                    Some(HandoffMessage::Ack)
                }
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

    /// Drive a handoff over `ours`, never cancelled.
    async fn drive_over(
        app: &ServerApp,
        ours: &UnixStream,
        checkpointer: &mut Option<Checkpointer>,
        committed: &AtomicBool,
    ) -> anyhow::Result<()> {
        let (_keep, cancel) = Cancel::channel();
        let link = Link {
            stream: ours,
            step: STEP,
            cancel,
        };
        drive(app, &link, checkpointer, committed).await
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
        let successor = tokio::spawn(successor(theirs, Script::Cooperates, || {}));
        let committed = AtomicBool::new(false);

        drive_over(&app, &ours, &mut checkpointer, &committed)
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

    /// A shutdown signal that lands after the successor sent its `Ack` but
    /// before this daemon read it does not commit: the handoff rolls back and
    /// the successor is told to stand down, so it never adopts PTYs whose
    /// children this daemon's shutdown kills (issue #207).
    #[tokio::test]
    async fn a_shutdown_before_the_ack_is_read_rolls_back_and_aborts() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, mut checkpointer) = fixture_predecessor(tmp.path()).await;
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (shutdown, cancel) = Cancel::channel();
        let successor = tokio::spawn(successor(theirs, Script::Cooperates, move || {
            shutdown.cancel();
        }));
        let committed = AtomicBool::new(false);
        let link = Link {
            stream: &ours,
            step: STEP,
            cancel,
        };

        let err = drive(&app, &link, &mut checkpointer, &committed)
            .await
            .expect_err("rolled back");
        drop(ours);
        let seen = successor.await.unwrap();

        assert!(format!("{err:#}").contains("shutting down"), "{err:#}");
        assert!(!committed.load(Ordering::SeqCst));
        assert_eq!(names(&seen), ["PaneFd", "Complete", "Abort"]);
        assert!(
            checkpointer.is_some(),
            "handed back for the shutdown's write"
        );
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
        let successor = tokio::spawn(successor(theirs, Script::StallsAfterAccept, || {}));
        let committed = AtomicBool::new(false);
        let started = Instant::now();

        let err = drive_over(&app, &ours, &mut checkpointer, &committed)
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
    /// back after its final checkpoint was written — within the short frozen
    /// window, not a full step, since the panes are frozen meanwhile. The
    /// checkpointer is handed back unsealed, so periodic writes resume and a
    /// later shutdown can make its own final write.
    #[tokio::test(start_paused = true)]
    async fn a_rollback_after_the_final_checkpoint_is_quick_and_unseals_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, mut checkpointer) = fixture_predecessor(tmp.path()).await;
        let periodic = checkpointer.as_ref().unwrap().periodic();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let successor = tokio::spawn(successor(theirs, Script::StallsAtComplete, || {}));
        let committed = AtomicBool::new(false);
        let started = Instant::now();

        let err = drive_over(&app, &ours, &mut checkpointer, &committed)
            .await
            .expect_err("times out waiting for Ack");
        let frozen_for = started.elapsed();
        drop(ours);
        let seen = successor.await.unwrap();

        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert!(frozen_for >= FROZEN && frozen_for < STEP, "{frozen_for:?}");
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
        let successor = tokio::spawn(successor(theirs, Script::Cooperates, || {}));
        let committed = AtomicBool::new(false);

        let err = drive_over(&app, &ours, &mut unwritable, &committed)
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
        let successor = tokio::spawn(successor(theirs, Script::Declines, || {}));
        let committed = AtomicBool::new(false);

        drive_over(&app, &ours, &mut checkpointer, &committed)
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

    /// A successor process running `script` under `sh`.
    fn successor_process(script: &str) -> Successor {
        let child = tokio::process::Command::new("/bin/sh")
            .args(["-c", script])
            .spawn()
            .expect("spawn sh");
        Successor { child, peer: None }
    }

    /// Stopping a successor reports one that stood down by its exit code,
    /// tells any other exit apart, and kills one that does not exit within
    /// the grace period.
    #[tokio::test]
    async fn stopping_a_successor_reports_how_it_ended() {
        let exit = |code: i32| successor_process(&format!("exit {code}"));
        let grace = Duration::from_secs(10);
        let code = kmux_protocol::control_rpc::HANDOFF_STOOD_DOWN_EXIT_CODE;
        assert_eq!(exit(code).stop(grace).await, "the successor stood down");
        assert!(exit(3).stop(grace).await.contains("exited"));

        let started = std::time::Instant::now();
        let lingering = successor_process("exec sleep 600");
        let pid = nix::unistd::Pid::from_raw(lingering.child.id().unwrap().cast_signed());
        let ended = lingering.stop(Duration::from_millis(200)).await;
        assert!(ended.contains("killed"), "{ended}");
        assert!(started.elapsed() < grace, "not waited out");
        assert!(
            nix::sys::signal::kill(pid, None).is_err(),
            "gone and reaped"
        );
    }

    /// A successor from before handoff version 3 daemonizes itself: its
    /// launcher exits at once, and the daemon is known by its peer
    /// credentials alone. One that lingers is killed by that pid.
    #[tokio::test]
    async fn a_self_daemonized_successor_is_stopped_by_its_peer_pid() {
        use std::os::unix::process::ExitStatusExt;

        let mut daemon = std::process::Command::new("/bin/sleep")
            .arg("600")
            .spawn()
            .expect("spawn sleep");
        let mut successor = successor_process("exit 0");
        successor.peer = Some(nix::unistd::Pid::from_raw(daemon.id().cast_signed()));

        let ended = successor.stop(Duration::from_millis(200)).await;
        assert!(ended.contains("killed"), "{ended}");
        let status = daemon.wait().expect("reaped");
        assert_eq!(status.signal(), Some(nix::libc::SIGKILL));
    }

    /// A self-daemonized successor that exits on its own within the grace
    /// period is not killed: its launcher's exit is reported.
    #[tokio::test]
    async fn a_self_daemonized_successor_that_exits_is_left_alone() {
        let mut daemon = tokio::process::Command::new("/bin/sh")
            .args(["-c", "sleep 0.2"])
            .spawn()
            .expect("spawn sh");
        let pid = nix::unistd::Pid::from_raw(daemon.id().unwrap().cast_signed());
        // Reaped as it exits, so it does not linger as a zombie `kill(pid, 0)`
        // still finds.
        let reaper = tokio::spawn(async move { daemon.wait().await });
        let mut successor = successor_process("exit 0");
        successor.peer = Some(pid);

        let ended = successor.stop(Duration::from_secs(20)).await;
        assert!(ended.contains("exited"), "{ended}");
        assert!(reaper.await.unwrap().unwrap().success(), "not killed");
    }

    /// A successor that never connects is given up on at the connect
    /// timeout; one that does is known by its peer credentials.
    #[tokio::test]
    async fn awaiting_the_successor_is_bounded_and_learns_its_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("handoff.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (_keep, cancel) = Cancel::channel();
        let mut successor = successor_process("exec sleep 600");

        let err = await_successor(
            &listener,
            &mut successor,
            Duration::from_millis(50),
            &cancel,
        )
        .await
        .expect_err("nobody connects");
        assert!(format!("{err:#}").contains("did not connect"), "{err:#}");

        let _client = UnixStream::connect(&path).await.unwrap();
        await_successor(&listener, &mut successor, Duration::from_secs(10), &cancel)
            .await
            .expect("connected");
        assert_eq!(successor.peer, Some(nix::unistd::Pid::this()));
        // The peer here is this test process, which stopping never signals.
        assert!(successor.stop(Duration::ZERO).await.contains("killed"));
    }

    /// A shutdown while waiting for the successor to connect ends the wait.
    #[tokio::test]
    async fn a_shutdown_ends_the_wait_for_the_successor() {
        let tmp = tempfile::tempdir().unwrap();
        let listener = tokio::net::UnixListener::bind(tmp.path().join("h.sock")).unwrap();
        let (shutdown, cancel) = Cancel::channel();
        shutdown.cancel();
        let mut successor = successor_process("exec sleep 600");
        let err = await_successor(&listener, &mut successor, Duration::from_secs(600), &cancel)
            .await
            .expect_err("cancelled");
        assert!(format!("{err:#}").contains("shutting down"), "{err:#}");
        successor.stop(Duration::ZERO).await;
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
