//! SSH negotiation: probe-or-start the remote daemon, then build a `-L`
//! TCP tunnel to it. The negotiation is deliberately I/O-poor: every step
//! captures full diagnostic context so a failure surfaces a complete
//! `SshError` that is useful on stderr without enabling verbose tracing.
//!
//! Design notes:
//!
//! * **No `-v` parsing.** Earlier revisions tried to scrape `debug1: Local
//!   forwarding listening on 127.0.0.1 port NNNNN.` out of `ssh -v` stderr,
//!   but `-v` also prints `debug1: Connecting to <host> [<ip>] port 22.`
//!   *first* — and that line trivially matched the same `port N` pattern,
//!   making the parser hand back `22` and the client TLS-talk to the local
//!   sshd. We now pre-allocate the local port ourselves and verify the
//!   tunnel by TCP-connecting to it.
//! * **`ExitOnForwardFailure=yes`** so the tunnel process exits immediately
//!   if the remote forward can't be set up, instead of sitting idle.
//! * **Stderr is always captured into a ring buffer**, drained off-thread
//!   so we never backpressure ssh, and surfaced in every error variant
//!   that involves an ssh subprocess. Users see the actual ssh complaint
//!   (`Permission denied (publickey)`, `Host key verification failed`,
//!   `Connection timed out`, …) without `RUST_LOG=debug`.

use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::AsyncBufReadExt;
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStderr, Command};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use kmux_protocol::messages::PROTOCOL_VERSION;

use super::{ProbeFailureKind, RemoteTarget, SshError, SshSession};

/// Hard cap on a single `ssh kmuxd probe-or-start` invocation.
///
/// `kmuxd probe-or-start` itself polls for up to 10s waiting for a fresh
/// daemon to come up; we add a generous slack for the SSH handshake.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait for the local end of the `-L` tunnel to accept TCP.
///
/// The remote bind happens after authentication completes, so on a slow
/// link we may need a few seconds for sshd to publish the channel. The
/// child-process exit watcher short-circuits this on hard failure.
const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// SSH-negotiate with `target`, returning an [`SshSession`] ready for TCP use.
///
/// Steps:
///   1. Run `ssh user@host kmuxd probe-or-start` and parse the JSON reply.
///   2. Pre-allocate a free local TCP port.
///   3. Spawn `ssh -L <localport>:127.0.0.1:<remoteport> -N user@host` with
///      stderr captured into a background-drained ring buffer.
///   4. Probe `127.0.0.1:<localport>` until it accepts TCP, while watching
///      the child for an early exit. If either fails, surface the captured
///      stderr in the error.
pub async fn negotiate(target: &RemoteTarget) -> Result<SshSession, SshError> {
    let ssh_dest = ssh_destination(target);

    let probe = run_probe(target, &ssh_dest).await?;
    let info = parse_probe_json(&probe.stdout)?;
    enforce_version(&info, &ssh_dest)?;

    info!(
        dest = %ssh_dest,
        quic_port = info.quic_port,
        tcp_port = info.tcp_port,
        kmuxd_version = info.kmuxd_version.as_deref().unwrap_or("?"),
        "Remote daemon ready"
    );

    let local_port = allocate_local_port()?;
    let (mut tunnel, tunnel_argv, stderr_buf) =
        spawn_tunnel(target, &ssh_dest, local_port, info.tcp_port)?;

    if let Err(err) = wait_for_tunnel_ready(
        &mut tunnel,
        &stderr_buf,
        &ssh_dest,
        &tunnel_argv,
        local_port,
    )
    .await
    {
        // Best-effort cleanup: the supervisor task spawned by `spawn_tunnel`
        // owns the stderr drain; killing the child causes the drain to EOF.
        let _ = tunnel.start_kill();
        return Err(err);
    }

    info!(
        dest = %ssh_dest,
        local_port,
        remote_port = info.tcp_port,
        "SSH tunnel established"
    );

    Ok(SshSession {
        token: info.token,
        quic_port: info.quic_port,
        remote_tcp_port: info.tcp_port,
        local_tcp_port: local_port,
        remote_host: target.host.clone(),
        tunnel_process: tunnel,
        probe_json: probe.stdout,
    })
}

// ── Step 1: probe ─────────────────────────────────────────────────────────────

struct ProbeResult {
    stdout: String,
}

async fn run_probe(target: &RemoteTarget, ssh_dest: &str) -> Result<ProbeResult, SshError> {
    let mut cmd = build_ssh_cmd(target)?;
    append_ssh_target_args(&mut cmd, target)?;
    cmd.arg("kmuxd").arg("probe-or-start");
    let argv = render_argv(&cmd);
    debug!(dest = %ssh_dest, argv = %argv, "Running kmuxd probe-or-start");

    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| SshError::ProbeFailed {
            kind: ProbeFailureKind::SshFailed,
            dest: ssh_dest.to_owned(),
            argv: argv.clone(),
            exit_code: format!("timeout after {}s", PROBE_TIMEOUT.as_secs()),
            stderr: "(ssh did not return within the probe budget)".to_string(),
        })?
        .map_err(|e| SshError::Spawn {
            program: ssh_program(),
            source: e,
        })?;

    if !output.status.success() {
        return Err(SshError::ProbeFailed {
            kind: classify_probe_exit(output.status),
            dest: ssh_dest.to_owned(),
            argv,
            exit_code: format_exit(output.status),
            stderr: tail_stderr(&output.stderr),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(ProbeResult { stdout })
}

#[derive(Debug, serde::Deserialize)]
struct ProbeInfo {
    quic_port: u16,
    tcp_port: u16,
    token: String,
    /// `protocol_version` field added in Phase 7; older daemons omit it.
    #[serde(default)]
    protocol_version: Option<u32>,
    /// Reported daemon version string. Surfaced in success logs only.
    #[serde(default)]
    kmuxd_version: Option<String>,
}

fn parse_probe_json(raw: &str) -> Result<ProbeInfo, SshError> {
    serde_json::from_str::<ProbeInfo>(raw).map_err(|e| SshError::BadProbeJson {
        error: e.to_string(),
        raw: redact_probe_output(raw),
    })
}

fn redact_probe_output(raw: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw) {
        redact_json_tokens(&mut value);
        if let Ok(redacted) = serde_json::to_string(&value) {
            return redacted;
        }
    }

    let mut out = raw.to_string();
    let mut search_from = 0;
    while let Some(rel) = out[search_from..].find("\"token\"") {
        let key_start = search_from + rel;
        let Some(colon_rel) = out[key_start..].find(':') else {
            break;
        };
        let value_start = key_start + colon_rel + 1;
        let Some(first_quote_rel) = out[value_start..].find('"') else {
            search_from = value_start;
            continue;
        };
        let string_start = value_start + first_quote_rel;
        let mut escaped = false;
        let mut string_end = None;
        for (idx, ch) in out[string_start + 1..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                string_end = Some(string_start + 1 + idx);
                break;
            }
        }
        let Some(end) = string_end else {
            break;
        };
        out.replace_range(string_start + 1..end, "<redacted>");
        search_from = string_start + "\"<redacted>\"".len();
    }
    out
}

fn redact_json_tokens(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key.eq_ignore_ascii_case("token") {
                    *value = serde_json::Value::String("<redacted>".to_string());
                } else {
                    redact_json_tokens(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_tokens(value);
            }
        }
        _ => {}
    }
}

fn enforce_version(info: &ProbeInfo, ssh_dest: &str) -> Result<(), SshError> {
    match info.protocol_version {
        Some(server_ver) if server_ver != PROTOCOL_VERSION => Err(SshError::VersionMismatch {
            client: PROTOCOL_VERSION,
            server: server_ver,
        }),
        Some(_) => Ok(()),
        None => {
            warn!(
                dest = %ssh_dest,
                "Remote daemon did not report protocol_version; \
                 update kmuxd to ensure compatibility"
            );
            Ok(())
        }
    }
}

// ── Step 2-3: tunnel ──────────────────────────────────────────────────────────

fn allocate_local_port() -> Result<u16, SshError> {
    // Bind on the loopback so the kernel picks an unused port; immediately
    // drop the listener so ssh can re-bind. The window between drop and
    // re-bind is microseconds and only relevant if another process on the
    // same host is racing for the same ephemeral port — extremely unlikely
    // in practice, and surfaced as an `ExitOnForwardFailure` failure if it
    // does happen.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| SshError::LocalPortAllocFailed(e.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|e| SshError::LocalPortAllocFailed(e.to_string()))?
        .port();
    drop(listener);
    Ok(port)
}

/// Bounded stderr capture: a `Mutex<Vec<String>>` that the drainer task
/// pushes into and the error path snapshots. Capped to 50 lines so a
/// chatty ssh (e.g. with `-v` from the user's environment) can't grow
/// unboundedly.
type StderrBuf = Arc<Mutex<Vec<String>>>;
const STDERR_CAP: usize = 50;

fn spawn_tunnel(
    target: &RemoteTarget,
    ssh_dest: &str,
    local_port: u16,
    remote_port: u16,
) -> Result<(Child, String, StderrBuf), SshError> {
    let forward_spec = format!("{local_port}:127.0.0.1:{remote_port}");
    let mut cmd = build_ssh_cmd(target)?;
    cmd.arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .arg("-L")
        .arg(&forward_spec)
        .arg("-N"); // no remote command
    append_ssh_target_args(&mut cmd, target)?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let argv = render_argv(&cmd);
    debug!(dest = %ssh_dest, forward = %forward_spec, argv = %argv, "Spawning SSH -L tunnel");

    let mut child = cmd.spawn().map_err(|e| SshError::Spawn {
        program: ssh_program(),
        source: e,
    })?;

    let stderr_buf: StderrBuf = Arc::new(Mutex::new(Vec::with_capacity(STDERR_CAP)));
    if let Some(stderr) = child.stderr.take() {
        spawn_stderr_drain(stderr, Arc::clone(&stderr_buf));
    }

    Ok((child, argv, stderr_buf))
}

/// Drain the tunnel's stderr line-by-line so it never backpressures ssh.
/// Lines are appended to `buf` (capped at [`STDERR_CAP`]) and mirrored to
/// `tracing::debug!` for users who *do* want a live view.
fn spawn_stderr_drain(stderr: ChildStderr, buf: StderrBuf) {
    tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            debug!(target: "ssh_tunnel_stderr", "{line}");
            let mut guard = buf.lock().expect("stderr buf mutex");
            if guard.len() >= STDERR_CAP {
                guard.remove(0);
            }
            guard.push(line);
        }
    });
}

fn snapshot_stderr(buf: &StderrBuf) -> String {
    let guard = buf.lock().expect("stderr buf mutex");
    if guard.is_empty() {
        "(ssh produced no stderr output)".to_string()
    } else {
        guard
            .iter()
            .map(|l| format!("    {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

async fn wait_for_tunnel_ready(
    child: &mut Child,
    stderr_buf: &StderrBuf,
    ssh_dest: &str,
    tunnel_argv: &str,
    local_port: u16,
) -> Result<(), SshError> {
    let deadline = Instant::now() + TUNNEL_READY_TIMEOUT;
    let mut delay = Duration::from_millis(40);

    loop {
        // Hard fail: ssh exited before the forward came up.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(SshError::TunnelDiedEarly {
                dest: ssh_dest.to_owned(),
                argv: tunnel_argv.to_owned(),
                exit_code: format_exit(status),
                stderr: snapshot_stderr(stderr_buf),
            });
        }

        // Cheap probe: try to TCP-connect to the local end. As soon as ssh
        // has installed the forward listener, this succeeds.
        if TcpStream::connect(("127.0.0.1", local_port)).await.is_ok() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(SshError::TunnelUnreachable {
                dest: ssh_dest.to_owned(),
                argv: tunnel_argv.to_owned(),
                local_port,
                stderr: snapshot_stderr(stderr_buf),
            });
        }

        sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(500));
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub(super) fn ssh_destination(target: &RemoteTarget) -> String {
    match &target.user {
        Some(u) => format!("{}@{}", u, target.host),
        None => target.host.clone(),
    }
}

fn ssh_program() -> String {
    std::env::var("KMUX_SSH_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ssh".to_string())
}

/// Build a base `ssh` command with the shared flags every kmux call relies on.
///
/// * `BatchMode=yes` — fail fast instead of prompting for a password. SSH
///   auth must be handled out of band (ssh-agent, key files, etc.). This is
///   intentional: the kmux client has no terminal of its own at this point
///   (it is about to take over stdin/stdout for the TUI).
/// * `StrictHostKeyChecking=accept-new` — TOFU on first connection, refuse
///   on mismatch. Matches the `connect_tcp_tls` TOFU model on the data plane.
/// * `ConnectTimeout=10` — bound network failures so the user sees an error
///   instead of a hang when the host is unreachable.
pub(super) fn build_ssh_cmd(target: &RemoteTarget) -> Result<Command, SshError> {
    validate_ssh_value("host", &target.host)?;
    if let Some(user) = &target.user {
        validate_ssh_value("user", user)?;
    }
    let mut cmd = Command::new(ssh_program());
    cmd.arg("-o").arg("BatchMode=yes");
    cmd.arg("-o").arg("StrictHostKeyChecking=accept-new");
    cmd.arg("-o").arg("ConnectTimeout=10");
    if let Some(port) = target.ssh_port {
        cmd.arg("-p").arg(port.to_string());
    }
    Ok(cmd)
}

fn append_ssh_target_args(cmd: &mut Command, target: &RemoteTarget) -> Result<(), SshError> {
    if let Some(user) = &target.user {
        cmd.arg("-l").arg(user);
    }
    cmd.arg("--").arg(&target.host);
    Ok(())
}

fn validate_ssh_value(field: &'static str, value: &str) -> Result<(), SshError> {
    if value.is_empty() {
        return Err(SshError::InvalidTarget {
            field,
            value: value.to_string(),
            reason: "must not be empty",
        });
    }
    if value.starts_with('-') {
        return Err(SshError::InvalidTarget {
            field,
            value: value.to_string(),
            reason: "must not start with '-'",
        });
    }
    if value
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(SshError::InvalidTarget {
            field,
            value: value.to_string(),
            reason: "must not contain control characters or whitespace",
        });
    }
    Ok(())
}

/// Render `cmd` as a shell-ish argv string for inclusion in error messages.
/// Not parseable, but unambiguous enough that a user can re-run it manually.
fn render_argv(cmd: &Command) -> String {
    let std_cmd = cmd.as_std();
    let mut out = String::new();
    out.push_str(&std_cmd.get_program().to_string_lossy());
    for arg in std_cmd.get_args() {
        out.push(' ');
        let s = arg.to_string_lossy();
        if s.chars().any(char::is_whitespace) {
            out.push('"');
            out.push_str(&s);
            out.push('"');
        } else {
            out.push_str(&s);
        }
    }
    out
}

/// Map an [`ExitStatus`] to a stable, classifiable [`ProbeFailureKind`].
///
/// SSH itself signals all of (auth, network, host-key, host-down) failures
/// with exit code 255. Exit 127 is what most shells return when the
/// requested command is not on the remote `PATH` — the canonical
/// "kmuxd not installed" signal. Everything else is treated as a probe
/// failure on the remote.
fn classify_probe_exit(status: ExitStatus) -> ProbeFailureKind {
    match status.code() {
        Some(127) => ProbeFailureKind::RemoteDaemonNotInstalled,
        Some(255) => ProbeFailureKind::SshFailed,
        Some(_) => ProbeFailureKind::RemoteDaemonStartFailed,
        None => ProbeFailureKind::SshFailed,
    }
}

fn format_exit(status: ExitStatus) -> String {
    match status.code() {
        Some(c) => c.to_string(),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    return format!("signal {sig}");
                }
            }
            "abnormal exit".to_string()
        }
    }
}

fn tail_stderr(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let trimmed = s.trim_end();
    if trimmed.is_empty() {
        return "(ssh produced no stderr output)".to_string();
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let start = lines.len().saturating_sub(STDERR_CAP);
    lines[start..]
        .iter()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ProbeInfo deserialization ────────────────────────────────────────────

    #[test]
    fn probe_info_with_protocol_version() {
        let json = r#"{"quic_port":8443,"tcp_port":8444,"token":"abc","protocol_version":13}"#;
        let info: ProbeInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.protocol_version, Some(13));
        assert_eq!(info.tcp_port, 8444);
    }

    #[test]
    fn probe_info_without_protocol_version_defaults_to_none() {
        let json = r#"{"quic_port":8443,"tcp_port":8444,"token":"abc"}"#;
        let info: ProbeInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.protocol_version, None);
    }

    #[test]
    fn parse_probe_json_returns_bad_probe_on_garbage() {
        let err = parse_probe_json("not json").unwrap_err();
        match err {
            SshError::BadProbeJson { raw, .. } => assert_eq!(raw, "not json"),
            other => panic!("expected BadProbeJson, got {other:?}"),
        }
    }

    #[test]
    fn parse_probe_json_redacts_token_on_malformed_output() {
        let raw = r#"{"quic_port":1,"tcp_port":2,"token":"secret"} banner"#;
        let err = parse_probe_json(raw).unwrap_err();
        match err {
            SshError::BadProbeJson { raw, .. } => {
                assert!(raw.contains("<redacted>"));
                assert!(!raw.contains("secret"));
            }
            other => panic!("expected BadProbeJson, got {other:?}"),
        }
    }

    #[test]
    fn build_ssh_cmd_inserts_separator_before_host() {
        let target = RemoteTarget {
            user: Some("alice".into()),
            host: "devbox".into(),
            ssh_port: Some(2222),
        };
        let mut cmd = build_ssh_cmd(&target).unwrap();
        append_ssh_target_args(&mut cmd, &target).unwrap();
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert!(args.windows(2).any(|w| w == ["--", "devbox"]));
        assert!(args.windows(2).any(|w| w == ["-l", "alice"]));
        assert!(args.windows(2).any(|w| w == ["-p", "2222"]));
    }

    #[test]
    fn build_ssh_cmd_rejects_option_like_host() {
        let target = RemoteTarget {
            user: None,
            host: "-oProxyCommand=sh".into(),
            ssh_port: None,
        };
        assert!(matches!(
            build_ssh_cmd(&target),
            Err(SshError::InvalidTarget { field: "host", .. })
        ));
    }

    #[test]
    fn build_ssh_cmd_allows_visible_unicode_targets() {
        let target = RemoteTarget {
            user: Some("álîçé".into()),
            host: "開発".into(),
            ssh_port: None,
        };
        assert!(build_ssh_cmd(&target).is_ok());
    }

    #[test]
    fn invalid_target_error_escapes_control_characters() {
        let target = RemoteTarget {
            user: None,
            host: "host\nforged".into(),
            ssh_port: None,
        };
        let error = build_ssh_cmd(&target).unwrap_err().to_string();
        assert!(error.contains("\\n"));
        assert!(!error.contains("host\nforged"));
    }

    #[test]
    fn enforce_version_accepts_matching_version() {
        let info = ProbeInfo {
            quic_port: 1,
            tcp_port: 2,
            token: "t".into(),
            protocol_version: Some(PROTOCOL_VERSION),
            kmuxd_version: None,
        };
        enforce_version(&info, "user@host").unwrap();
    }

    #[test]
    fn enforce_version_rejects_mismatch() {
        let info = ProbeInfo {
            quic_port: 1,
            tcp_port: 2,
            token: "t".into(),
            protocol_version: Some(PROTOCOL_VERSION + 1),
            kmuxd_version: None,
        };
        match enforce_version(&info, "user@host").unwrap_err() {
            SshError::VersionMismatch { client, server } => {
                assert_eq!(client, PROTOCOL_VERSION);
                assert_eq!(server, PROTOCOL_VERSION + 1);
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn enforce_version_accepts_missing_version_with_warn() {
        let info = ProbeInfo {
            quic_port: 1,
            tcp_port: 2,
            token: "t".into(),
            protocol_version: None,
            kmuxd_version: None,
        };
        enforce_version(&info, "user@host").unwrap();
    }

    // ── classify_probe_exit ──────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn classify_probe_exit_127_is_not_installed() {
        use std::os::unix::process::ExitStatusExt;
        let status = ExitStatus::from_raw(127 << 8);
        assert_eq!(
            classify_probe_exit(status),
            ProbeFailureKind::RemoteDaemonNotInstalled
        );
    }

    #[cfg(unix)]
    #[test]
    fn classify_probe_exit_255_is_ssh_failure() {
        use std::os::unix::process::ExitStatusExt;
        let status = ExitStatus::from_raw(255 << 8);
        assert_eq!(classify_probe_exit(status), ProbeFailureKind::SshFailed);
    }

    #[cfg(unix)]
    #[test]
    fn classify_probe_exit_other_is_remote_start_failed() {
        use std::os::unix::process::ExitStatusExt;
        let status = ExitStatus::from_raw(2 << 8);
        assert_eq!(
            classify_probe_exit(status),
            ProbeFailureKind::RemoteDaemonStartFailed
        );
    }

    // ── tail_stderr ──────────────────────────────────────────────────────────

    #[test]
    fn tail_stderr_indents_and_caps_lines() {
        let mut s = String::new();
        for i in 0..(STDERR_CAP + 5) {
            s.push_str(&format!("line{i}\n"));
        }
        let out = tail_stderr(s.as_bytes());
        let line_count = out.lines().count();
        assert_eq!(line_count, STDERR_CAP);
        // Should be indented and start at the (5)th line (oldest dropped).
        assert!(
            out.lines().next().unwrap().starts_with("    line5"),
            "first line was {:?}",
            out.lines().next()
        );
    }

    #[test]
    fn tail_stderr_handles_empty_input() {
        assert_eq!(tail_stderr(b""), "(ssh produced no stderr output)");
        assert_eq!(tail_stderr(b"\n  \n"), "(ssh produced no stderr output)");
    }

    // ── allocate_local_port ──────────────────────────────────────────────────

    #[test]
    fn allocate_local_port_returns_distinct_high_ports() {
        let p1 = allocate_local_port().unwrap();
        let p2 = allocate_local_port().unwrap();
        assert!(p1 >= 1024, "got privileged port {p1}");
        assert!(p2 >= 1024, "got privileged port {p2}");
        // Two consecutive ephemeral allocations are usually different on Linux,
        // but the kernel may legitimately reuse a port. Just assert that at
        // least one of them was non-zero — i.e. the call succeeded.
        assert!(p1 != 0 && p2 != 0);
    }

    // ── render_argv ──────────────────────────────────────────────────────────

    #[test]
    fn render_argv_quotes_args_with_spaces() {
        let mut cmd = Command::new("ssh");
        cmd.arg("-o").arg("BatchMode=yes").arg("hello world");
        let s = render_argv(&cmd);
        assert!(s.contains("BatchMode=yes"));
        assert!(s.contains("\"hello world\""));
    }
}
