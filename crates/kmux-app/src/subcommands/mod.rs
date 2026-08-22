mod client_cmd;
mod clients;
mod daemon_cmd;
mod debug;
mod dry_run;
mod list;
mod logs;
mod notify;
mod ps;
pub mod render;
mod status;
pub use client_cmd::run_client_command;
pub use clients::{KickClientConfig, ListClientsConfig, run_kick_client, run_list_clients};
pub use daemon_cmd::run_daemon_command;
pub use debug::run_debug_command;
pub use dry_run::run_dry_run;
pub use list::{ListSessionsConfig, run_list_sessions};
pub use notify::{NotifyConfig, run_notify};
pub use ps::{ProcessOverviewConfig, run_process_overview};
pub use status::run_status;

use kmux_client::pipeline::ResolvedTarget;
use kmux_client::ssh::{self, ParsedServer};

use crate::cli::ResolvedConnection;

/// Resolve CLI args to a [`ResolvedTarget`] without any network I/O.
///
/// `server == None` selects the local daemon (UDS). Any non-empty string
/// resolves to an SSH target — there is no direct-QUIC CLI surface. All I/O
/// (daemon probe, SSH negotiation, handshake) happens inside
/// [`kmux_client::pipeline::run_bootstrap`].
pub fn parse_target(
    server: Option<&str>,
    ssh_port_override: Option<u16>,
) -> (ResolvedTarget, Option<ParsedServer>) {
    let parsed = server.map(ssh::parse_server_string);

    let target = match parsed.as_ref().and_then(ssh::resolve_remote_target) {
        Some(mut t) => {
            if let Some(p) = ssh_port_override {
                t.ssh_port = Some(p);
            }
            ResolvedTarget::Ssh {
                target: t,
                accept_invalid_certs: true,
            }
        }
        None => ResolvedTarget::LocalDaemon,
    };

    (target, parsed)
}

/// Resolve connection parameters for headless subcommands (`list-sessions`).
///
/// Two modes: local daemon (UDS-mediated TCP token) or SSH (full negotiation
/// plus tunnel). Unlike the TUI path, this performs negotiation up front
/// because list.rs speaks raw TCP rather than going through the bootstrap
/// pipeline.
pub async fn resolve_connection(
    server: Option<&str>,
    ssh_port_override: Option<u16>,
) -> anyhow::Result<ResolvedConnection> {
    let parsed = server.map(ssh::parse_server_string);
    let ssh_target = parsed
        .as_ref()
        .and_then(ssh::resolve_remote_target)
        .map(|mut t| {
            if let Some(p) = ssh_port_override {
                t.ssh_port = Some(p);
            }
            t
        });

    if let Some(target) = ssh_target {
        #[cfg(feature = "remote")]
        {
            tracing::info!(
                host = %target.host,
                user = ?target.user,
                "SSH negotiation starting"
            );
            let session = ssh::negotiate(&target)
                .await
                .map_err(|e| anyhow::anyhow!("SSH negotiation failed: {e}"))?;
            Ok(ResolvedConnection {
                host: "127.0.0.1".to_string(),
                port: session.local_tcp_port,
                tcp_port: None,
                token: session.token,
            })
        }
        // Lean build: no direct SSH dial-out. CLI subcommands (`ls`, `--dry-run`)
        // reach only the local daemon; remote sessions are federated through the
        // GUI via OpenPeer (issue #121).
        #[cfg(not(feature = "remote"))]
        {
            let _ = target;
            Err(anyhow::anyhow!(
                "remote `--server` targets are not supported in this build; \
                 this client connects only to the local daemon"
            ))
        }
    } else {
        let status = kmux_client::daemon::ensure_compatible_daemon().await?;
        Ok(ResolvedConnection {
            host: "127.0.0.1".to_string(),
            port: status.port,
            tcp_port: Some(status.tcp_port),
            token: status.token,
        })
    }
}

/// Run the full `Auth → AuthChallenge → AuthProof → AuthResult` handshake on a
/// raw framed stream for the headless one-shot subcommands (issue #146). Returns
/// once the daemon confirms authentication, or an error on rejection.
pub(crate) async fn authenticate<R, W>(
    read_half: &mut R,
    write_half: &mut W,
    token: String,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use kmux_protocol::messages::{
        ClientCapabilities, ClientMessage, FrontendKind, ServerMessage, version_mismatch_hint,
    };
    use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
    use kmux_sys::identity::Identity;

    let identity = Identity::load_or_create()?;
    let auth = ClientMessage::Auth {
        token,
        protocol_range: kmux_protocol::messages::PROTOCOL_RANGE,
        protocol_capabilities: kmux_protocol::messages::protocol_capabilities(),
        capabilities: ClientCapabilities::default(),
        connection_id: None,
        public_key: identity.public_key_bytes().to_vec(),
        hostname: kmux_sys::identity::local_hostname(),
        username: kmux_sys::identity::local_username(),
        // This is the CLI control path (kmux clients / kick); always a CLI build.
        client_kind: FrontendKind::Cli,
        client_git_sha: kmux_protocol::buildinfo::git_sha().to_string(),
        client_git_dirty: kmux_protocol::buildinfo::git_dirty(),
        client_build_profile: kmux_protocol::buildinfo::build_profile().to_string(),
    };
    write_frame(write_half, &encode_client(&auth)?).await?;

    loop {
        let data = read_frame(read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Connection closed before auth response"))?;
        match decode_server(&data)? {
            ServerMessage::AuthChallenge { nonce } => {
                let proof = ClientMessage::AuthProof {
                    signature: identity.sign(&nonce),
                };
                write_frame(write_half, &encode_client(&proof)?).await?;
            }
            ServerMessage::AuthResult { success: true, .. } => return Ok(()),
            ServerMessage::AuthResult {
                success: false,
                reason,
                ..
            } => {
                let reason_str = reason.unwrap_or_else(|| "unknown error".into());
                let hint = version_mismatch_hint(&reason_str);
                if hint.is_empty() {
                    anyhow::bail!("Authentication failed: {reason_str}");
                }
                anyhow::bail!("Authentication failed: {reason_str}\n{hint}");
            }
            _ => continue,
        }
    }
}

/// Connect to `conn` over TCP and complete the auth handshake.
///
/// The six headless subcommands each open a socket, split it, and authenticate
/// with the same three lines and the same error text. Having one copy means a
/// change to how the CLI dials the daemon — a timeout, a different port
/// preference, IPv6 — is one edit rather than six that can drift apart.
pub(crate) async fn connect_authenticated(
    conn: &ResolvedConnection,
) -> anyhow::Result<(
    tokio::net::tcp::OwnedReadHalf,
    tokio::net::tcp::OwnedWriteHalf,
)> {
    let tcp_port = conn.tcp_port.unwrap_or(conn.port);
    let stream = tokio::net::TcpStream::connect(format!("{}:{}", conn.host, tcp_port))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}:{}: {e}", conn.host, tcp_port))?;
    let (mut read_half, mut write_half) = stream.into_split();
    authenticate(&mut read_half, &mut write_half, conn.token.clone()).await?;
    Ok((read_half, write_half))
}

/// Send one request and return the first reply `accept` recognises.
///
/// The daemon interleaves unrelated pushes on the same channel, so a one-shot
/// request is always "write, then read until the answer arrives" — which the
/// subcommands had each spelled out, eleven times, along with the two arms that
/// are the same everywhere: `ServerMessage::Error` becomes an `Err` labelled
/// with `what`, and a closed connection becomes an `Err` saying so. Only the
/// arm that recognises the answer is per-call, and that is what `accept` is.
///
/// Any `Error` ends the call, not only one carrying a matching `request_id`.
/// These are one-shot connections that issue a handful of requests in sequence,
/// so there is no other request an error could belong to — and treating it as
/// unrelated is what made `kmux ls` and `kmux ps` report "connection closed"
/// instead of what the daemon actually said.
pub(crate) async fn request_reply<R, W, T>(
    read_half: &mut R,
    write_half: &mut W,
    request: &kmux_protocol::messages::ClientMessage,
    what: &str,
    accept: impl Fn(kmux_protocol::messages::ServerMessage) -> Option<T>,
) -> anyhow::Result<T>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use kmux_protocol::messages::ServerMessage;
    use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};

    write_frame(write_half, &encode_client(request)?).await?;
    loop {
        let data = read_frame(read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Connection closed before {what}"))?;
        let msg = decode_server(&data)?;
        if let ServerMessage::Error { message, .. } = &msg {
            anyhow::bail!("{what} failed: {message}");
        }
        if let Some(value) = accept(msg) {
            return Ok(value);
        }
    }
}

/// Ask a yes/no question on the terminal, returning `true` only on an explicit
/// yes. Mirrors the nested-GUI guard in `kmux/src/main.rs`: the prompt goes to
/// stderr, EOF (Ctrl-D) and an empty line are the safe default (`false`).
///
/// The caller is responsible for the no-TTY policy (this is only invoked once a
/// TTY is confirmed); `default_no` controls which option is capitalized.
pub(crate) fn confirm_yes_no(question: &str) -> std::io::Result<bool> {
    use std::io::Write;

    let mut err = std::io::stderr();
    loop {
        let _ = write!(err, "{question} [y/N] ");
        let _ = err.flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            let _ = writeln!(err);
            return Ok(false);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" | "n" | "no" => return Ok(false),
            "y" | "yes" => return Ok(true),
            _ => {
                let _ = writeln!(err, "Please answer 'y' or 'n'.");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_server_is_local_daemon() {
        let (target, parsed) = parse_target(None, None);
        assert!(matches!(target, ResolvedTarget::LocalDaemon));
        assert!(parsed.is_none());
    }

    #[test]
    fn bare_hostname_is_ssh() {
        // The historical bug: `kmux focalors` parsed to a Direct target with
        // an empty token, dropping the user into the unrecoverable Connect
        // form. After the SSH-strict change every non-empty server string
        // resolves to an Ssh target.
        let (target, _) = parse_target(Some("focalors"), None);
        match target {
            ResolvedTarget::Ssh { target, .. } => {
                assert_eq!(target.host, "focalors");
                assert!(target.ssh_port.is_none());
            }
            other => panic!("expected Ssh target, got {other:?}"),
        }
    }

    #[test]
    fn host_colon_port_is_ssh_port() {
        // `host:port` (no `@`) is the SSH port, not a daemon data-plane port.
        let (target, _) = parse_target(Some("focalors:2222"), None);
        match target {
            ResolvedTarget::Ssh { target, .. } => {
                assert_eq!(target.host, "focalors");
                assert_eq!(target.ssh_port, Some(2222));
            }
            other => panic!("expected Ssh target, got {other:?}"),
        }
    }

    #[test]
    fn ssh_port_flag_overrides_server_string() {
        let (target, _) = parse_target(Some("focalors:2222"), Some(2223));
        match target {
            ResolvedTarget::Ssh { target, .. } => {
                assert_eq!(target.ssh_port, Some(2223));
            }
            other => panic!("expected Ssh target, got {other:?}"),
        }
    }
}
