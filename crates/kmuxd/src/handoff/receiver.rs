//! Incoming-daemon (N) side of a graceful handoff: connect to the predecessor,
//! pull each live PTY master fd, and report them back to startup.
//!
//! The actual relay reconstruction happens afterwards via
//! [`crate::app::ServerApp::restore_with_handoff`], keyed by `pane_id` against
//! the on-disk checkpoint. This module only performs the protocol exchange.
//!
//! Two daemons must never serve at once (issue #207). The predecessor rolls a
//! handoff back — and keeps serving — whenever a step fails before its commit
//! point, so this side takes over only on the predecessor's word (`Released`),
//! or once the predecessor is gone:
//!
//! - The predecessor is identified by the pid in its `Hello`, which must match
//!   the socket's peer credentials (`SO_PEERCRED` on Linux, `LOCAL_PEEREPID` on
//!   macOS) — never by the pid file, which may be missing, stale or name a
//!   reused pid.
//! - A predecessor that goes silent or closes the socket mid-handoff is given
//!   [`PREDECESSOR_EXIT_GRACE`] to exit; still alive after that, it is serving,
//!   so this daemon stands down.
//! - A successor that cannot reach the handoff socket at all (or has no runtime
//!   dir to find it in) has no predecessor to ask, so it asks the control
//!   socket: a daemon still answering there is serving, and this one stands
//!   down.
//!
//! Every frame is bounded by [`STEP`].

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::time::Duration;

use anyhow::{anyhow, bail};
use kmux_protocol::control_rpc::handoff_timeouts::{
    PREDECESSOR_CONNECT, PREDECESSOR_EXIT_GRACE, PREDECESSOR_POLL, STEP,
};
use kmux_protocol::control_rpc::{HANDOFF_PROTOCOL_VERSION, HandoffMessage, HandoffPaneMeta};
use nix::unistd::Pid;
use tokio::net::UnixStream;
use tracing::{info, warn};

use super::{peer_pid, read_frame_within, write_frame_within};

/// The live PTYs pulled from a predecessor that committed.
pub struct Inherited {
    /// The predecessor's auth token, adopted so already-connected clients can
    /// re-authenticate without a credential rotation.
    pub token: String,
    /// Live PTY master fds keyed by `pane_id`, to be adopted by
    /// `restore_with_handoff`. Panes absent here are respawned from the snapshot.
    pub inherited: HashMap<String, (OwnedFd, Pid)>,
    /// The predecessor, as its `Hello` and the socket's peer credentials agree.
    pub predecessor: Pid,
}

/// What this daemon does after the handoff exchange.
pub enum Outcome {
    /// The predecessor committed: adopt its live PTYs and serve.
    Inherited(Inherited),
    /// No live PTYs to adopt (no predecessor, it declined the transfer, or it
    /// died before committing): restore from the checkpoint and serve, once
    /// `predecessor` (when there was one) has exited.
    Restore {
        /// The predecessor to outlast before claiming the pid file.
        predecessor: Option<Pid>,
    },
    /// Another daemon keeps serving (the predecessor rolled back, or could
    /// not be ruled out as still serving): exit without serving, with
    /// [`kmux_protocol::control_rpc::HANDOFF_STOOD_DOWN_EXIT_CODE`].
    StandDown(String),
}

/// Pull live PTY fds from the predecessor daemon and decide how this daemon
/// proceeds.
pub async fn run() -> Outcome {
    let Ok(path) = kmux_sys::dirs::handoff_socket_path() else {
        // Without a runtime dir there is no control socket to ask either.
        return stand_down_unverified("no runtime dir to find the predecessor in");
    };
    let Some(stream) = connect_with_retry(&path, PREDECESSOR_CONNECT).await else {
        let serving = kmux_sys::dirs::socket_path()
            .is_ok_and(|socket| crate::daemon::socket_is_live(&socket));
        return unreachable_predecessor(serving);
    };
    let predecessor = match peer_pid(&stream) {
        Ok(pid) => pid,
        Err(e) => return stand_down_unverified(&format!("{e:#}")),
    };
    let alive = move || nix::sys::signal::kill(predecessor, None).is_ok();
    pull(stream, predecessor, STEP, alive, PREDECESSOR_EXIT_GRACE).await
}

/// No handoff socket answered within [`PREDECESSOR_CONNECT`]. Without a
/// predecessor to ask, the control socket decides: a daemon still `serving`
/// there must not be joined by a second one.
fn unreachable_predecessor(serving: bool) -> Outcome {
    if serving {
        warn!("handoff: no predecessor handoff socket, but a daemon still serves; standing down");
        Outcome::StandDown(
            "could not reach the predecessor's handoff socket, and a daemon still \
             answers the control socket"
                .into(),
        )
    } else {
        warn!(
            "handoff: no predecessor handoff socket and no daemon serving; restoring the snapshot"
        );
        Outcome::Restore { predecessor: None }
    }
}

/// Stand down because whether another daemon serves cannot be told.
fn stand_down_unverified(why: &str) -> Outcome {
    warn!("handoff: {why}; cannot rule out a predecessor still serving, standing down");
    Outcome::StandDown(format!(
        "{why}; cannot rule out a predecessor still serving"
    ))
}

/// Where the exchange stood when the predecessor was lost.
enum Lost {
    /// Before our `Ack`: nothing was committed.
    BeforeAck,
    /// After our `Ack`: the predecessor may have committed.
    AfterAck(Inherited),
}

/// Run the exchange over `stream`, connected to `predecessor` (by its peer
/// credentials), every frame bounded by `step`. When the predecessor is lost
/// mid-way (an error, a timeout, an unexpected frame), the socket is closed —
/// so a predecessor still waiting on us fails its step and rolls back — and
/// the outcome turns on whether `predecessor_alive` stays true through `grace`.
async fn pull(
    stream: UnixStream,
    predecessor: Pid,
    step: Duration,
    predecessor_alive: impl Fn() -> bool,
    grace: Duration,
) -> Outcome {
    let (lost, cause) = match exchange(&stream, predecessor, step).await {
        Ok(outcome) => return outcome,
        Err(lost) => lost,
    };
    drop(stream);
    warn!("handoff: lost the predecessor mid-handoff ({cause:#}); waiting for it to exit");
    let gone = exits_within(&predecessor_alive, grace).await;
    match (lost, gone) {
        (_, false) => {
            warn!("handoff: the predecessor is still running and keeps serving; standing down");
            Outcome::StandDown(format!(
                "lost the predecessor mid-handoff ({cause:#}), and it is still running"
            ))
        }
        (Lost::BeforeAck, true) => {
            warn!("handoff: the predecessor exited before committing; restoring the snapshot");
            Outcome::Restore {
                predecessor: Some(predecessor),
            }
        }
        (Lost::AfterAck(inherited), true) => {
            info!("handoff: the predecessor exited after our Ack; taking over its PTYs");
            Outcome::Inherited(inherited)
        }
    }
}

/// Whether `alive` turns false within `grace`, checked every
/// [`PREDECESSOR_POLL`].
async fn exits_within(alive: &impl Fn() -> bool, grace: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + grace;
    while alive() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(PREDECESSOR_POLL).await;
    }
    true
}

/// The protocol exchange proper. `Err` says where it stood when it failed.
async fn exchange(
    stream: &UnixStream,
    predecessor: Pid,
    step: Duration,
) -> Result<Outcome, (Lost, anyhow::Error)> {
    let before_ack = |e: anyhow::Error| (Lost::BeforeAck, e);

    let hello = read_hello(stream, step).await.map_err(before_ack)?;
    if hello.version != HANDOFF_PROTOCOL_VERSION {
        return decline(stream, step, hello.version, predecessor)
            .await
            .map_err(before_ack);
    }
    if i64::from(hello.pid) != i64::from(predecessor.as_raw()) {
        warn!(
            claimed = hello.pid,
            peer = predecessor.as_raw(),
            "handoff: the predecessor's pid does not match the socket's peer"
        );
        return Ok(Outcome::StandDown(format!(
            "the handoff peer says it is pid {}, but the socket's peer credentials say pid {}",
            hello.pid, predecessor
        )));
    }
    write_frame_within(stream, &HandoffMessage::Accept, None, step)
        .await
        .map_err(|e| before_ack(e.into()))?;

    let inherited = match receive_fds(stream, step, &hello.panes)
        .await
        .map_err(before_ack)?
    {
        Received::Fds(inherited) => inherited,
        Received::Aborted(reason) => return Ok(stand_down(&reason)),
    };
    match read_frame_within(stream, step)
        .await
        .map_err(|e| before_ack(e.into()))?
        .0
    {
        HandoffMessage::Complete => {}
        HandoffMessage::Abort { reason } => return Ok(stand_down(&reason)),
        other => {
            return Err(before_ack(anyhow!(
                "handoff: expected Complete, got {other:?}"
            )));
        }
    }
    // We hold every live fd and the predecessor's final checkpoint is on
    // disk. Our Ack is its commit point — if it arrives.
    let inherited = Inherited {
        token: hello.token,
        inherited,
        predecessor,
    };
    acknowledge(stream, step, inherited).await
}

/// The predecessor's `Hello`.
struct Hello {
    version: u32,
    token: String,
    panes: Vec<HandoffPaneMeta>,
    pid: u32,
}

/// Read the predecessor's `Hello`.
async fn read_hello(stream: &UnixStream, step: Duration) -> anyhow::Result<Hello> {
    match read_frame_within(stream, step).await?.0 {
        HandoffMessage::Hello {
            version,
            token,
            panes,
            pid,
        } => Ok(Hello {
            version,
            token,
            panes,
            pid,
        }),
        other => bail!("handoff: expected Hello, got {other:?}"),
    }
}

/// Decline a predecessor of another protocol `version`. It writes its final
/// checkpoint, then says whether it exits (`Released`: restore from that
/// checkpoint) or keeps serving (`Abort`).
async fn decline(
    stream: &UnixStream,
    step: Duration,
    version: u32,
    predecessor: Pid,
) -> anyhow::Result<Outcome> {
    let reason = format!("predecessor handoff version {version} != {HANDOFF_PROTOCOL_VERSION}");
    warn!("handoff: {reason}; declining live migration, will snapshot-restore");
    write_frame_within(stream, &HandoffMessage::Decline { reason }, None, step).await?;
    match read_frame_within(stream, step).await?.0 {
        HandoffMessage::Released => Ok(Outcome::Restore {
            predecessor: Some(predecessor),
        }),
        HandoffMessage::Abort { reason } => Ok(stand_down(&reason)),
        other => bail!("handoff: expected Released after Decline, got {other:?}"),
    }
}

/// What pulling the fds came to.
enum Received {
    /// One fd per live pane, keyed by `pane_id`.
    Fds(HashMap<String, (OwnedFd, Pid)>),
    /// The predecessor rolled back mid-way, for this reason.
    Aborted(String),
}

/// Pull one fd per live pane, lock-step.
async fn receive_fds(
    stream: &UnixStream,
    step: Duration,
    panes: &[HandoffPaneMeta],
) -> anyhow::Result<Received> {
    let live = panes.iter().filter(|p| p.has_live_fd).count();
    let mut inherited = HashMap::with_capacity(live);
    for _ in 0..live {
        let (msg, fd) = read_frame_within(stream, step).await?;
        let pane_id = match msg {
            HandoffMessage::PaneFd { pane_id } => pane_id,
            HandoffMessage::Abort { reason } => return Ok(Received::Aborted(reason)),
            other => bail!("handoff: expected PaneFd, got {other:?}"),
        };
        let fd = fd.ok_or_else(|| anyhow!("handoff: PaneFd for {pane_id} carried no fd"))?;
        let pid = panes
            .iter()
            .find(|p| p.pane_id == pane_id)
            .map_or(0, |p| p.pid);
        inherited.insert(pane_id, (fd, Pid::from_raw(pid)));
        write_frame_within(stream, &HandoffMessage::PaneFdAck, None, step).await?;
    }
    Ok(Received::Fds(inherited))
}

/// Send our `Ack` and learn whether the predecessor committed (`Released`)
/// or rolled back instead (`Abort`: it timed out waiting for the `Ack`, or is
/// shutting down).
async fn acknowledge(
    stream: &UnixStream,
    step: Duration,
    inherited: Inherited,
) -> Result<Outcome, (Lost, anyhow::Error)> {
    if let Err(e) = write_frame_within(stream, &HandoffMessage::Ack, None, step).await {
        return Err((Lost::AfterAck(inherited), e.into()));
    }
    match read_frame_within(stream, step).await {
        Ok((HandoffMessage::Released, _)) => {
            info!(
                inherited = inherited.inherited.len(),
                "handoff: pulled live PTYs from predecessor"
            );
            Ok(Outcome::Inherited(inherited))
        }
        Ok((HandoffMessage::Abort { reason }, _)) => Ok(stand_down(&reason)),
        Ok((other, _)) => Err((
            Lost::AfterAck(inherited),
            anyhow!("handoff: expected Released, got {other:?}"),
        )),
        Err(e) => Err((Lost::AfterAck(inherited), e.into())),
    }
}

/// The predecessor rolled back (`Abort`) and keeps serving, or is shutting
/// down: either way this daemon must not serve.
fn stand_down(reason: &str) -> Outcome {
    warn!("handoff: the predecessor rolled back ({reason}); standing down");
    Outcome::StandDown(format!("the predecessor rolled back: {reason}"))
}

/// Connect to the handoff socket, retrying for up to `timeout` while the
/// predecessor binds.
async fn connect_with_retry(path: &std::path::Path, timeout: Duration) -> Option<UnixStream> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match UnixStream::connect(path).await {
            Ok(s) => return Some(s),
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use kmux_protocol::control_rpc::HandoffPaneMeta;

    use super::*;
    use crate::handoff::{read_frame, write_frame};

    const STEP: Duration = Duration::from_secs(30);

    /// This process: the peer at the far end of a `UnixStream::pair`.
    fn us() -> Pid {
        Pid::this()
    }

    /// A scripted predecessor at the far end of a socket: send `Hello` (of
    /// `version`, claiming `pid`, with one live pane when `with_pane`), then
    /// play `rest`, sending each `Some` frame and reading one frame for each
    /// `None`. Returns the frames it read.
    async fn predecessor(
        stream: UnixStream,
        version: u32,
        pid: u32,
        with_pane: bool,
        rest: Vec<Option<HandoffMessage>>,
    ) -> Vec<HandoffMessage> {
        let panes = if with_pane {
            vec![HandoffPaneMeta {
                pane_id: "eagle/0".into(),
                pid: 4242,
                has_live_fd: true,
            }]
        } else {
            vec![]
        };
        let hello = HandoffMessage::Hello {
            version,
            token: "adopted".into(),
            panes,
            pid,
        };
        write_frame(&stream, &hello, None).await.unwrap();
        let mut read = Vec::new();
        for step in rest {
            match step {
                Some(HandoffMessage::PaneFd { pane_id }) => {
                    let fd = std::fs::File::open("/dev/null").unwrap();
                    let msg = HandoffMessage::PaneFd { pane_id };
                    write_frame(&stream, &msg, Some(fd.as_raw_fd()))
                        .await
                        .unwrap();
                }
                Some(msg) => write_frame(&stream, &msg, None).await.unwrap(),
                None => match read_frame(&stream).await {
                    Ok((msg, _)) => read.push(msg),
                    Err(_) => break,
                },
            }
        }
        read
    }

    fn names(frames: &[HandoffMessage]) -> Vec<String> {
        frames
            .iter()
            .map(|msg| {
                format!("{msg:?}")
                    .split([' ', '{'])
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// Run the successor side against `script` from a predecessor of
    /// `version` that claims to be this process, with the predecessor's
    /// liveness answered by `alive`.
    async fn pull_against(
        version: u32,
        with_pane: bool,
        script: Vec<Option<HandoffMessage>>,
        alive: bool,
    ) -> (Outcome, Vec<HandoffMessage>) {
        let pid = us().as_raw().unsigned_abs();
        pull_claiming(version, pid, with_pane, script, alive).await
    }

    /// [`pull_against`] with the pid the predecessor claims in its `Hello`.
    async fn pull_claiming(
        version: u32,
        pid: u32,
        with_pane: bool,
        script: Vec<Option<HandoffMessage>>,
        alive: bool,
    ) -> (Outcome, Vec<HandoffMessage>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let far = tokio::spawn(predecessor(theirs, version, pid, with_pane, script));
        let outcome = pull(ours, us(), STEP, move || alive, PREDECESSOR_EXIT_GRACE).await;
        (outcome, far.await.unwrap())
    }

    fn abort(reason: &str) -> Option<HandoffMessage> {
        Some(HandoffMessage::Abort {
            reason: reason.into(),
        })
    }

    /// The reason a stand-down gives, or a panic naming what came instead.
    fn stood_down(outcome: Outcome) -> String {
        match outcome {
            Outcome::StandDown(reason) => reason,
            Outcome::Inherited(_) => panic!("expected a stand-down, got an inheritance"),
            Outcome::Restore { .. } => panic!("expected a stand-down, got a restore"),
        }
    }

    /// A predecessor that commits hands over its live fd, its token, and its
    /// identity.
    #[tokio::test]
    async fn a_committed_handoff_inherits_the_live_ptys_and_the_token() {
        let script = vec![
            None,
            Some(HandoffMessage::PaneFd {
                pane_id: "eagle/0".into(),
            }),
            None,
            Some(HandoffMessage::Complete),
            None,
            Some(HandoffMessage::Released),
        ];
        let (outcome, read) = pull_against(HANDOFF_PROTOCOL_VERSION, true, script, true).await;

        assert_eq!(names(&read), ["Accept", "PaneFdAck", "Ack"]);
        let Outcome::Inherited(inherited) = outcome else {
            panic!("expected the PTYs to be inherited");
        };
        assert_eq!(inherited.token, "adopted");
        assert_eq!(inherited.predecessor, us());
        let (_, pid) = &inherited.inherited["eagle/0"];
        assert_eq!(pid.as_raw(), 4242);
    }

    /// Each `Abort` — instead of a `PaneFd`, instead of `Complete`, after a
    /// `Decline`, after our `Ack` — stands this daemon down, even with the
    /// predecessor already gone: `Abort` is its word that it rolled back, so
    /// its exit is no sign that it committed (it may be shutting down).
    #[tokio::test(start_paused = true)]
    async fn every_abort_stands_down_at_once_even_with_the_predecessor_gone() {
        let version = HANDOFF_PROTOCOL_VERSION;
        let cases = [
            (
                "instead of a PaneFd",
                version,
                true,
                vec![None, abort("fd")],
            ),
            (
                "instead of Complete",
                version,
                false,
                vec![None, abort("hold")],
            ),
            (
                "after a Decline",
                version + 1,
                false,
                vec![None, abort("disk")],
            ),
            (
                "after our Ack",
                version,
                false,
                vec![None, Some(HandoffMessage::Complete), None, abort("late")],
            ),
        ];
        for (when, version, with_pane, script) in cases {
            // Taken for a lost predecessor instead, a gone one would mean a
            // restore (before our `Ack`) or a takeover (after it).
            let (outcome, _) = pull_against(version, with_pane, script, false).await;
            let reason = stood_down(outcome);
            assert!(reason.contains("rolled back"), "{when}: {reason}");
        }
    }

    /// A predecessor that rolls back after our `Ack` while it still runs
    /// says so, and this daemon stands down rather than serving beside it.
    #[tokio::test]
    async fn an_abort_after_our_ack_stands_down() {
        let script = vec![None, Some(HandoffMessage::Complete), None, abort("no Ack")];
        let (outcome, _) = pull_against(HANDOFF_PROTOCOL_VERSION, false, script, true).await;
        assert!(stood_down(outcome).contains("no Ack"));
    }

    /// A predecessor of another handoff version is declined, and its
    /// `Released` (its final checkpoint is on disk) means a snapshot restore
    /// once it has exited.
    #[tokio::test]
    async fn another_version_is_declined_and_restored_from_the_snapshot() {
        let script = vec![None, Some(HandoffMessage::Released)];
        let (outcome, read) = pull_against(HANDOFF_PROTOCOL_VERSION + 1, true, script, true).await;
        assert_eq!(names(&read), ["Decline"]);
        assert!(matches!(
            outcome,
            Outcome::Restore { predecessor: Some(pid) } if pid == us()
        ));
    }

    /// A `Hello` whose pid is not the socket's peer is refused before
    /// anything is accepted: this daemon stands down.
    #[tokio::test]
    async fn a_hello_from_another_pid_than_the_peer_stands_down() {
        let impostor = us().as_raw().unsigned_abs() + 1;
        let (outcome, read) =
            pull_claiming(HANDOFF_PROTOCOL_VERSION, impostor, false, vec![None], true).await;
        assert!(read.is_empty(), "nothing accepted: {read:?}");
        assert!(stood_down(outcome).contains("peer credentials"));
    }

    /// A predecessor lost before our `Ack` (here: it goes quiet after
    /// `Accept`) is waited on. Still running, it is serving, so this daemon
    /// stands down; gone, the snapshot is restored.
    #[tokio::test(start_paused = true)]
    async fn a_predecessor_lost_before_our_ack_decides_by_its_liveness() {
        let quiet = || vec![None];
        let (outcome, _) = pull_against(HANDOFF_PROTOCOL_VERSION, false, quiet(), true).await;
        assert!(stood_down(outcome).contains("still running"), "alive");

        let (outcome, _) = pull_against(HANDOFF_PROTOCOL_VERSION, false, quiet(), false).await;
        assert!(
            matches!(outcome, Outcome::Restore { predecessor: Some(pid) } if pid == us()),
            "gone: restore"
        );
    }

    /// A predecessor that exits after our `Ack` without a `Released` did
    /// commit: its live PTYs are taken over.
    #[tokio::test]
    async fn a_predecessor_gone_after_our_ack_is_taken_over() {
        let script = vec![None, Some(HandoffMessage::Complete), None];
        let (outcome, _) = pull_against(HANDOFF_PROTOCOL_VERSION, false, script, false).await;
        assert!(matches!(outcome, Outcome::Inherited(_)));
    }

    /// With no handoff socket to reach, the control socket decides: a daemon
    /// still serving there means stand down, none means restore.
    #[test]
    fn an_unreachable_predecessor_is_decided_by_the_control_socket() {
        assert!(stood_down(unreachable_predecessor(true)).contains("control socket"));
        assert!(matches!(
            unreachable_predecessor(false),
            Outcome::Restore { predecessor: None }
        ));
    }

    /// Connecting gives up once its timeout has passed with nothing
    /// listening, and connects at once to a socket that is.
    #[tokio::test(start_paused = true)]
    async fn connecting_retries_until_its_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("handoff.sock");
        let timeout = Duration::from_secs(3);
        let started = tokio::time::Instant::now();
        assert!(connect_with_retry(&path, timeout).await.is_none());
        assert!(started.elapsed() >= timeout);

        let _listener = tokio::net::UnixListener::bind(&path).unwrap();
        let started = tokio::time::Instant::now();
        assert!(connect_with_retry(&path, timeout).await.is_some());
        assert!(started.elapsed() < timeout);
    }

    /// Waiting for a predecessor to exit ends as soon as it has, and gives up
    /// at the grace period while it runs on.
    #[tokio::test(start_paused = true)]
    async fn exits_within_waits_up_to_the_grace_period() {
        let grace = Duration::from_secs(3);
        let started = tokio::time::Instant::now();
        assert!(!exits_within(&|| true, grace).await);
        assert!(started.elapsed() >= grace);

        let checks = std::cell::Cell::new(0);
        let dies_on_third_check = || {
            checks.set(checks.get() + 1);
            checks.get() < 3
        };
        assert!(exits_within(&dies_on_third_check, grace).await);
        assert_eq!(checks.get(), 3);
    }
}
