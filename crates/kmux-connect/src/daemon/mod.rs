mod lifecycle;
pub(crate) use lifecycle::find_server_binary;
pub use lifecycle::{ensure_daemon, ensure_daemon_in};
pub use lifecycle::{
    force_kill_daemon, force_kill_daemon_in, pid_alive, running_daemon_pid, running_daemon_pid_in,
    wait_for_exit,
};

/// Resolve the `kmuxd` binary an auto-spawn would launch, using the same
/// precedence as the spawn path (`KMUX_KMUXD` → exe sibling → debug
/// `target/<profile>` → `$PATH`). Exposed for diagnostics (`kmux debug paths`)
/// so a developer can see *which* daemon a connect would start.
pub fn resolve_kmuxd_path() -> anyhow::Result<std::path::PathBuf> {
    find_server_binary()
}

/// The tail of the `kmuxd-boot.log`, formatted as an error suffix (or `""` when
/// empty). Exposed so `kmux daemon restart` can explain a timeout by showing
/// why a freshly-spawned daemon never came up — e.g. `No space left on device`
/// — instead of a blind "timed out" with no cause.
pub fn boot_log_hint() -> String {
    lifecycle::format_boot_log_hint()
}

use std::path::Path;
use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use kmux_protocol::compat::BuildProfile;
use kmux_protocol::control_rpc::{SessionsResponse, StatusResponse};
use kmux_protocol::messages::ProtocolRange;

/// Connection parameters returned by the running daemon.
#[derive(Debug)]
pub struct DaemonStatus {
    pub port: u16,
    pub tcp_port: u16,
    pub token: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub session_count: usize,
    pub protocol_version: u32,
    pub protocol_range: Option<ProtocolRange>,
    pub kmuxd_version: String,
    /// Build fingerprint of the running daemon, `<sha>[-dirty]` (empty when the
    /// daemon predates this field). Compared against the client/CLI build to
    /// surface skew that an overlapping protocol range alone cannot.
    pub kmuxd_build: String,
    /// `None` when the daemon predates this field — treated as
    /// unverifiable and therefore rejected by `ensure_compatible_daemon`.
    pub build_profile: Option<BuildProfile>,
}

/// The daemon control socket for this profile, resolved from the environment.
///
/// The single environment boundary of this module: every `*_at` function below
/// takes the path instead, so a test can point it at a socket in its own
/// tempdir (docs/testing.md R3).
fn control_socket() -> anyhow::Result<std::path::PathBuf> {
    kmux_sys::dirs::socket_path().map_err(|e| anyhow::anyhow!("could not resolve socket path: {e}"))
}

/// Send a single JSON command to the daemon control socket and parse the response.
///
/// Returns an error if the daemon is unreachable, times out, or the response
/// cannot be deserialized into `Resp`.
async fn control_request_at<Resp: DeserializeOwned>(
    socket_path: &Path,
    command: &str,
) -> anyhow::Result<Resp> {
    let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(socket_path))
        .await
        .map_err(|_| anyhow::anyhow!("daemon is not running (connection timed out)"))?
        .map_err(|_| anyhow::anyhow!("daemon is not running"))?;

    let (read_half, mut write_half) = stream.into_split();

    let request = format!("{{\"command\":\"{command}\"}}\n");
    write_half
        .write_all(request.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("failed to send command: {e}"))?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .map_err(|_| anyhow::anyhow!("daemon did not respond in time"))?
        .map_err(|e| anyhow::anyhow!("failed to read response: {e}"))?;

    serde_json::from_str(line.trim())
        .map_err(|e| anyhow::anyhow!("invalid response from daemon: {e}"))
}

/// Query a running daemon via its Unix control socket.
///
/// Returns `None` if the daemon is not reachable, not responding, or the PID
/// reported by the daemon is no longer alive.
pub async fn query_daemon() -> Option<DaemonStatus> {
    query_daemon_at(&control_socket().ok()?).await
}

/// [`query_daemon`] against an explicit control socket.
pub async fn query_daemon_at(socket_path: &Path) -> Option<DaemonStatus> {
    let resp: StatusResponse = control_request_at(socket_path, "status").await.ok()?;

    if !pid_alive(resp.pid) {
        return None;
    }

    Some(DaemonStatus {
        port: resp.port,
        tcp_port: resp.tcp_port,
        token: resp.token,
        pid: resp.pid,
        uptime_secs: resp.uptime_secs,
        session_count: resp.session_count,
        protocol_version: resp.protocol_version,
        protocol_range: resp.protocol_range,
        kmuxd_version: resp.kmuxd_version,
        kmuxd_build: resp.kmuxd_build,
        build_profile: resp.build_profile,
    })
}

/// Ensure a local daemon is running and that its protocol version matches ours.
///
/// Starts the daemon if it is not running, then verifies the version reported
/// via the control socket. Returns `Err` immediately when there is a version
/// mismatch — the caller must not attempt a data-plane connection.
///
/// Use this instead of `ensure_daemon()` for every connection path that talks
/// to the local daemon.
pub async fn ensure_compatible_daemon() -> anyhow::Result<DaemonStatus> {
    let status = ensure_daemon().await?;
    let socket =
        control_socket().map_or_else(|_| "<unknown>".to_string(), |p| p.display().to_string());

    match attach_refusal(&status, &socket) {
        Some(message) => Err(anyhow::anyhow!(message)),
        None => Ok(status),
    }
}

/// The attach gate as a pure function: `None` when `status` is compatible with
/// this build, otherwise the refusal message to surface.
///
/// Split out from [`ensure_compatible_daemon`] so every refusal is testable by
/// constructing a [`DaemonStatus`] — no daemon, no socket, no environment. The
/// policy itself (which mismatches block) lives in `kmux_protocol::compat`;
/// this only turns a [`compat::BlockReason`](kmux_protocol::compat::BlockReason)
/// into a hint-rich message. `socket` is only ever interpolated into the text.
fn attach_refusal(status: &DaemonStatus, socket: &str) -> Option<String> {
    use kmux_protocol::compat::{self, BlockReason};
    use kmux_protocol::messages::PROTOCOL_RANGE;

    // One attach-gate policy, defined in `kmux_protocol::compat`; each refusal
    // formats its own hint-rich message.
    Some(
        match compat::attach_block(status.protocol_range, status.build_profile)? {
            BlockReason::Protocol => {
                let daemon_range = status.protocol_range.expect("differing range is present");
                let hint = if daemon_range.max < PROTOCOL_RANGE.min {
                    "Hint: the running kmuxd is older than kmux. Run `kmux daemon restart` to update it."
                } else {
                    "Hint: the running kmuxd is newer than kmux. Update the kmux client to match."
                };
                format!(
                    "protocol version mismatch: client={}, daemon={} ({})\n{}",
                    PROTOCOL_RANGE, daemon_range, status.kmuxd_version, hint
                )
            }
            BlockReason::ProtocolUnknown => format!(
                "legacy protocol version: daemon={} ({})\nHint: restart the daemon with a current kmuxd build.",
                status.protocol_version, status.kmuxd_version,
            ),
            BlockReason::ProfileMismatch => format!(
                "build profile mismatch: kmux is {client} but the daemon answering on \
                 {socket} is {daemon}. Debug and release builds keep separate runtime \
                 dirs, so the two never share sockets — run the matching kmux binary \
                 or restart the daemon with a matching build.",
                client = BuildProfile::CURRENT,
                daemon = status
                    .build_profile
                    .map_or("<unknown>", BuildProfile::as_str),
            ),
            BlockReason::ProfileUnknown => format!(
                "daemon on {socket} did not report a build profile; refusing to attach \
                 because we cannot verify it matches kmux ({client}). Restart the \
                 daemon with a current kmuxd build.",
                client = BuildProfile::CURRENT,
            ),
        },
    )
}

/// Request a graceful shutdown by sending `stop` to the daemon control socket.
///
/// This only *asks* the daemon to shut down — the `"ok"` reply is sent before
/// the process actually exits, so it is **not** proof of termination. Callers
/// that need to confirm the daemon is gone must follow up with
/// [`wait_for_exit`] (and escalate via [`force_kill_daemon`] on timeout).
pub async fn stop_daemon() -> anyhow::Result<()> {
    stop_daemon_at(&control_socket()?).await
}

/// [`stop_daemon`] against an explicit control socket.
pub async fn stop_daemon_at(socket_path: &Path) -> anyhow::Result<()> {
    use kmux_protocol::control_rpc::StopResponse;
    let resp: StopResponse = control_request_at(socket_path, "stop").await?;
    if resp.status != "ok" {
        return Err(anyhow::anyhow!("unexpected stop response: {}", resp.status));
    }
    Ok(())
}

/// Ask the running daemon to perform a graceful live-PTY handoff to a successor.
///
/// Returns `Ok(true)` when the daemon accepted the handoff (running shells will
/// migrate), `Ok(false)` when it reports `busy`, and `Err(_)` when the daemon is
/// too old to understand `restart` (it closes the connection without replying,
/// so the response cannot be read) or is unreachable. The caller falls back to a
/// hard stop-then-respawn restart in those cases.
pub async fn restart_daemon() -> anyhow::Result<bool> {
    restart_daemon_at(&control_socket()?).await
}

/// [`restart_daemon`] against an explicit control socket.
pub async fn restart_daemon_at(socket_path: &Path) -> anyhow::Result<bool> {
    use kmux_protocol::control_rpc::RestartResponse;
    let resp: RestartResponse = control_request_at(socket_path, "restart").await?;
    Ok(resp.status == "ok")
}

/// Query the daemon for its active sessions and per-connection metrics.
pub async fn query_daemon_sessions() -> anyhow::Result<SessionsResponse> {
    query_daemon_sessions_at(&control_socket()?).await
}

/// [`query_daemon_sessions`] against an explicit control socket.
pub async fn query_daemon_sessions_at(socket_path: &Path) -> anyhow::Result<SessionsResponse> {
    control_request_at(socket_path, "sessions").await
}

/// Query the daemon for every live client connection with its build identity
/// (protocol 37). Used by `kmux client status` to find the local GUI client's
/// connection and compare its build against the daemon's.
pub async fn query_connections() -> anyhow::Result<kmux_protocol::control_rpc::ConnectionsResponse>
{
    query_connections_at(&control_socket()?).await
}

/// [`query_connections`] against an explicit control socket.
pub async fn query_connections_at(
    socket_path: &Path,
) -> anyhow::Result<kmux_protocol::control_rpc::ConnectionsResponse> {
    control_request_at(socket_path, "connections").await
}

/// Query the daemon for its isolated per-pane VT workers (issue #126). Used by
/// `kmux status` to surface each worker's pid and crash-loop history. A daemon
/// too old to know the `workers` command closes without replying, so this
/// returns `Err` and the caller degrades gracefully.
pub async fn query_workers() -> anyhow::Result<kmux_protocol::control_rpc::WorkersResponse> {
    query_workers_at(&control_socket()?).await
}

/// [`query_workers`] against an explicit control socket.
pub async fn query_workers_at(
    socket_path: &Path,
) -> anyhow::Result<kmux_protocol::control_rpc::WorkersResponse> {
    control_request_at(socket_path, "workers").await
}

#[cfg(test)]
mod tests {
    use super::*;

    use kmux_protocol::messages::{PROTOCOL_RANGE, ProtocolVersion};

    /// Every test that needs a control socket binds one inside its own
    /// `tempfile::tempdir()` and passes the path in, so the module holds no
    /// process-global state and runs fully in parallel (docs/testing.md R3/R13).
    fn socket_in(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        tmp.path().join("daemon.sock")
    }

    /// A `DaemonStatus` with everything but the two compatibility-relevant
    /// fields fixed, so an `attach_refusal` test states only what it varies.
    fn status_with(
        protocol_range: Option<ProtocolRange>,
        build_profile: Option<BuildProfile>,
    ) -> DaemonStatus {
        DaemonStatus {
            port: 9999,
            tcp_port: 0,
            token: "tok".to_string(),
            pid: std::process::id(),
            uptime_secs: 0,
            session_count: 0,
            protocol_version: 41,
            protocol_range,
            kmuxd_version: "9.9.9-test".to_string(),
            kmuxd_build: "deadbeef".to_string(),
            build_profile,
        }
    }

    #[test]
    fn attach_refusal_allows_a_matching_daemon() {
        assert_eq!(
            attach_refusal(
                &status_with(Some(PROTOCOL_RANGE), Some(BuildProfile::CURRENT)),
                "/run/kmux/daemon.sock",
            ),
            None,
            "a daemon matching this build on both axes must not be refused"
        );
    }

    /// Replaces the old test that stood up a fake daemon over a Unix socket just
    /// to reach this branch: the refusal is a pure function of the status.
    #[test]
    fn attach_refusal_reports_a_newer_daemon_with_both_versions() {
        let daemon_range = ProtocolRange::exact(ProtocolVersion::new(2, 0, 0));
        let msg = attach_refusal(
            &status_with(Some(daemon_range), Some(BuildProfile::CURRENT)),
            "/run/kmux/daemon.sock",
        )
        .expect("a differing protocol range must block the attach");

        assert!(
            msg.contains("protocol version mismatch"),
            "error should mention mismatch: {msg}"
        );
        assert!(
            msg.contains(&format!("client={PROTOCOL_RANGE}")),
            "the refusal must name the client's range: {msg}"
        );
        assert!(
            msg.contains(&format!("daemon={daemon_range}")),
            "the refusal must name the daemon's range: {msg}"
        );
        assert!(
            msg.contains("9.9.9-test"),
            "the refusal must name the daemon's version: {msg}"
        );
        assert!(
            msg.contains("Update the kmux client"),
            "a newer daemon must be fixed by updating the client: {msg}"
        );
    }

    #[test]
    fn attach_refusal_tells_an_older_daemon_to_restart_instead() {
        let daemon_range = ProtocolRange::exact(ProtocolVersion::new(0, 9, 0));
        let msg = attach_refusal(
            &status_with(Some(daemon_range), Some(BuildProfile::CURRENT)),
            "/run/kmux/daemon.sock",
        )
        .expect("a differing protocol range must block the attach");

        assert!(
            msg.contains(&format!("daemon={daemon_range}")),
            "the refusal must name the daemon's range: {msg}"
        );
        assert!(
            msg.contains("kmux daemon restart"),
            "an older daemon is fixed by restarting it, not by updating kmux: {msg}"
        );
    }

    #[test]
    fn attach_refusal_reports_a_daemon_with_no_protocol_range() {
        let msg = attach_refusal(
            &status_with(None, Some(BuildProfile::CURRENT)),
            "/run/kmux/daemon.sock",
        )
        .expect("an unverifiable protocol range must block the attach");

        assert!(
            msg.contains("legacy protocol version"),
            "error should name the legacy case: {msg}"
        );
        assert!(
            msg.contains("daemon=41"),
            "the refusal must quote the frozen protocol_version it did report: {msg}"
        );
        assert!(msg.contains("9.9.9-test"), "{msg}");
    }

    /// Replaces the old fake-daemon test for the profile gate. The daemon
    /// profile is flipped so the assertion is profile-agnostic.
    #[test]
    fn attach_refusal_reports_build_profile_mismatch_naming_both_sides() {
        let wrong_profile = match BuildProfile::CURRENT {
            BuildProfile::Debug => BuildProfile::Release,
            BuildProfile::Release => BuildProfile::Debug,
        };
        let msg = attach_refusal(
            &status_with(Some(PROTOCOL_RANGE), Some(wrong_profile)),
            "/run/kmux/daemon.sock",
        )
        .expect("a mismatched build profile must block the attach");

        assert!(
            msg.contains("build profile mismatch"),
            "error should mention build profile mismatch: {msg}"
        );
        assert!(
            msg.contains(&format!("kmux is {}", BuildProfile::CURRENT)),
            "the refusal must name the client's profile: {msg}"
        );
        assert!(
            msg.contains(wrong_profile.as_str()),
            "the refusal must name the daemon's profile: {msg}"
        );
        assert!(
            msg.contains("/run/kmux/daemon.sock"),
            "the refusal must name the socket that answered: {msg}"
        );
    }

    #[test]
    fn attach_refusal_reports_a_daemon_with_no_build_profile() {
        let msg = attach_refusal(
            &status_with(Some(PROTOCOL_RANGE), None),
            "/run/kmux/daemon.sock",
        )
        .expect("an unverifiable build profile must block the attach");

        assert!(
            msg.contains("did not report a build profile"),
            "error should name the unverifiable case: {msg}"
        );
        assert!(
            msg.contains(&BuildProfile::CURRENT.to_string()),
            "the refusal must name the client's profile: {msg}"
        );
        assert!(
            msg.contains("/run/kmux/daemon.sock"),
            "the refusal must name the socket that answered: {msg}"
        );
    }

    #[tokio::test]
    async fn query_daemon_parses_session_count() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().expect("tempdir");
        let socket_path = socket_in(&tmp);
        let listener = UnixListener::bind(&socket_path).expect("bind control socket");

        let my_pid = std::process::id();

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                let _ = reader.read_line(&mut line).await;
                let response = format!(
                    "{{\"status\":\"running\",\"port\":9999,\"token\":\"tok\",\
                     \"pid\":{my_pid},\"uptime_secs\":42,\"session_count\":3,\
                     \"protocol_version\":41,\"protocol_range\":{{\"min\":{{\"major\":1,\"minor\":0,\"patch\":0}},\"max\":{{\"major\":1,\"minor\":0,\"patch\":0}}}},\"kmuxd_version\":\"0.0.0\"}}\n"
                );
                write_half.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let status = query_daemon_at(&socket_path).await;
        let status = status.expect("expected Some from mock daemon");
        assert_eq!(status.port, 9999);
        assert_eq!(status.uptime_secs, 42);
        assert_eq!(status.session_count, 3);
    }

    #[tokio::test]
    async fn query_daemon_sessions_roundtrip() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().expect("tempdir");
        let socket_path = socket_in(&tmp);
        let listener = UnixListener::bind(&socket_path).expect("bind control socket");

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                let _ = reader.read_line(&mut line).await;
                // Verify the command name was forwarded correctly.
                assert!(line.contains("\"sessions\""));
                let response = r#"{"sessions":[],"unattached":[]}"#.to_string() + "\n";
                write_half.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let resp = query_daemon_sessions_at(&socket_path)
            .await
            .expect("should parse");
        assert!(resp.sessions.is_empty());
        assert!(resp.unattached.is_empty());
    }

    #[tokio::test]
    async fn control_request_timeout_surfaces_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // No listener — should return an error, not hang.
        let result: anyhow::Result<StatusResponse> =
            control_request_at(&socket_in(&tmp), "status").await;
        let Err(error) = result else {
            panic!("an absent daemon must surface as Err");
        };
        assert!(
            error.to_string().contains("daemon is not running"),
            "the error must say why the request failed: {error}"
        );
    }

    /// The `restart` control RPC has three outcomes the `kmux daemon restart`
    /// command branches on (see `kmux-app/src/subcommands/daemon_cmd.rs`):
    ///   - `{"status":"ok"}`   → `Ok(true)`  — graceful handoff accepted
    ///   - `{"status":"busy"}` → `Ok(false)` — a restart is already in progress
    ///   - connection closed without a reply → `Err` — daemon predates `restart`,
    ///     so the caller falls back to a hard stop-then-respawn.
    #[tokio::test]
    async fn restart_daemon_maps_accepted_busy_and_unsupported() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().expect("tempdir");
        let socket_path = socket_in(&tmp);
        let listener = UnixListener::bind(&socket_path).expect("bind control socket");

        // Serve three connections in order: accept, busy, then close-without-reply.
        tokio::spawn(async move {
            let replies: [Option<&str>; 3] = [
                Some(r#"{"status":"ok","handoff":true}"#),
                Some(r#"{"status":"busy","handoff":false}"#),
                None, // mimic an old daemon: close without replying
            ];
            for reply in replies {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                let _ = reader.read_line(&mut line).await;
                assert!(line.contains("\"restart\""), "expected a restart command");
                if let Some(body) = reply {
                    let _ = write_half.write_all(format!("{body}\n").as_bytes()).await;
                }
                // Dropping `write_half` (reply == None) closes the connection so the
                // client reads EOF and surfaces an error.
            }
        });

        assert!(
            restart_daemon_at(&socket_path)
                .await
                .expect("accepted reply parses"),
            "status=ok must report an accepted handoff"
        );
        assert!(
            !restart_daemon_at(&socket_path)
                .await
                .expect("busy reply parses"),
            "status=busy must report no handoff"
        );
        assert!(
            restart_daemon_at(&socket_path).await.is_err(),
            "a daemon that closes without replying must surface as Err (unsupported)"
        );
    }
}
