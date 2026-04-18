use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tracing::{info, warn};

use crate::config::{ListenKind, ServerConfig};
use crate::tls::{CertMaterial, build_server_config};
use kmux_protocol::messages::TransportKind;
use kmux_protocol::transport::quic::QuicListener;
use kmux_protocol::transport::tcp_tls::TlsTcpListener;
use kmux_protocol::transport::uds::UdsListener;
use kmux_protocol::transport::{AcceptError, IncomingSession, Listener};

use crate::app::ServerApp;
use crate::auth::{generate_token, persist_token};
use crate::term_state;
use crate::tls;

pub async fn async_main(daemon: bool, cfg: ServerConfig) -> anyhow::Result<()> {
    info!(backend = term_state::BACKEND_NAME, "terminal backend");
    info!(
        runtime_dir = %cfg.runtime_dir,
        allow_peer_cred = cfg.auth.allow_peer_cred,
        "effective configuration loaded"
    );

    // ── TLS material ───────────────────────────────────────────────────────────
    let material = if cfg.tls.self_signed {
        CertMaterial::self_signed()?
    } else {
        let cert_path = cfg.tls.cert.ok_or_else(|| {
            anyhow::anyhow!(
                "TLS cert path required (set [tls] cert in kmuxd.toml or use --self-signed)"
            )
        })?;
        let key_path = cfg.tls.key.ok_or_else(|| {
            anyhow::anyhow!("TLS key path required (set [tls] key in kmuxd.toml)")
        })?;
        CertMaterial::from_files(&cert_path, &key_path)?
    };

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

    // Periodic checkpoint task.
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

    // ── Build and bind all configured listeners ────────────────────────────────
    // Track resolved listener configs with actual bound ports (replacing port=0).
    let mut resolved_listeners = cfg.listeners.clone();
    let mut bound_listeners: Vec<Box<dyn Listener>> = Vec::new();
    // Track QUIC endpoints so we can close them on shutdown.
    let mut quic_endpoints: Vec<quinn::Endpoint> = Vec::new();
    // Ports for the daemon control socket (first QUIC + first TCP+TLS).
    let mut quic_port: u16 = 0;
    let mut tcp_port: u16 = 0;

    for (i, listener_cfg) in cfg.listeners.iter().enumerate() {
        if !listener_cfg.enabled {
            continue;
        }
        match listener_cfg.kind {
            ListenKind::Quic => {
                let addr: std::net::SocketAddr =
                    format!("{}:{}", listener_cfg.bind, listener_cfg.port).parse()?;
                let quinn_config = tls::build_quinn_config(build_server_config(material.clone())?)?;
                let endpoint = quinn::Endpoint::server(quinn_config, addr)?;
                let actual_addr = endpoint.local_addr()?;
                info!("Listening on quic://{actual_addr}");
                if quic_port == 0 {
                    quic_port = actual_addr.port();
                }
                resolved_listeners[i].port = actual_addr.port();
                quic_endpoints.push(endpoint.clone());
                bound_listeners.push(Box::new(QuicListener::new(endpoint)));
            }
            ListenKind::TcpTls => {
                let addr: std::net::SocketAddr =
                    format!("{}:{}", listener_cfg.bind, listener_cfg.port).parse()?;
                let tcp_cfg = build_server_config(material.clone())?;
                let tls_listener = TlsTcpListener::bind(addr, tcp_cfg).await?;
                let actual_addr = tls_listener.local_addr()?;
                info!("Listening on tcp+tls://{actual_addr}");
                if tcp_port == 0 {
                    tcp_port = actual_addr.port();
                }
                resolved_listeners[i].port = actual_addr.port();
                bound_listeners.push(Box::new(tls_listener));
            }
            ListenKind::Unix => {
                let path = if listener_cfg.path == "auto" {
                    kmux_protocol::dirs::data_socket_path()?
                } else {
                    std::path::PathBuf::from(&listener_cfg.path)
                };
                let uds_listener = UdsListener::bind(&path)?;
                // Resolve "auto" in the config so announce.rs has the real path.
                resolved_listeners[i].path = path.to_string_lossy().into_owned();
                info!("Listening on unix://{}", path.display());
                bound_listeners.push(Box::new(uds_listener));
            }
        }
    }

    // ── Spawn one accept-loop task per listener ────────────────────────────────
    let mut listener_handles = Vec::new();
    for mut listener in bound_listeners {
        let app = Arc::clone(&app);
        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok(session) => {
                        let app = Arc::clone(&app);
                        tokio::spawn(async move {
                            dispatch_session(session, app).await;
                        });
                    }
                    Err(AcceptError::Closed) => break,
                    Err(e) => warn!("accept error on {} listener: {e}", listener.kind()),
                }
            }
        });
        listener_handles.push(handle);
    }

    let shutdown = Arc::new(Notify::new());

    if daemon {
        let params = crate::daemon::ControlSocketParams {
            socket_path: kmux_protocol::dirs::socket_path()?,
            pid_path: kmux_protocol::dirs::pid_path()?,
            quic_port,
            tcp_port,
            token: token.clone(),
            start_time: Instant::now(),
            app: Arc::clone(&app),
            shutdown: Arc::clone(&shutdown),
            listeners: resolved_listeners,
            public_host: cfg.advertise.public_host.clone(),
        };
        tokio::spawn(async move {
            crate::daemon::serve_control_socket(params).await;
        });
    }

    // Install signal handlers and wait for any shutdown signal.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    tokio::select! {
        _ = tokio::signal::ctrl_c() => { info!("Received SIGINT, shutting down"); }
        _ = sigterm.recv() => { info!("Received SIGTERM, shutting down"); }
        _ = shutdown.notified() => { info!("Shutdown requested via control socket"); }
    }

    // Abort listener tasks.
    for handle in listener_handles {
        handle.abort();
    }

    // Clean shutdown: checkpoint the full session state.
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

    for endpoint in quic_endpoints {
        endpoint.close(0u32.into(), b"shutdown");
    }
    Ok(())
}

/// Dispatch a newly accepted session to the appropriate transport handler.
async fn dispatch_session(session: IncomingSession, app: Arc<ServerApp>) {
    use tracing::Instrument;
    let span = session.span.clone();
    match session.kind {
        TransportKind::Quic => {
            let conn = *session
                .extra
                .downcast::<quinn::Connection>()
                .expect("QUIC IncomingSession must carry quinn::Connection in extra");
            crate::connection::handle_with_io(
                session.read,
                session.write,
                conn,
                app,
                TransportKind::Quic,
                span.clone(),
            )
            .instrument(span)
            .await;
        }
        kind @ (TransportKind::Tcp | TransportKind::TcpTls | TransportKind::Uds) => {
            crate::tcp_listener::handle_tcp_io(
                session.read,
                session.write,
                app,
                kind,
                span.clone(),
            )
            .instrument(span)
            .await;
        }
    }
}
