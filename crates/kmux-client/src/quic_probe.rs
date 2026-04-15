use kmux_protocol::messages::{ClientCapabilities, ClientMessage, ConnectionId, ServerMessage};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::connect::{self, ConnectResult};

/// Result of a successful QUIC probe.
pub struct UpgradeReady {
    pub sender: mpsc::UnboundedSender<ClientMessage>,
}

/// Parameters for the QUIC upgrade probe loop.
pub struct QuicProbeParams {
    pub remote_host: String,
    pub quic_port: u16,
    pub token: String,
    pub connection_id: ConnectionId,
    pub capabilities: ClientCapabilities,
    pub accept_invalid_certs: bool,
    pub srv_tx: mpsc::UnboundedSender<ServerMessage>,
    pub upgrade_tx: mpsc::Sender<UpgradeReady>,
    /// Maximum consecutive failures before stopping (0 = unlimited).
    pub max_failures: u32,
}

/// Periodically attempts a direct QUIC connection to the remote host.
/// On success, sends the new sender via `params.upgrade_tx` so the caller can
/// swap the active transport.  Stops after `max_failures` consecutive failures
/// or once an upgrade has been delivered.
pub async fn quic_upgrade_loop(params: QuicProbeParams) {
    const INITIAL_DELAY_SECS: u64 = 2;
    const RETRY_INTERVAL_SECS: u64 = 30;

    let QuicProbeParams {
        remote_host,
        quic_port,
        token,
        connection_id,
        capabilities,
        accept_invalid_certs,
        srv_tx,
        upgrade_tx,
        max_failures,
    } = params;

    tokio::time::sleep(tokio::time::Duration::from_secs(INITIAL_DELAY_SECS)).await;

    let mut failures: u32 = 0;
    loop {
        debug!(
            host = %remote_host,
            port = quic_port,
            attempt = failures + 1,
            "Attempting QUIC upgrade probe"
        );

        let result = connect::connect(
            remote_host.clone(),
            quic_port,
            token.clone(),
            accept_invalid_certs,
            srv_tx.clone(),
            capabilities.clone(),
            Some(connection_id),
        )
        .await;

        match result {
            ConnectResult::Connected(sender) => {
                info!(
                    host = %remote_host,
                    port = quic_port,
                    "QUIC upgrade probe succeeded; signalling channel switch"
                );
                let _ = upgrade_tx.send(UpgradeReady { sender }).await;
                return;
            }
            ConnectResult::Failed(e) => {
                failures += 1;
                debug!(
                    host = %remote_host,
                    port = quic_port,
                    error = %e,
                    failures,
                    "QUIC upgrade probe failed"
                );
                if max_failures > 0 && failures >= max_failures {
                    info!(
                        host = %remote_host,
                        "QUIC upgrade probe stopped after {failures} consecutive failures"
                    );
                    return;
                }
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(RETRY_INTERVAL_SECS)).await;
    }
}
