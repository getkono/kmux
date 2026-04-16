use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use tracing::{info, warn};

use crate::app::ServerApp;
use crate::auth::{generate_token, persist_token};
use crate::term_state;
use crate::tls;

use super::Cli;

pub async fn async_main(cli: Cli) -> anyhow::Result<()> {
    info!(backend = term_state::BACKEND_NAME, "terminal backend");

    let tls_config = if cli.self_signed {
        let (cert, key) = tls::generate_self_signed()?;
        tls::build_tls_config(cert, key)?
    } else {
        let cert_path = cli
            .cert
            .ok_or_else(|| anyhow::anyhow!("--cert is required without --self-signed"))?;
        let key_path = cli
            .key
            .ok_or_else(|| anyhow::anyhow!("--key is required without --self-signed"))?;
        tls::load_tls_config(&cert_path, &key_path)?
    };

    let quinn_config = tls::build_quinn_config(tls_config)?;

    let token = generate_token();
    match persist_token(&token) {
        Ok(path) => info!("Auth token persisted to {}", path.display()),
        Err(e) => tracing::warn!("Failed to persist auth token: {e}"),
    }
    println!("Auth token: {token}");

    let app = Arc::new(ServerApp::new(token.clone()));

    // Restore persisted sessions from the previous daemon instance, if any.
    if let Ok(path) = kmux_protocol::dirs::session_state_path()
        && path.exists()
    {
        match crate::persist::restore::read_checkpoint(&path) {
            Ok(state) => {
                let report = app.restore_from(state).await;
                info!(
                    restored = report.restored,
                    alive = report.alive,
                    dead = report.dead,
                    "session restore complete"
                );
            }
            Err(e) => warn!("failed to restore sessions from checkpoint: {e}"),
        }
    }

    // Periodic checkpoint task: saves session state every 30 seconds for
    // crash recovery. Does NOT set keep_alive (children may still be killed
    // by the kernel if the daemon crashes).
    {
        let persist_app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let state = persist_app.checkpoint_state().await;
                match kmux_protocol::dirs::session_state_path() {
                    Ok(path) => {
                        if let Err(e) = crate::persist::checkpoint::write_checkpoint(&state, &path)
                        {
                            warn!("periodic checkpoint failed: {e}");
                        }
                    }
                    Err(e) => warn!("could not determine checkpoint path: {e}"),
                }
            }
        });
    }

    let addr: SocketAddr = format!("{}:{}", cli.bind, cli.port).parse()?;
    let endpoint = quinn::Endpoint::server(quinn_config, addr)?;
    let actual_addr = endpoint.local_addr()?;
    let actual_port = actual_addr.port();
    info!("Listening on quic://{actual_addr}");

    // Start the TCP fallback/tunnel transport listener.
    let tcp_bind: SocketAddr = format!("{}:{}", cli.bind, cli.tcp_port).parse()?;
    let tcp_port = crate::tcp_listener::serve_tcp(tcp_bind, Arc::clone(&app)).await?;

    let shutdown = Arc::new(Notify::new());

    if cli.daemon {
        let socket_path = kmux_protocol::dirs::socket_path()?;
        let pid_path = kmux_protocol::dirs::pid_path()?;
        let start_time = Instant::now();
        let token_clone = token.clone();
        let app_clone = Arc::clone(&app);
        let shutdown_clone = Arc::clone(&shutdown);
        tokio::spawn(async move {
            crate::daemon::serve_control_socket(
                socket_path,
                pid_path,
                actual_port,
                tcp_port,
                token_clone,
                start_time,
                app_clone,
                shutdown_clone,
            )
            .await;
        });
    }

    // Install signal handlers for the foreground (non-daemon) case.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                match incoming {
                    Some(incoming) => {
                        let app = Arc::clone(&app);
                        tokio::spawn(async move {
                            match incoming.await {
                                Ok(conn) => {
                                    let remote = conn.remote_address();
                                    info!("QUIC connection from {remote}");
                                    crate::connection::handle(conn, app).await;
                                }
                                Err(e) => tracing::error!("QUIC connection failed: {e}"),
                            }
                        });
                    }
                    None => break,
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down");
                break;
            }
            _ = shutdown.notified() => {
                info!("Shutdown requested via control socket");
                break;
            }
        }
    }

    // Clean shutdown: checkpoint the full session state so the next daemon
    // start can replay the visual content as preamble in fresh shells.
    let shutdown_state = app.checkpoint_state().await;
    match kmux_protocol::dirs::session_state_path() {
        Ok(path) => {
            if let Err(e) = crate::persist::checkpoint::write_checkpoint(&shutdown_state, &path) {
                warn!("shutdown checkpoint failed: {e}");
            } else {
                info!("session state checkpointed on shutdown");
            }
        }
        Err(e) => warn!("could not determine checkpoint path on shutdown: {e}"),
    }

    endpoint.close(0u32.into(), b"shutdown");
    Ok(())
}
