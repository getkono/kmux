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
//! or once the predecessor is gone. Every frame is bounded by
//! [`STEP_TIMEOUT`]; a predecessor that goes silent or closes the socket is
//! given [`PREDECESSOR_EXIT_GRACE`] to exit, and if it is still alive after
//! that it is serving, so this daemon stands down.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::time::Duration;

use anyhow::{anyhow, bail};
use kmux_protocol::control_rpc::{HANDOFF_PROTOCOL_VERSION, HandoffMessage, HandoffPaneMeta};
use nix::unistd::Pid;
use tokio::net::UnixStream;
use tracing::{info, warn};

use super::{STEP_TIMEOUT, read_frame_within, write_frame_within};

/// The live PTYs pulled from a predecessor that committed.
pub struct Inherited {
    /// The predecessor's auth token, adopted so already-connected clients can
    /// re-authenticate without a credential rotation.
    pub token: String,
    /// Live PTY master fds keyed by `pane_id`, to be adopted by
    /// `restore_with_handoff`. Panes absent here are respawned from the snapshot.
    pub inherited: HashMap<String, (OwnedFd, Pid)>,
}

/// What this daemon does after the handoff exchange.
pub enum Outcome {
    /// The predecessor committed: adopt its live PTYs and serve.
    Inherited(Inherited),
    /// No live PTYs to adopt (no predecessor, it declined the transfer, or it
    /// died before committing): restore from the checkpoint and serve.
    Restore,
    /// The predecessor rolled the handoff back and keeps serving: exit
    /// without serving.
    StandDown,
}

/// How long to keep retrying the initial connect: the predecessor may not have
/// bound the handoff socket yet when we start.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a predecessor that went silent or closed the socket gets to exit
/// before it is taken to be serving still. One that committed exits within
/// moments of its last frame.
const PREDECESSOR_EXIT_GRACE: Duration = Duration::from_secs(15);

/// How often a predecessor's liveness is checked while waiting for it to exit.
const PREDECESSOR_POLL: Duration = Duration::from_millis(100);

/// Pull live PTY fds from the predecessor daemon (pid `predecessor`, when
/// known) and decide how this daemon proceeds.
pub async fn run(predecessor: Option<Pid>) -> Outcome {
    let Ok(path) = kmux_sys::dirs::handoff_socket_path() else {
        warn!("handoff: no runtime dir; falling back to snapshot restore");
        return Outcome::Restore;
    };
    let Some(stream) = connect_with_retry(&path).await else {
        warn!("handoff: no predecessor handoff socket; falling back to snapshot restore");
        return Outcome::Restore;
    };
    let alive = move || predecessor.is_some_and(|pid| nix::sys::signal::kill(pid, None).is_ok());
    pull(stream, STEP_TIMEOUT, alive, PREDECESSOR_EXIT_GRACE).await
}

/// Where the exchange stood when the predecessor was lost.
enum Lost {
    /// Before our `Ack`: nothing was committed.
    BeforeAck,
    /// After our `Ack`: the predecessor may have committed.
    AfterAck(Inherited),
}

/// Run the exchange over `stream`, every frame bounded by `step`. When the
/// predecessor is lost mid-way (an error, a timeout, an unexpected frame), the
/// socket is closed — so a predecessor still waiting on us fails its step and
/// rolls back — and the outcome turns on whether `predecessor_alive` stays
/// true through `grace`.
async fn pull(
    stream: UnixStream,
    step: Duration,
    predecessor_alive: impl Fn() -> bool,
    grace: Duration,
) -> Outcome {
    let (lost, cause) = match exchange(&stream, step).await {
        Ok(outcome) => return outcome,
        Err(lost) => lost,
    };
    drop(stream);
    warn!("handoff: lost the predecessor mid-handoff ({cause:#}); waiting for it to exit");
    let gone = exits_within(&predecessor_alive, grace).await;
    match (lost, gone) {
        (_, false) => {
            warn!("handoff: the predecessor is still running and keeps serving; standing down");
            Outcome::StandDown
        }
        (Lost::BeforeAck, true) => {
            warn!("handoff: the predecessor exited before committing; restoring the snapshot");
            Outcome::Restore
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
async fn exchange(stream: &UnixStream, step: Duration) -> Result<Outcome, (Lost, anyhow::Error)> {
    let before_ack = |e: anyhow::Error| (Lost::BeforeAck, e);

    let (version, token, panes) = read_hello(stream, step).await.map_err(before_ack)?;
    if version != HANDOFF_PROTOCOL_VERSION {
        return decline(stream, step, version).await.map_err(before_ack);
    }
    write_frame_within(stream, &HandoffMessage::Accept, None, step)
        .await
        .map_err(|e| before_ack(e.into()))?;

    let Some(inherited) = receive_fds(stream, step, &panes)
        .await
        .map_err(before_ack)?
    else {
        return Ok(Outcome::StandDown);
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
    acknowledge(stream, step, Inherited { token, inherited }).await
}

/// Read the predecessor's `Hello`: its protocol version, token and panes.
async fn read_hello(
    stream: &UnixStream,
    step: Duration,
) -> anyhow::Result<(u32, String, Vec<HandoffPaneMeta>)> {
    match read_frame_within(stream, step).await?.0 {
        HandoffMessage::Hello {
            version,
            token,
            panes,
        } => Ok((version, token, panes)),
        other => bail!("handoff: expected Hello, got {other:?}"),
    }
}

/// Decline a predecessor of another protocol `version`. It writes its final
/// checkpoint, then says whether it exits (`Released`: restore from that
/// checkpoint) or keeps serving (`Abort`).
async fn decline(stream: &UnixStream, step: Duration, version: u32) -> anyhow::Result<Outcome> {
    let reason = format!("predecessor handoff version {version} != {HANDOFF_PROTOCOL_VERSION}");
    warn!("handoff: {reason}; declining live migration, will snapshot-restore");
    write_frame_within(stream, &HandoffMessage::Decline { reason }, None, step).await?;
    match read_frame_within(stream, step).await?.0 {
        HandoffMessage::Released => Ok(Outcome::Restore),
        HandoffMessage::Abort { reason } => Ok(stand_down(&reason)),
        other => bail!("handoff: expected Released after Decline, got {other:?}"),
    }
}

/// Pull one fd per live pane, lock-step. `None` when the predecessor aborts.
async fn receive_fds(
    stream: &UnixStream,
    step: Duration,
    panes: &[HandoffPaneMeta],
) -> anyhow::Result<Option<HashMap<String, (OwnedFd, Pid)>>> {
    let live = panes.iter().filter(|p| p.has_live_fd).count();
    let mut inherited = HashMap::with_capacity(live);
    for _ in 0..live {
        let (msg, fd) = read_frame_within(stream, step).await?;
        let pane_id = match msg {
            HandoffMessage::PaneFd { pane_id } => pane_id,
            HandoffMessage::Abort { reason } => {
                stand_down(&reason);
                return Ok(None);
            }
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
    Ok(Some(inherited))
}

/// Send our `Ack` and learn whether the predecessor committed (`Released`)
/// or timed out waiting for it and rolled back (`Abort`).
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

/// The predecessor rolled back (`Abort`) and keeps serving.
fn stand_down(reason: &str) -> Outcome {
    warn!("handoff: the predecessor rolled back ({reason}) and keeps serving; standing down");
    Outcome::StandDown
}

/// Connect to the handoff socket, retrying briefly while the predecessor binds.
async fn connect_with_retry(path: &std::path::Path) -> Option<UnixStream> {
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
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

    /// A scripted predecessor at the far end of a socket: send `Hello` (of
    /// `version`, with one live pane when `with_pane`), then play `rest`,
    /// sending each `Some` frame and reading one frame for each `None`.
    /// Returns the frames it read.
    async fn predecessor(
        stream: UnixStream,
        version: u32,
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

    /// Run the successor side against `script`, with the predecessor's
    /// liveness answered by `alive`.
    async fn pull_against(
        version: u32,
        with_pane: bool,
        script: Vec<Option<HandoffMessage>>,
        alive: bool,
    ) -> (Outcome, Vec<HandoffMessage>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let far = tokio::spawn(predecessor(theirs, version, with_pane, script));
        let outcome = pull(ours, STEP, move || alive, PREDECESSOR_EXIT_GRACE).await;
        (outcome, far.await.unwrap())
    }

    /// A predecessor that commits hands over its live fd and its token.
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
        let (_, pid) = &inherited.inherited["eagle/0"];
        assert_eq!(pid.as_raw(), 4242);
    }

    /// A predecessor that rolls back after our `Ack` (it never got it) says
    /// so, and this daemon stands down rather than serving beside it.
    #[tokio::test]
    async fn an_abort_after_our_ack_stands_down() {
        let script = vec![
            None,
            Some(HandoffMessage::Complete),
            None,
            Some(HandoffMessage::Abort {
                reason: "no Ack".into(),
            }),
        ];
        let (outcome, _) = pull_against(HANDOFF_PROTOCOL_VERSION, false, script, true).await;
        assert!(matches!(outcome, Outcome::StandDown));
    }

    /// A predecessor of another handoff version is declined, and its
    /// `Released` (its final checkpoint is on disk) means a snapshot restore.
    #[tokio::test]
    async fn another_version_is_declined_and_restored_from_the_snapshot() {
        let script = vec![None, Some(HandoffMessage::Released)];
        let (outcome, read) = pull_against(HANDOFF_PROTOCOL_VERSION + 1, true, script, true).await;
        assert_eq!(names(&read), ["Decline"]);
        assert!(matches!(outcome, Outcome::Restore));
    }

    /// A predecessor lost before our `Ack` (here: it goes quiet after
    /// `Accept`) is waited on. Still running, it is serving, so this daemon
    /// stands down; gone, the snapshot is restored.
    #[tokio::test(start_paused = true)]
    async fn a_predecessor_lost_before_our_ack_decides_by_its_liveness() {
        let quiet = || vec![None];
        let (outcome, _) = pull_against(HANDOFF_PROTOCOL_VERSION, false, quiet(), true).await;
        assert!(matches!(outcome, Outcome::StandDown), "alive: stand down");

        let (outcome, _) = pull_against(HANDOFF_PROTOCOL_VERSION, false, quiet(), false).await;
        assert!(matches!(outcome, Outcome::Restore), "gone: restore");
    }

    /// A predecessor that exits after our `Ack` without a `Released` did
    /// commit: its live PTYs are taken over.
    #[tokio::test]
    async fn a_predecessor_gone_after_our_ack_is_taken_over() {
        let script = vec![None, Some(HandoffMessage::Complete), None];
        let (outcome, _) = pull_against(HANDOFF_PROTOCOL_VERSION, false, script, false).await;
        assert!(matches!(outcome, Outcome::Inherited(_)));
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
