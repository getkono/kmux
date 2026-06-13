//! `--dry-run` and `--test` diagnostic subcommands.
//!
//! `--dry-run` exercises the real bootstrap (same `run_bootstrap` the TUI uses),
//! verifies the connection with one `Ping`/`Pong`, prints a human-readable
//! report on stdout, and exits.
//!
//! `--test` is a superset that additionally spawns the live
//! [`TransportSupervisor`] for ~10 seconds so transport scoring and any
//! hot-swap upgrade are visible. `--dry-run --test` together prints a warning
//! and behaves as `--test`.

use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use kmux_client::pipeline::{
    BootstrapEvent, BootstrapObserver, BootstrapOutcome, ResolvedTarget, run_bootstrap,
};
use kmux_client::supervisor::{SupervisorParams, TransportSupervisor, UpgradeSignal};
use kmux_protocol::messages::{ClientMessage, ServerMessage};
use kmux_protocol::transport::bootstrap::EndpointAdvert;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::cli::ServerArgs;
use crate::host_caps;

use super::parse_target;

const PING_TIMEOUT: Duration = Duration::from_secs(5);
const TEST_DURATION: Duration = Duration::from_secs(10);

/// Dispatch entry point wired in from `main.rs`.
///
/// If both flags are set, `test_mode` is true and a warning is printed.
pub async fn run_dry_run(args: &ServerArgs, test_mode: bool) -> anyhow::Result<()> {
    let (target, _) = parse_target(args.server.as_deref(), args.ssh_port);

    let observer = ConsoleObserver::new(Instant::now());
    observer.header(&target);

    let (srv_tx, mut srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
    // Dry-run path: no TUI, so no kitty kbd push. Always advertise false.
    let capabilities = host_caps::detect(false);

    let started = Instant::now();
    let outcome = run_bootstrap(target, capabilities, None, srv_tx, &observer)
        .await
        .context("bootstrap failed")?;

    let ping_rtt = verify_ping(&outcome, &mut srv_rx, &observer).await?;

    observer.line(
        "RESULT",
        format!(
            "connected via {}; bootstrap {:.0} ms, ping {:.2} ms",
            outcome.transport,
            started.elapsed().as_secs_f64() * 1000.0,
            ping_rtt.as_secs_f64() * 1000.0,
        ),
    );

    if test_mode {
        run_supervisor_phase(outcome, srv_rx, &observer).await?;
    }

    Ok(())
}

/// Send a `Ping { seq: 0 }` and await `Pong { seq: 0 }` within [`PING_TIMEOUT`].
///
/// Intervening messages (AuthResult, SessionListResult, etc.) are drained
/// and ignored — we only care that the control-plane is healthy end-to-end.
async fn verify_ping(
    outcome: &BootstrapOutcome,
    srv_rx: &mut mpsc::UnboundedReceiver<ServerMessage>,
    observer: &ConsoleObserver,
) -> anyhow::Result<Duration> {
    observer.line("PING", format!("seq=0; waiting up to {PING_TIMEOUT:?}"));

    outcome
        .client_tx
        .send(ClientMessage::Ping { seq: 0 })
        .map_err(|_| anyhow::anyhow!("data channel closed before ping could be sent"))?;

    let started = Instant::now();
    loop {
        let remaining = PING_TIMEOUT
            .checked_sub(started.elapsed())
            .unwrap_or_default();
        if remaining.is_zero() {
            anyhow::bail!("no pong within {PING_TIMEOUT:?}");
        }
        match timeout(remaining, srv_rx.recv()).await {
            Ok(Some(ServerMessage::Pong { seq: 0 })) => {
                let rtt = started.elapsed();
                observer.line(
                    "PING",
                    format!("OK - RTT {:.2} ms", rtt.as_secs_f64() * 1000.0),
                );
                return Ok(rtt);
            }
            Ok(Some(_)) => continue,
            Ok(None) => anyhow::bail!("server channel closed before pong"),
            Err(_) => anyhow::bail!("no pong within {PING_TIMEOUT:?}"),
        }
    }
}

/// Run the live supervisor for [`TEST_DURATION`] so upgrade scoring and
/// potential hot-swaps are observable on stdout.
async fn run_supervisor_phase(
    outcome: BootstrapOutcome,
    mut srv_rx: mpsc::UnboundedReceiver<ServerMessage>,
    observer: &ConsoleObserver,
) -> anyhow::Result<()> {
    let mut endpoints = match &outcome.ssh_context {
        Some(ctx) => ctx.endpoints.clone(),
        None => {
            observer.line(
                "SUPERVISOR",
                format!(
                    "no upgrade candidates (transport {} is terminal); observing for {}s",
                    outcome.transport,
                    TEST_DURATION.as_secs()
                ),
            );
            Vec::new()
        }
    };

    // Mirror the production launch_ssh_supervisor: include the active
    // transport in the endpoint set so the scoreboard log shows both
    // transports being compared, not just the upgrade candidate.
    if !endpoints.is_empty() {
        let active_address = format!("{}:{}", outcome.host, outcome.port);
        if !endpoints
            .iter()
            .any(|e| e.kind == outcome.transport && e.address == active_address)
        {
            endpoints.push(EndpointAdvert {
                kind: outcome.transport,
                address: active_address,
            });
        }
    }

    if endpoints.is_empty() {
        let _ = tokio::time::timeout(TEST_DURATION, async {
            while let Some(_msg) = srv_rx.recv().await {
                // drain so the channel doesn't accumulate
            }
        })
        .await;
        return Ok(());
    }

    let (upgrade_tx, mut upgrade_rx) = mpsc::channel::<UpgradeSignal>(1);
    let (fwd_tx, mut fwd_rx) = mpsc::unbounded_channel::<ServerMessage>();

    let supervisor = TransportSupervisor::new(SupervisorParams {
        endpoints,
        connection_id: outcome.connection_id,
        token: outcome.token.clone(),
        capabilities: outcome.capabilities.clone(),
        accept_invalid_certs: outcome.accept_invalid_certs,
        active_transport: outcome.transport,
        is_local: outcome.is_local,
        server_tx: fwd_tx,
        upgrade_tx,
        rtt_rx: None,
        forced: None,
        override_rx: None,
    });
    let handle = tokio::spawn(async move { supervisor.run().await });

    observer.line(
        "SUPERVISOR",
        format!("probing upgrades for {}s", TEST_DURATION.as_secs()),
    );

    let mut active = outcome.transport;
    let deadline = tokio::time::sleep(TEST_DURATION);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            upgrade = upgrade_rx.recv() => {
                if let Some(signal) = upgrade {
                    observer.line(
                        "SUPERVISOR",
                        format!("upgrade candidate: {} -> {}", active, signal.new_kind),
                    );
                    active = signal.new_kind;
                    // Intentionally do not swap senders: --test is observation-only.
                }
            }
            _ = srv_rx.recv() => {}
            _ = fwd_rx.recv() => {}
        }
    }

    handle.abort();
    observer.line(
        "RESULT",
        format!("supervisor ended; final observed transport {}", active),
    );
    Ok(())
}

// ─── ConsoleObserver ──────────────────────────────────────────────────────────

/// Observer for `--dry-run`: formats each [`BootstrapEvent`] as one
/// `[TAG] message (elapsed)` line on stdout, with tokens redacted from the
/// raw probe-or-start JSON.
pub struct ConsoleObserver {
    started: Instant,
    /// Serialises writes so interleaved tokio tasks don't scramble output.
    out: Mutex<std::io::Stdout>,
}

impl ConsoleObserver {
    pub fn new(started: Instant) -> Self {
        Self {
            started,
            out: Mutex::new(std::io::stdout()),
        }
    }

    pub fn header(&self, target: &ResolvedTarget) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "kmux dry-run for {}", target.label());
    }

    fn line(&self, tag: &str, msg: impl AsRef<str>) {
        let elapsed = self.started.elapsed();
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(
            out,
            "[{:<10}] {} ({:.2}s)",
            tag,
            msg.as_ref(),
            elapsed.as_secs_f64()
        );
    }
}

impl BootstrapObserver for ConsoleObserver {
    fn on_event(&self, event: &BootstrapEvent<'_>) {
        match event {
            BootstrapEvent::ParsedTarget { target } => {
                self.line("PARSE", format!("target={}", target.label()));
            }
            BootstrapEvent::DaemonQuery { socket } => {
                self.line(
                    "DAEMON",
                    format!("querying control socket {}", socket.display()),
                );
            }
            BootstrapEvent::DaemonAlreadyRunning {
                pid,
                port,
                tcp_port,
            } => {
                self.line(
                    "DAEMON",
                    format!("already running pid={pid} quic_port={port} tcp_port={tcp_port}"),
                );
            }
            BootstrapEvent::DaemonNotRunning => {
                self.line("DAEMON", "not running; will spawn");
            }
            BootstrapEvent::DaemonSpawning { binary } => {
                self.line("DAEMON", format!("spawning {}", binary.display()));
            }
            BootstrapEvent::DaemonReady {
                pid,
                port,
                tcp_port,
                elapsed,
            } => {
                self.line(
                    "DAEMON",
                    format!(
                        "ready pid={pid} quic_port={port} tcp_port={tcp_port} startup={:.2}s",
                        elapsed.as_secs_f64()
                    ),
                );
            }
            BootstrapEvent::SshProbeStarting { dest } => {
                self.line("SSH", format!("running `ssh {dest} kmuxd probe-or-start`"));
            }
            BootstrapEvent::SshProbeResponseRaw { json } => {
                self.line("SSH", format!("raw response: {}", redact_token(json)));
            }
            BootstrapEvent::SshProtocolVersionOk { version } => {
                self.line("SSH", format!("protocol_version OK ({version})"));
            }
            BootstrapEvent::SshTunnelReady {
                local_port,
                remote_port,
                elapsed,
            } => {
                self.line(
                    "SSH",
                    format!(
                        "tunnel ready: local={local_port} remote={remote_port} ({:.2}s)",
                        elapsed.as_secs_f64()
                    ),
                );
            }
            BootstrapEvent::HandshakeStarting {
                transport,
                host,
                port,
            } => {
                let addr = if *port > 0 {
                    format!("{}:{}", host, port)
                } else {
                    host.to_string()
                };
                self.line("HANDSHAKE", format!("{} {}", transport, addr));
            }
            BootstrapEvent::HandshakeAuthSent {
                protocol_version,
                connection_id,
            } => {
                let conn = match connection_id {
                    Some(c) => c.0.to_string(),
                    None => "None".to_string(),
                };
                self.line(
                    "AUTH",
                    format!("Auth sent (protocol={protocol_version}, conn_id={conn})"),
                );
            }
            BootstrapEvent::HandshakeAuthResult {
                success,
                connection_id,
                server_version,
                reason,
            } => {
                let conn = match connection_id {
                    Some(c) => c.0.to_string(),
                    None => "None".to_string(),
                };
                let ver = server_version.unwrap_or("?");
                let reason = reason.map(|r| format!(" reason={r}")).unwrap_or_default();
                self.line(
                    "AUTH",
                    format!(
                        "AuthResult success={success} conn_id={conn} server_version={ver}{reason}"
                    ),
                );
            }
            BootstrapEvent::BootstrapFailure { strategy, error } => {
                self.line("ERROR", format!("{strategy}: {error}"));
            }
            _ => {}
        }
    }
}

/// Replace every occurrence of `"token":"..."` with `"token":"***"` in a
/// JSON string. A raw string scan is used (no JSON parse) so malformed
/// responses still get redacted.
fn redact_token(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    let bytes = json.as_bytes();
    let needle = b"\"token\"";
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(needle) {
            out.push_str("\"token\"");
            let mut j = i + needle.len();
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b':') {
                out.push(bytes[j] as char);
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                out.push_str("\"***\"");
                j += 1;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += 1;
                }
                if j < bytes.len() {
                    j += 1;
                }
            }
            i = j;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_token_replaces_inline_token() {
        let raw = r#"{"protocol_version":13,"token":"supersecret","quic_port":8443}"#;
        let got = redact_token(raw);
        assert!(!got.contains("supersecret"));
        assert!(got.contains(r#""token":"***""#));
        assert!(got.contains("quic_port"));
    }

    #[test]
    fn redact_token_handles_whitespace() {
        let raw = r#"{"token" : "abc", "a":"b"}"#;
        let got = redact_token(raw);
        assert!(!got.contains("abc"));
        assert!(got.contains(r#""token" : "***""#));
    }

    #[test]
    fn redact_token_is_idempotent_without_token_field() {
        let raw = r#"{"foo":"bar"}"#;
        assert_eq!(redact_token(raw), raw);
    }

    #[test]
    fn dry_run_flag_is_parsed() {
        use crate::cli::Cli;
        use clap::Parser;
        let cli = Cli::try_parse_from(["kmux", "--dry-run"]).unwrap();
        assert!(cli.connect.dry_run);
        assert!(!cli.connect.test);
    }

    #[test]
    fn short_dry_run_flag_is_parsed() {
        use crate::cli::Cli;
        use clap::Parser;
        let cli = Cli::try_parse_from(["kmux", "-n"]).unwrap();
        assert!(cli.connect.dry_run);
    }

    #[test]
    fn test_flag_is_parsed() {
        use crate::cli::Cli;
        use clap::Parser;
        let cli = Cli::try_parse_from(["kmux", "--test"]).unwrap();
        assert!(!cli.connect.dry_run);
        assert!(cli.connect.test);
    }

    #[test]
    fn both_flags_coexist() {
        use crate::cli::Cli;
        use clap::Parser;
        let cli = Cli::try_parse_from(["kmux", "--dry-run", "--test"]).unwrap();
        assert!(cli.connect.dry_run);
        assert!(cli.connect.test);
    }
}
