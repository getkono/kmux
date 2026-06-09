//! Outgoing-daemon (O) side of a graceful handoff: spawn the successor, stream
//! each live PTY master fd to it, then quiesce, checkpoint, and exit.
//!
//! `run` returns `Ok(())` once the handoff has *committed* (the successor holds
//! every live fd, or explicitly declined and will snapshot-restore). The caller
//! then tears down its listeners and exits. On `Err` nothing destructive has
//! happened — the relays were never stopped and keep-alive was never set — so the
//! caller simply resumes serving (a rollback).

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use kmux_protocol::control_rpc::{DAEMON_BOOT_ARGS, HANDOFF_PROTOCOL_VERSION, HandoffMessage};
use tokio::net::UnixListener;
use tracing::{info, warn};

use crate::app::ServerApp;

use super::{PathGuard, read_frame, write_frame};

/// Maximum time to wait for the successor daemon to connect to the handoff
/// socket. It must start, daemonize, restore-read, and connect within this.
const SUCCESSOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Drive a graceful handoff to a freshly-spawned successor daemon.
///
/// On `Ok(())` the handoff committed and `app` must not serve further (the
/// caller releases sockets and exits). On `Err(_)` the handoff failed before the
/// commit point and the daemon should resume normal operation.
pub async fn run(app: &Arc<ServerApp>) -> anyhow::Result<()> {
    let path = kmux_protocol::dirs::handoff_socket_path()?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding handoff socket {}", path.display()))?;
    let _guard = PathGuard(path.clone());

    spawn_successor().context("spawning successor daemon")?;

    let (stream, _) = tokio::time::timeout(SUCCESSOR_CONNECT_TIMEOUT, listener.accept())
        .await
        .map_err(|_| anyhow!("successor did not connect within {SUCCESSOR_CONNECT_TIMEOUT:?}"))?
        .context("accepting successor handoff connection")?;

    let panes = app.collect_handoff_panes().await;
    let live = panes.iter().filter(|p| p.has_live_fd).count();
    info!(
        total = panes.len(),
        live, "handoff: advertising panes to successor"
    );

    write_frame(
        &stream,
        &HandoffMessage::Hello {
            version: HANDOFF_PROTOCOL_VERSION,
            token: app.auth_token.clone(),
            panes: panes.clone(),
        },
        None,
    )
    .await?;

    match read_frame(&stream).await?.0 {
        HandoffMessage::Accept => {}
        HandoffMessage::Decline { reason } => {
            warn!(
                "handoff: successor declined live migration ({reason}); it will snapshot-restore"
            );
            // The successor will respawn from the checkpoint, so make sure a
            // fresh one is on disk. Children are NOT kept alive — this degrades
            // to today's restart behavior.
            write_checkpoint(app).await?;
            let _ = write_frame(&stream, &HandoffMessage::Released, None).await;
            return Ok(());
        }
        other => bail!("handoff: expected Accept/Decline, got {other:?}"),
    }

    // Stream each live fd, lock-step (one in flight at a time), so each frame is
    // delivered to the successor with exactly its own fd.
    for meta in panes.iter().filter(|p| p.has_live_fd) {
        let fd = app
            .manager
            .dup_master_fd(&meta.pane_id)
            .await
            .with_context(|| format!("duplicating master fd for {}", meta.pane_id))?;
        write_frame(
            &stream,
            &HandoffMessage::PaneFd {
                pane_id: meta.pane_id.clone(),
            },
            Some(fd.as_raw_fd()),
        )
        .await?;
        // Our dup has been copied into the successor; close ours. The child stays
        // alive: the successor holds a dup, and our original master fds are still
        // open until we quiesce and exit below.
        drop(fd);
        match read_frame(&stream).await?.0 {
            HandoffMessage::PaneFdAck => {}
            other => bail!(
                "handoff: expected PaneFdAck for {}, got {other:?}",
                meta.pane_id
            ),
        }
    }

    write_frame(&stream, &HandoffMessage::Complete, None).await?;

    // Commit point: the successor confirms it holds every live fd. Past here we
    // are irrevocably handing off — failures no longer roll back.
    match read_frame(&stream).await?.0 {
        HandoffMessage::Ack => {}
        other => bail!("handoff: expected Ack, got {other:?}"),
    }

    // Suppress SIGKILL on our PTY children, stop reading their masters, and
    // snapshot the (now-frozen) emulator state so the successor seeds from
    // exactly what we consumed; anything after sits in the kernel buffer for it.
    app.manager.set_all_keep_alive(true).await;
    app.quiesce_relays().await;
    write_checkpoint(app).await?;

    // Tell the successor it may bind the control/data sockets; then we exit.
    let _ = write_frame(&stream, &HandoffMessage::Released, None).await;
    info!(live, "handoff: committed; releasing sockets and exiting");
    Ok(())
}

/// Spawn the successor daemon: re-exec this binary with the standard boot args
/// plus `--handoff`. After an in-place upgrade `current_exe()` resolves to the
/// new binary at the same path.
fn spawn_successor() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let mut args: Vec<&str> = DAEMON_BOOT_ARGS.to_vec();
    args.push("--handoff");
    std::process::Command::new(&exe)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;
    Ok(())
}

/// Write a fresh checkpoint to the standard session-state path.
async fn write_checkpoint(app: &ServerApp) -> anyhow::Result<()> {
    let state = app.checkpoint_state().await;
    let path = kmux_protocol::dirs::session_state_path()?;
    crate::persist::checkpoint::write_checkpoint(&state, &path)?;
    Ok(())
}
