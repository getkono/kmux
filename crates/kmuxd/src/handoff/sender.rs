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

/// Write a fresh checkpoint to the standard session-state path.
async fn write_checkpoint(app: &ServerApp) -> anyhow::Result<()> {
    let state = app.checkpoint_state().await;
    let path = kmux_sys::dirs::session_state_path()?;
    crate::persist::checkpoint::write_checkpoint(&state, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::resolve_successor_exe;

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
