//! Incoming-daemon (N) side of a graceful handoff: connect to the predecessor,
//! pull each live PTY master fd, and report them back to startup.
//!
//! The actual relay reconstruction happens afterwards via
//! [`crate::app::ServerApp::restore_with_handoff`], keyed by `pane_id` against
//! the on-disk checkpoint. This module only performs the protocol exchange.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::time::Duration;

use anyhow::{anyhow, bail};
use kmux_protocol::control_rpc::{HANDOFF_PROTOCOL_VERSION, HandoffMessage};
use nix::unistd::Pid;
use tokio::net::UnixStream;
use tracing::{info, warn};

use super::{read_frame, write_frame};

/// Result of a successful handoff pull.
pub struct Outcome {
    /// The predecessor's auth token, adopted so already-connected clients can
    /// re-authenticate without a credential rotation.
    pub token: String,
    /// Live PTY master fds keyed by `pane_id`, to be adopted by
    /// `restore_with_handoff`. Panes absent here are respawned from the snapshot.
    pub inherited: HashMap<String, (OwnedFd, Pid)>,
}

/// How long to keep retrying the initial connect: the predecessor may not have
/// bound the handoff socket yet when we start.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Pull live PTY fds from the predecessor daemon. Returns `Ok(None)` if no
/// predecessor is reachable or it declines/mismatches — the caller then falls
/// back to a normal snapshot restore.
pub async fn run() -> anyhow::Result<Option<Outcome>> {
    let path = kmux_protocol::dirs::handoff_socket_path()?;
    let Some(stream) = connect_with_retry(&path).await else {
        warn!("handoff: no predecessor handoff socket; falling back to snapshot restore");
        return Ok(None);
    };

    // Handshake: read Hello, verify the protocol version.
    let (hello, _) = read_frame(&stream).await?;
    let (version, token, panes) = match hello {
        HandoffMessage::Hello {
            version,
            token,
            panes,
        } => (version, token, panes),
        other => bail!("handoff: expected Hello, got {other:?}"),
    };

    if version != HANDOFF_PROTOCOL_VERSION {
        let reason = format!("predecessor handoff version {version} != {HANDOFF_PROTOCOL_VERSION}");
        warn!("handoff: {reason}; declining live migration, will snapshot-restore");
        let _ = write_frame(&stream, &HandoffMessage::Decline { reason }, None).await;
        return Ok(None);
    }

    write_frame(&stream, &HandoffMessage::Accept, None).await?;

    // Pull one fd per live pane, lock-step.
    let live = panes.iter().filter(|p| p.has_live_fd).count();
    let mut inherited: HashMap<String, (OwnedFd, Pid)> = HashMap::with_capacity(live);
    for _ in 0..live {
        let (msg, fd) = read_frame(&stream).await?;
        let pane_id = match msg {
            HandoffMessage::PaneFd { pane_id } => pane_id,
            other => bail!("handoff: expected PaneFd, got {other:?}"),
        };
        let fd = fd.ok_or_else(|| anyhow!("handoff: PaneFd for {pane_id} carried no fd"))?;
        let pid = panes
            .iter()
            .find(|p| p.pane_id == pane_id)
            .map_or(0, |p| p.pid);
        inherited.insert(pane_id, (fd, Pid::from_raw(pid)));
        write_frame(&stream, &HandoffMessage::PaneFdAck, None).await?;
    }

    match read_frame(&stream).await?.0 {
        HandoffMessage::Complete => {}
        other => bail!("handoff: expected Complete, got {other:?}"),
    }

    // Commit: we hold every live fd. From here the predecessor is dispensable.
    write_frame(&stream, &HandoffMessage::Ack, None).await?;

    // Wait for the predecessor to release its sockets before we bind them. If it
    // died instead of sending Released, that's fine — we already have everything.
    match read_frame(&stream).await {
        Ok((HandoffMessage::Released, _)) => {}
        Ok((other, _)) => warn!("handoff: unexpected post-Ack frame {other:?}; proceeding"),
        Err(e) => info!("handoff: predecessor closed before Released ({e}); proceeding"),
    }

    info!(
        inherited = inherited.len(),
        "handoff: pulled live PTYs from predecessor"
    );
    Ok(Some(Outcome { token, inherited }))
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
