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

pub async fn async_main(daemon: bool, handoff: bool, cfg: ServerConfig) -> anyhow::Result<()> {
    info!(backend = term_state::backend_name(), "terminal backend");

    // Diagnostics (issue #72): log whether network impairment / frame tracing
    // are active so the operator sees the knobs at startup.
    crate::impair::init_and_log();
    crate::trace::init_and_log();

    // A handoff successor daemonized without taking the pid file (the
    // predecessor holds its lock). Capture the predecessor's pid now — while its
    // pid file still exists — so we can write our own once it has exited.
    let predecessor_pid: Option<i32> = if handoff {
        kmux_protocol::dirs::pid_path()
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| s.trim().parse::<i32>().ok())
    } else {
        None
    };
    info!(
        runtime_dir = %cfg.runtime_dir,
        allow_peer_cred = cfg.auth.allow_peer_cred,
        "effective configuration loaded"
    );

    // ── TLS material ───────────────────────────────────────────────────────────
    // A configured cert/key pair loads a custom certificate; otherwise the
    // daemon generates an in-memory self-signed certificate. Self-signed is the
    // default for this kind of software, so it needs no flag or config knob
    // (issue #100). `ServerConfig::resolve` has already rejected a half-set pair.
    let material = match (&cfg.tls.cert, &cfg.tls.key) {
        (Some(cert_path), Some(key_path)) => CertMaterial::from_files(cert_path, key_path)?,
        _ => CertMaterial::self_signed()?,
    };

    // When launched as a graceful-restart successor (`--handoff`), pull the
    // predecessor's live PTY fds and adopt its auth token *before* binding any
    // listeners, so already-connected clients re-auth seamlessly. On any failure
    // this returns `None` and we fall back to a normal snapshot restore.
    let mut handoff_outcome = if handoff {
        match crate::handoff::receiver::run().await {
            Ok(outcome) => outcome,
            Err(e) => {
                warn!("handoff receive failed: {e}; falling back to snapshot restore");
                None
            }
        }
    } else {
        None
    };

    let token = match &handoff_outcome {
        Some(o) => o.token.clone(),
        None => generate_token(),
    };
    match persist_token(&token) {
        Ok(path) => info!("Auth token persisted to {}", path.display()),
        Err(e) => tracing::warn!("Failed to persist auth token: {e}"),
    }
    println!("Auth token: {token}");

    let app = Arc::new(ServerApp::new(token.clone()).with_compression(cfg.compression.clone()));

    // Restore persisted sessions from the previous daemon instance, if any.
    // With a successful handoff, panes named in `inherited` keep their live
    // process; the rest respawn from the snapshot exactly as a cold start would.
    if let Ok(path) = kmux_protocol::dirs::session_state_path()
        && path.exists()
    {
        match crate::persist::restore::read_checkpoint(&path) {
            Ok(state) => {
                let report = match handoff_outcome.take() {
                    Some(o) => app.restore_with_handoff(state, o.inherited).await,
                    None => app.restore_from(state).await,
                };
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
    if handoff_outcome.is_some() {
        warn!("handoff: live fds received but no usable checkpoint; cannot reconstruct panes");
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
    // Signals a graceful live-PTY handoff (issue #35); `handoff_in_progress`
    // guards against concurrent restart commands.
    let restart = Arc::new(Notify::new());
    let handoff_in_progress = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Spawn idle-shutdown watcher when configured.
    if cfg.idle_shutdown_secs > 0 {
        let idle_secs = cfg.idle_shutdown_secs;
        let mut count_rx = app.conn_count_rx();
        let shutdown_idle = Arc::clone(&shutdown);
        tokio::spawn(async move {
            use std::time::Duration;
            loop {
                // Wait for any connection-count change.
                if count_rx.changed().await.is_err() {
                    break; // sender dropped → daemon shutting down
                }
                let count = *count_rx.borrow();
                if count == 0 {
                    // Debounce: wait idle_secs, but cancel if a client connects.
                    let idle = tokio::time::sleep(Duration::from_secs(idle_secs));
                    tokio::pin!(idle);
                    loop {
                        tokio::select! {
                            () = &mut idle => {
                                info!(idle_secs, "idle shutdown: no clients for {idle_secs}s");
                                shutdown_idle.notify_waiters();
                                return;
                            }
                            changed = count_rx.changed() => {
                                if changed.is_err() { return; }
                                if *count_rx.borrow() > 0 {
                                    break; // client reconnected; restart outer loop
                                }
                                // count changed but still 0 (spurious); reset debounce
                                idle.as_mut().reset(
                                    tokio::time::Instant::now() + Duration::from_secs(idle_secs)
                                );
                            }
                        }
                    }
                }
            }
        });
    }

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
            restart: Arc::clone(&restart),
            handoff_in_progress: Arc::clone(&handoff_in_progress),
            listeners: resolved_listeners,
            public_host: cfg.advertise.public_host.clone(),
        };
        tokio::spawn(async move {
            crate::daemon::serve_control_socket(params).await;
        });

        // A handoff successor must claim the pid file itself (it daemonized
        // without one). Wait for the predecessor to exit so its pid-file cleanup
        // can't clobber ours, then write our pid. The control socket already
        // answers `status` (which reports the live pid), so this is not on the
        // critical path for clients.
        if handoff {
            let pid_path = kmux_protocol::dirs::pid_path()?;
            tokio::spawn(async move {
                if let Some(raw) = predecessor_pid {
                    let pid = nix::unistd::Pid::from_raw(raw);
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while nix::sys::signal::kill(pid, None).is_ok() && Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                match std::fs::write(&pid_path, std::process::id().to_string()) {
                    Ok(()) => info!("claimed pid file after predecessor exit"),
                    Err(e) => warn!("failed to write pid file after handoff: {e}"),
                }
            });
        }
    }

    // Install signal handlers and wait for a shutdown or restart signal.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    // `true` once a graceful handoff committed: the successor wrote the
    // checkpoint and owns the live PTYs, so we skip our own shutdown checkpoint.
    let mut handed_off = false;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => { info!("Received SIGINT, shutting down"); break; }
            _ = sigterm.recv() => { info!("Received SIGTERM, shutting down"); break; }
            _ = shutdown.notified() => { info!("Shutdown requested via control socket"); break; }
            _ = restart.notified() => {
                info!("Graceful restart requested; beginning live PTY handoff");
                match crate::handoff::sender::run(&app).await {
                    Ok(()) => { handed_off = true; break; }
                    Err(e) => {
                        warn!("handoff failed, resuming normal operation: {e}");
                        // Nothing destructive happened before the commit point —
                        // clear the guard and keep serving.
                        handoff_in_progress.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            }
        }
    }

    // Abort listener tasks.
    for handle in listener_handles {
        handle.abort();
    }

    // Tear down federated peer links synchronously before the runtime is dropped:
    // their `ssh -L` tunnel children are not kill-on-drop and would otherwise
    // orphan when the process exits. Applies to every shutdown path (incl. a
    // committed handoff — peer links are not migrated; the successor re-federates
    // when GUIs reconnect). A no-op when no peers are open / federation is off.
    app.close_all_peers();

    // Clean shutdown: checkpoint the full session state — unless a handoff
    // already wrote a fresh (post-quiesce) checkpoint and owns the live PTYs.
    if handed_off {
        info!("handoff committed; successor owns the live sessions");
    } else {
        let shutdown_state = app.checkpoint_state().await;
        match kmux_protocol::dirs::session_state_path() {
            Ok(path) => {
                if let Err(e) = crate::persist::checkpoint::write_checkpoint(&shutdown_state, &path)
                {
                    warn!("shutdown checkpoint failed: {e}");
                } else {
                    info!("session state checkpointed on shutdown");
                }
            }
            Err(e) => warn!("could not determine checkpoint path on shutdown: {e}"),
        }
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
