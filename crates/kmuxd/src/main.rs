mod announce;
mod app;
mod auth;
mod capability;
mod capture;
mod client_handler;
mod config;
mod connection;
mod conversions;
mod daemon;
mod engine;
#[cfg(feature = "federation")]
mod federation;
#[cfg(test)]
mod fixtures;
mod handoff;
mod impair;
mod lock;
mod log_writer;
mod outbound;
mod persist;
mod process_stats;
mod relay;
mod scrollback;
mod startup;
mod supervisor;
mod tcp_listener;
mod tls;
mod trace;
mod wordlist;

// The server-side VT pipeline (terminal backend, diff engine, scrollback
// mirror, `TermState`) lives in `kmux-vt-core` so the daemon's in-process path
// and the isolated `kmux-vt-worker` subprocess run identical diff code (issue
// #126). Re-exported at the crate root so existing `crate::backend::…` /
// `crate::diff_engine::…` / `crate::term_state::…` paths keep resolving.
pub use kmux_vt_core::{backend, diff_engine, term_state};

use anyhow::Context as _;
use clap::Parser;
use rand::Rng;
use tracing::Instrument;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "kmuxd",
    about = "kmux remote terminal server",
    version = concat!(
        env!("CARGO_PKG_VERSION"),
        " (",
        env!("BUILD_GIT_SHA"),
        env!("BUILD_GIT_DIRTY_SUFFIX"),
        ", ",
        env!("BUILD_DATE"),
        ", ",
        env!("BUILD_PROFILE"),
        ")"
    )
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to a `kmuxd.toml` config file. When absent, config is discovered at
    /// `$XDG_CONFIG_HOME/kmuxd/kmuxd.toml` or `/etc/kmuxd/kmuxd.toml`.
    #[arg(long)]
    config: Option<std::path::PathBuf>,

    /// Address to bind (deprecated; prefer `[[listen]] bind = "..."` in
    /// `kmuxd.toml`). When omitted, the per-listener `bind` from the config
    /// file (or its built-in default of `0.0.0.0`) is used; passing this
    /// flag overrides every QUIC and TCP+TLS listener.
    #[arg(long)]
    bind: Option<String>,

    /// QUIC port to listen on (0 = ephemeral). Overrides every QUIC listener
    /// when set; absent leaves each listener's configured port intact.
    #[arg(long)]
    port: Option<u16>,

    /// Path to a PEM certificate file. Optional: when no cert/key pair is
    /// configured the daemon generates an in-memory self-signed certificate.
    /// Prefer `[tls] cert = "..."` in kmuxd.toml for persistent configuration.
    #[arg(long)]
    cert: Option<String>,

    /// Path to a PEM private key file. Optional: when no cert/key pair is
    /// configured the daemon generates an in-memory self-signed certificate.
    /// Prefer `[tls] key = "..."` in kmuxd.toml for persistent configuration.
    #[arg(long)]
    key: Option<String>,

    /// Run as a background daemon (double-fork, PID file, Unix socket control).
    /// Daemonization happens before the tokio runtime starts, so fork-safety is maintained.
    #[arg(long)]
    daemon: bool,

    /// TCP+TLS port (0 = ephemeral). Overrides every TCP+TLS listener when
    /// set; absent leaves each listener's configured port intact.
    #[arg(long)]
    tcp_port: Option<u16>,

    /// Pull live PTY sessions from a still-running daemon during a graceful
    /// restart. Set automatically by the outgoing daemon when it spawns its
    /// successor; not intended for manual use. On any failure the daemon falls
    /// back to the normal on-disk snapshot restore.
    #[arg(long)]
    handoff: bool,

    /// Pane VT isolation mode. Overrides `[daemon] session_isolation` in
    /// kmuxd.toml when set. `in-process` (default) keeps the emulator in the
    /// daemon; `process` runs each pane's VT pipeline in an isolated
    /// `kmux-vt-worker` subprocess (issue #126).
    #[arg(long, value_enum)]
    session_isolation: Option<config::SessionIsolationMode>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Probe for a running daemon or start one, then print connection JSON to stdout.
    ///
    /// Output format (JSON):
    ///   `{"protocol_version": N, "kmuxd_version": "...", "quic_port": N, "tcp_port": N,
    ///     "token": "...", "endpoints": [...]}`
    ///
    /// Designed for SSH-based auto-negotiation: `ssh user@host kmuxd probe-or-start`.
    /// Exits 0 on success; exits 1 with an error message on stderr on failure.
    ProbeOrStart,

    /// Print the effective configuration (defaults merged with the config file) and exit.
    PrintConfig {
        /// Path to the config file (overrides standard search order).
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },
}

/// The full multi-line version matrix for `kmuxd -V`, mirroring the client's
/// `VersionInfo::long_string()` but built from this crate's own build env vars
/// (the daemon does not link the client-side `kmux-app::version`). The const
/// protocol number can't live in a clap derive literal, so we override the
/// version at runtime below.
fn long_version() -> String {
    use std::fmt::Write;
    let mut s = format!(
        "{} ({}{}, {}, {})",
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_GIT_SHA"),
        env!("BUILD_GIT_DIRTY_SUFFIX"),
        env!("BUILD_DATE"),
        env!("BUILD_PROFILE"),
    );
    let _ = write!(
        s,
        "\n  protocol:   {}",
        kmux_protocol::messages::PROTOCOL_RANGE
    );
    let _ = write!(s, "\n  rustc:      {}", env!("BUILD_RUSTC_VERSION"));
    let _ = write!(s, "\n  built:      {}", env!("BUILD_TIMESTAMP"));
    s
}

fn main() -> anyhow::Result<()> {
    // Parse CLI before daemonizing so --help/--version work in the foreground.
    // Override clap's compile-time one-line version with the full runtime matrix
    // (the const protocol number can't be a derive literal), so `kmuxd -V`
    // matches `kmux -V`.
    let cli = {
        use clap::{CommandFactory, FromArgMatches};
        let version: &'static str = Box::leak(long_version().into_boxed_str());
        let mut cmd = Cli::command().version(version);
        let matches = cmd.get_matches_mut();
        match Cli::from_arg_matches(&matches) {
            Ok(cli) => cli,
            Err(e) => e.format(&mut cmd).exit(),
        }
    };

    // probe-or-start: short-lived query/start, no need to daemonize or init full logging.
    if matches!(cli.command, Some(Command::ProbeOrStart)) {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(probe_or_start());
    }

    // print-config: dump effective config and exit.
    if let Some(Command::PrintConfig { config: cfg_path }) = &cli.command {
        let (cfg, source) = config::load_config(cfg_path.as_deref())?;
        match source {
            Some(p) => eprintln!("# Loaded from: {}", p.display()),
            None => eprintln!("# Using built-in defaults (no config file found)"),
        }
        println!("{}", toml::to_string_pretty(&cfg)?);
        return Ok(());
    }

    if cli.daemon {
        if cli.handoff {
            // A graceful-restart successor does not fork: it stays its
            // predecessor's direct child — spawned in a process group of its
            // own, in `/`, with no terminal and its output in the boot log —
            // so the predecessor can stop it and learn how it exited (issue
            // #207). Of the rest of what `daemonize` does, only the umask is
            // left to set. Nor does it take the pid file: the predecessor
            // still holds its `flock`. It writes the pid file itself once the
            // predecessor exits (see `startup::async_main`).
            nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
        } else {
            daemon::daemonize_process(Some(&kmux_sys::dirs::pid_path()?))?;
            // After this point we are in the daemonized child process with
            // fresh fds.
        }
    }

    // What a graceful-restart successor is started with, built here where
    // this daemon resolves its paths: from the flags it was started with.
    let successor_args = successor_args(&cli)?;

    // Load the config before logging starts: it says how the log rotates.
    let (mut cfg_file, cfg_source) = config::load_config(cli.config.as_deref())?;

    // Initialize tracing after daemonize (child process has fresh fds).
    // Log to a persistent file, rolled over by size (issue #207); fall back to
    // stderr if the path can't be opened.
    let instance_id = generate_instance_id();
    let rotation = cfg_file.daemon.log_rotation();
    match kmux_sys::dirs::daemon_log_path()
        .and_then(|p| Ok(log_writer::RotatingFile::open(p, rotation)?))
    {
        Ok(file) => {
            // `ResilientWriter` (not the stock `Mutex<File>`) so a write that
            // fails on a full disk degrades to "no logs" instead of poisoning
            // the lock and cascading into worker panics that kill the daemon —
            // the root cause of `kmux daemon restart` failing under disk
            // pressure. See `log_writer`.
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env()
                        .add_directive("kmuxd=info".parse()?)
                        // Surface forwarded libghostty-vt diagnostics — notably
                        // unknown control sequences (issue #187).
                        .add_directive("kmux::vt=warn".parse()?),
                )
                .with_writer(log_writer::ResilientWriter::new(file))
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env()
                        .add_directive("kmuxd=info".parse()?)
                        .add_directive("kmux::vt=warn".parse()?),
                )
                .init();
        }
    }
    // Route libghostty-vt's own diagnostics (unknown control sequences, …) into
    // the daemon log now that tracing is up (issue #187).
    backend::install_vt_log_forwarding();
    tracing::info!(
        instance_id = %instance_id,
        version = concat!(
            env!("CARGO_PKG_VERSION"),
            " (",
            env!("BUILD_GIT_SHA"),
            env!("BUILD_GIT_DIRTY_SUFFIX"),
            ", ",
            env!("BUILD_DATE"),
            ", ",
            env!("BUILD_PROFILE"),
            ")"
        ),
        protocol_version = %kmux_protocol::messages::PROTOCOL_RANGE,
        "kmuxd started"
    );

    // Apply deprecated CLI overrides to the config loaded above.
    if cfg_source.is_none() {
        // No config file found: write a default template on first run.
        if let Ok(xdg_cfg) = std::env::var("XDG_CONFIG_HOME") {
            let default_path = std::path::PathBuf::from(xdg_cfg)
                .join("kmuxd")
                .join("kmuxd.toml");
            if let Err(e) = config::write_default_config(&default_path) {
                tracing::warn!(
                    "Could not write default config to {}: {e}",
                    default_path.display()
                );
            } else {
                tracing::info!("Wrote default config to {}", default_path.display());
            }
        }
    }

    // Apply deprecated CLI overrides (they win over the config file when present).
    if let Some(cert) = cli.cert {
        cfg_file.tls.cert = Some(cert);
    }
    if let Some(key) = cli.key {
        cfg_file.tls.key = Some(key);
    }
    // The `--session-isolation` flag (issue #126) overrides the `[daemon]` key.
    if let Some(mode) = cli.session_isolation {
        cfg_file.daemon.session_isolation = mode;
    }
    // Apply bind/port overrides only when the user explicitly passed the
    // corresponding flag. Previously these were eager-defaulted on the CLI
    // (`--bind 0.0.0.0`, `--port 0`), so every invocation silently rewrote
    // every listener's bind+port — making `[[listen]] bind = "..."` in
    // kmuxd.toml impossible to honour.
    for l in &mut cfg_file.listen {
        use config::ListenKind;
        if !l.enabled {
            continue;
        }
        match l.kind {
            ListenKind::Quic => {
                if let Some(bind) = &cli.bind {
                    l.bind = bind.clone();
                }
                if let Some(port) = cli.port {
                    l.port = port;
                }
            }
            ListenKind::TcpTls => {
                if let Some(bind) = &cli.bind {
                    l.bind = bind.clone();
                }
                if let Some(port) = cli.tcp_port {
                    l.port = port;
                }
            }
            ListenKind::Unix => {}
        }
    }

    let server_cfg = config::ServerConfig::resolve(cfg_file)?;

    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(
        startup::async_main(cli.daemon, cli.handoff, server_cfg, successor_args)
            .instrument(tracing::info_span!("instance", id = %instance_id)),
    );

    // Shut the runtime down without joining its blocking pool. PTY children no
    // longer park a blocking thread each (kmux-pty reaps them all from one
    // dedicated thread outside this runtime, issue #205), so a graceful-restart
    // predecessor whose migrated children live on no longer hangs here. What is
    // left is defence: dropping the runtime would wait for any blocking task
    // still running (a filesystem call stuck on a full or dead disk), and a
    // daemon on its way out must "completely shut-off" (issue #36) regardless.
    rt.shutdown_background();

    if let Some(code) = stood_down_exit_code(&result) {
        // Not a failure: another daemon serves, and the one that spawned
        // this successor tells the two apart by this code (issue #207).
        if let Err(e) = &result {
            tracing::warn!("{e:#}");
            eprintln!("{e:#}");
        }
        std::process::exit(code);
    }
    result
}

/// The exit code for a daemon run that ended in `result`, when it is not the
/// usual one: a handoff successor that stood down exits with
/// [`kmux_protocol::control_rpc::HANDOFF_STOOD_DOWN_EXIT_CODE`].
fn stood_down_exit_code(result: &anyhow::Result<()>) -> Option<i32> {
    let stood_down = result
        .as_ref()
        .err()?
        .downcast_ref::<handoff::StoodDown>()
        .is_some();
    stood_down.then_some(kmux_protocol::control_rpc::HANDOFF_STOOD_DOWN_EXIT_CODE)
}

/// Implementation of the `probe-or-start` subcommand.
///
/// Queries the local daemon control socket. If the daemon is not running,
/// starts it and polls until it responds.  Prints extended connection JSON to stdout.
///
/// Output includes `protocol_range` and `kmuxd_version` so current clients can
/// detect mismatches before attempting a full connection. `protocol_version`
/// remains a frozen legacy sentinel for old probe consumers.
async fn probe_or_start() -> anyhow::Result<()> {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    /// Query the Unix control socket and return the full status response on success.
    async fn query() -> Option<serde_json::Value> {
        let socket_path = kmux_sys::dirs::socket_path().ok()?;
        let stream =
            tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(&socket_path))
                .await
                .ok()?
                .ok()?;

        let (read_half, mut write_half) = stream.into_split();
        write_half
            .write_all(b"{\"command\":\"status\"}\n")
            .await
            .ok()?;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .ok()?
            .ok()?;

        #[derive(serde::Deserialize)]
        struct Resp {
            port: u16,
            #[serde(default)]
            tcp_port: u16,
            token: String,
            pid: u32,
            /// The daemon's SSH view of its endpoints; absent in older builds.
            #[serde(default)]
            ssh_endpoints: Option<Vec<serde_json::Value>>,
        }
        let resp: Resp = serde_json::from_str(line.trim()).ok()?;

        // Verify the reported PID is alive.
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        if kill(Pid::from_raw(resp.pid as i32), None).is_err() {
            return None;
        }

        let endpoints = ssh_view(resp.ssh_endpoints, resp.port, resp.tcp_port);

        // Build the extended probe-or-start JSON (backward-compatible: adds new fields).
        let json = serde_json::json!({
            "protocol_version": kmux_protocol::messages::LEGACY_PROTOCOL_VERSION,
            "protocol_range": kmux_protocol::messages::PROTOCOL_RANGE,
            "kmuxd_version": env!("CARGO_PKG_VERSION"),
            "quic_port": resp.port,
            "tcp_port": resp.tcp_port,
            "token": resp.token,
            "endpoints": endpoints,
        });
        Some(json)
    }

    // Fast path — already running.
    if let Some(json) = query().await {
        println!("{json}");
        return Ok(());
    }

    // Slow path — start a new daemon instance.
    cleanup_and_start_daemon()?;

    // Poll until ready (up to 10 s — remote machines can be slower to start).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;

        if let Some(json) = query().await {
            println!("{json}");
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }

    anyhow::bail!("timed out waiting for local kmuxd to start")
}

/// The endpoints `probe-or-start` hands its SSH caller: the daemon's own SSH
/// view (`ssh-only` listeners included, `local` ones left out), or — from a
/// daemon that predates it — one built from the daemon's ports.
///
/// The daemon's `endpoints` list is never the answer: it is the local view,
/// which drops every `ssh-only` listener (issue #227).
fn ssh_view(
    ssh_endpoints: Option<Vec<serde_json::Value>>,
    quic_port: u16,
    tcp_port: u16,
) -> serde_json::Value {
    let endpoints = ssh_endpoints.unwrap_or_else(|| build_ssh_endpoints(quic_port, tcp_port));
    serde_json::Value::Array(endpoints)
}

/// Build a fallback endpoint list for SSH callers from raw port numbers.
///
/// Used when the running daemon's status reply carries no `ssh_endpoints`.
/// Applies `BootstrapPath::Ssh` audience filtering so only `Any` and `SshOnly`
/// listeners are included.
fn build_ssh_endpoints(quic_port: u16, tcp_port: u16) -> Vec<serde_json::Value> {
    use announce::{BootstrapPath, build_endpoint_list};
    use config::{Audience, ListenConfig, ListenKind};

    // Synthetic listener configs from the running ports; no config file needed.
    let mut listeners = Vec::new();
    if quic_port != 0 {
        listeners.push(ListenConfig {
            kind: ListenKind::Quic,
            bind: "127.0.0.1".to_string(),
            port: quic_port,
            enabled: true,
            path: String::new(),
            audience: Audience::Any,
            priority: 0,
        });
    }
    if tcp_port != 0 {
        listeners.push(ListenConfig {
            kind: ListenKind::TcpTls,
            bind: "127.0.0.1".to_string(),
            port: tcp_port,
            enabled: true,
            path: String::new(),
            audience: Audience::SshOnly,
            priority: 0,
        });
    }

    build_endpoint_list(&listeners, BootstrapPath::Ssh, None)
        .into_iter()
        .map(|a| serde_json::json!({"kind": format!("{}", a.kind), "address": a.address}))
        .collect()
}

/// Remove stale daemon artifacts and spawn `kmuxd --daemon`.
///
/// Unlike the client auto-spawn path (`kmux-connect`'s `start_daemon`), this
/// `probe-or-start` slow path does not take the client-side `daemon.spawn.lock`.
/// It does not need to: the **authoritative** single-instance guard is the
/// `flock` the daemonized grandchild holds on `daemon.pid` (see
/// `daemon::daemonize_process`). If two `probe-or-start` invocations race here,
/// only one daemonized child wins the pid-file lock; the loser exits before
/// binding the control socket. Debug and release never collide because they
/// resolve different runtime dirs entirely.
fn cleanup_and_start_daemon() -> anyhow::Result<()> {
    use std::os::unix::io::AsRawFd;

    let pid_path = kmux_sys::dirs::pid_path()?;
    let socket_path = kmux_sys::dirs::socket_path()?;

    // The daemonized child holds an exclusive flock on its PID file. A held
    // lock proves ownership without trusting a possibly reused PID; preserve
    // both artifacts and fail safely when that owner is unresponsive.
    if pid_path.exists() {
        let pid_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pid_path)
            .map_err(|e| anyhow::anyhow!("failed to inspect {}: {e}", pid_path.display()))?;
        #[allow(deprecated)]
        match nix::fcntl::flock(
            pid_file.as_raw_fd(),
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        ) {
            Ok(()) => {}
            Err(nix::errno::Errno::EWOULDBLOCK) => {
                let owner = std::fs::read_to_string(&pid_path)
                    .ok()
                    .and_then(|value| value.trim().parse::<u32>().ok())
                    .map_or_else(
                        || "an active process".to_string(),
                        |pid| format!("PID {pid}"),
                    );
                anyhow::bail!(
                    "{owner} owns the daemon PID file but the control socket is unresponsive; \
                     automatic startup left it untouched. Inspect `kmux daemon status` and \
                     `kmux daemon logs`, then run `kmux daemon restart` if needed"
                );
            }
            Err(error) => anyhow::bail!(
                "failed to lock {} while checking daemon ownership: {error}",
                pid_path.display()
            ),
        }
        std::fs::remove_file(&pid_path)
            .map_err(|e| anyhow::anyhow!("failed to remove {}: {e}", pid_path.display()))?;
    }
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .map_err(|e| anyhow::anyhow!("failed to remove {}: {e}", socket_path.display()))?;
    }

    // Resolve the path of the current executable (i.e., this very binary).
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("kmuxd"));

    // Capture the spawned daemon's pre-daemonize stdout+stderr in the boot log
    // (rather than discarding it) so a boot crash is diagnosable.
    let (out, err) = boot_log_stdio();
    std::process::Command::new(&exe)
        .args(kmux_protocol::control_rpc::DAEMON_BOOT_ARGS)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn {}: {e}", exe.display()))?;

    Ok(())
}

/// Open the shared boot log (truncating) for a freshly-spawned daemon's
/// stdout+stderr, falling back to `/dev/null` if it can't be created.
///
/// Used by every daemon-spawn path in this binary (`probe-or-start`, the
/// graceful-restart successor) so a child that dies before it daemonizes (full
/// disk, panic during restore, bind failure) leaves a trail at
/// [`kmux_sys::dirs::boot_log_path`] instead of vanishing. The boot log can
/// contain the auth token, so it is created `0o600`.
pub(crate) fn boot_log_stdio() -> (std::process::Stdio, std::process::Stdio) {
    use std::os::unix::fs::OpenOptionsExt;
    let opened = kmux_sys::dirs::boot_log_path().ok().and_then(|path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .ok()
    });
    match opened.and_then(|f| f.try_clone().ok().map(|c| (f, c))) {
        Some((out, err)) => (
            std::process::Stdio::from(out),
            std::process::Stdio::from(err),
        ),
        None => (std::process::Stdio::null(), std::process::Stdio::null()),
    }
}

/// The arguments a graceful-restart successor is started with (issue #234):
/// `--daemon --handoff`, and every flag `cli` carries that shapes what a
/// daemon serves — its config file, listeners, certificate and pane
/// isolation — so the successor serves exactly what this daemon does. It
/// used to get the client's boot arguments instead, so a daemon bound to
/// loopback came back from a restart bound to every interface.
///
/// Paths are made absolute against this process's working directory, where
/// this daemon resolves them; the successor starts in `/`.
fn successor_args(cli: &Cli) -> anyhow::Result<Vec<std::ffi::OsString>> {
    use clap::ValueEnum as _;
    use std::ffi::OsString;

    let absolute = |path: &std::path::Path| -> anyhow::Result<OsString> {
        Ok(std::path::absolute(path)
            .with_context(|| format!("resolving {}", path.display()))?
            .into_os_string())
    };
    let mut args: Vec<OsString> = vec!["--daemon".into(), "--handoff".into()];
    let mut flag = |name: &str, value: OsString| args.extend([name.into(), value]);
    if let Some(config) = &cli.config {
        flag("--config", absolute(config)?);
    }
    if let Some(bind) = &cli.bind {
        flag("--bind", bind.into());
    }
    if let Some(port) = cli.port {
        flag("--port", port.to_string().into());
    }
    if let Some(port) = cli.tcp_port {
        flag("--tcp-port", port.to_string().into());
    }
    if let Some(cert) = &cli.cert {
        flag("--cert", absolute(cert.as_ref())?);
    }
    if let Some(key) = &cli.key {
        flag("--key", absolute(key.as_ref())?);
    }
    if let Some(mode) = cli.session_isolation
        && let Some(value) = mode.to_possible_value()
    {
        flag("--session-isolation", value.get_name().into());
    }
    Ok(args)
}

fn generate_instance_id() -> String {
    let mut bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use kmux_protocol::control_rpc::HANDOFF_STOOD_DOWN_EXIT_CODE;

    use clap::Parser as _;

    use super::{Cli, ssh_view, stood_down_exit_code, successor_args};
    use crate::config::SessionIsolationMode;

    /// A successor is started with every flag that shapes what its
    /// predecessor serves, meaning to it what it meant to the predecessor
    /// (issue #234): parsed back, each is the predecessor's, its paths
    /// absolute. A daemon started with none passes none on, so the config
    /// file decides for both.
    #[test]
    fn a_successor_is_started_with_what_shapes_its_predecessor() {
        let cli = Cli::try_parse_from([
            "kmuxd",
            "--daemon",
            "--config",
            "kmuxd.toml",
            "--bind",
            "127.0.0.1",
            "--port",
            "4433",
            "--tcp-port",
            "4434",
            "--cert",
            "cert.pem",
            "--key",
            "/etc/kmuxd/key.pem",
            "--session-isolation",
            "process",
        ])
        .unwrap();

        let args = successor_args(&cli).unwrap();
        let successor = Cli::try_parse_from(std::iter::once("kmuxd".into()).chain(args)).unwrap();

        let cwd = std::env::current_dir().unwrap();
        assert!(successor.daemon && successor.handoff);
        assert_eq!(successor.config, Some(cwd.join("kmuxd.toml")));
        assert_eq!(successor.bind.as_deref(), Some("127.0.0.1"));
        assert_eq!(
            (successor.port, successor.tcp_port),
            (Some(4433), Some(4434))
        );
        let cert = cwd.join("cert.pem").display().to_string();
        assert_eq!(successor.cert, Some(cert));
        assert_eq!(successor.key.as_deref(), Some("/etc/kmuxd/key.pem"));
        assert_eq!(
            successor.session_isolation,
            Some(SessionIsolationMode::Process)
        );

        let bare = Cli::try_parse_from(["kmuxd", "--daemon"]).unwrap();
        assert_eq!(successor_args(&bare).unwrap(), ["--daemon", "--handoff"]);
    }

    /// `probe-or-start` hands back the daemon's SSH view as it is, and builds
    /// one from the ports — the TCP+TLS listener as `ssh-only` — only for a
    /// daemon that sends none (issue #227).
    #[test]
    fn probe_or_start_prefers_the_daemons_ssh_view() {
        let daemons = vec![serde_json::json!({"kind": "tcp+tls", "address": "10.0.0.2:7000"})];
        assert_eq!(
            ssh_view(Some(daemons.clone()), 5000, 6000),
            serde_json::Value::Array(daemons)
        );

        let built = ssh_view(None, 5000, 6000);
        let addresses: Vec<&str> = built
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(|e| e["address"].as_str())
            .collect();
        assert_eq!(addresses, ["127.0.0.1:5000", "127.0.0.1:6000"]);
    }

    /// Only a successor that stood down exits with its own code; a failure
    /// keeps the usual one.
    #[test]
    fn only_a_stand_down_has_its_own_exit_code() {
        let stood_down: anyhow::Result<()> =
            Err(crate::handoff::StoodDown("the predecessor rolled back".into()).into());
        assert_eq!(
            stood_down_exit_code(&stood_down),
            Some(HANDOFF_STOOD_DOWN_EXIT_CODE)
        );
        assert_eq!(stood_down_exit_code(&Err(anyhow::anyhow!("boom"))), None);
        assert_eq!(stood_down_exit_code(&Ok(())), None);
    }
}
