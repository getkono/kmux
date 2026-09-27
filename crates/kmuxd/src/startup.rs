use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tracing::{info, warn};

use crate::config::{ListenKind, ServerConfig};
use crate::tls::{CertMaterial, build_server_config};
use kmux_protocol::messages::TransportKind;
use kmux_sys::transport::quic::QuicListener;
use kmux_sys::transport::tcp_tls::TlsTcpListener;
use kmux_sys::transport::uds::UdsListener;
use kmux_sys::transport::{HANDSHAKE_TIMEOUT, IncomingSession, Listener, SessionTransport, serve};

use crate::app::ServerApp;
use crate::auth::{generate_token, persist_token};
use crate::handoff::receiver::Outcome;
use crate::persist::checkpoint::{Checkpointer, PeriodicCheckpointer};
use crate::term_state;
use crate::tls;

pub async fn async_main(daemon: bool, handoff: bool, cfg: ServerConfig) -> anyhow::Result<()> {
    info!(backend = term_state::backend_name(), "terminal backend");

    // Diagnostics (issue #72): log whether network impairment / frame tracing
    // are active so the operator sees the knobs at startup.
    crate::impair::init_and_log();
    crate::trace::init_and_log();

    // Before anything with a side effect: restoring the checkpoint respawns
    // shells, and binding the data socket unlinks whatever is at its path. A
    // second daemon doing either while another is live splits the host's
    // sessions between two daemons. A handoff successor is exempt — its
    // predecessor released the sockets to it.
    if daemon && !handoff {
        crate::daemon::ensure_no_live_daemon(&kmux_sys::dirs::socket_path()?)?;
    }

    // A handoff successor daemonized without taking the pid file (the
    // predecessor holds its lock). Capture the predecessor's pid now — while its
    // pid file still exists — so we can write our own once it has exited.
    let predecessor_pid: Option<i32> = if handoff {
        kmux_sys::dirs::pid_path()
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
    // listeners, so already-connected clients re-auth seamlessly. Without live
    // fds we fall back to a normal snapshot restore; and if the predecessor
    // rolled the handoff back and keeps serving, this daemon must not serve
    // at all (issue #207).
    let mut handoff_outcome = if handoff {
        let predecessor = predecessor_pid.map(nix::unistd::Pid::from_raw);
        match crate::handoff::receiver::run(predecessor).await {
            Outcome::Inherited(inherited) => Some(inherited),
            Outcome::Restore => None,
            Outcome::StandDown => return Ok(()),
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

    // Load (or generate on first run) this daemon's cryptographic identity, so it
    // can report its own `machine_id` to clients and present a verifiable identity
    // when federating to peers (issue #146).
    let server_machine_id = match kmux_sys::identity::Identity::load_or_create() {
        Ok(id) => id.fingerprint(),
        Err(e) => {
            tracing::warn!("Failed to load identity key: {e}");
            String::new()
        }
    };

    let mut app_builder = ServerApp::new(token.clone())
        .with_compression(cfg.compression.clone())
        .with_machine_id(server_machine_id)
        .with_session_isolation(cfg.session_isolation);
    // Closed-session graveyard (issue #64): retain closed sessions for restore.
    match kmux_sys::dirs::closed_sessions_path() {
        Ok(path) => {
            app_builder = app_builder.with_closed_sessions(
                cfg.closed_session_keep,
                cfg.closed_session_ttl_days,
                path,
            );
        }
        Err(e) => warn!("could not determine graveyard path; closed sessions won't persist: {e}"),
    }
    let app = Arc::new(app_builder);
    // Respawn isolated VT workers that crash (issue #126); no-op until a pane
    // actually runs in a worker (`session_isolation = "process"`).
    app.spawn_worker_respawn_task();

    // Restore persisted sessions from the previous daemon instance, if any.
    // With a successful handoff, panes named in `inherited` keep their live
    // process; the rest respawn from the snapshot exactly as a cold start would.
    if let Ok(path) = kmux_sys::dirs::session_state_path()
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

    // Load the closed-session graveyard (issue #64). Done after live restore so
    // it can drop any entry that collides with a just-restored live session
    // (live wins). Stale/over-cap entries are pruned on load; rewrite if so.
    match kmux_sys::dirs::closed_sessions_path() {
        Ok(path) => match crate::persist::graveyard::read_graveyard(&path) {
            Ok(graveyard) => {
                let found = graveyard.sessions.len();
                let changed = app.load_graveyard(graveyard).await;
                info!(
                    retained = found,
                    pruned = changed,
                    "closed-session graveyard loaded"
                );
                if changed {
                    app.persist_graveyard();
                }
            }
            Err(e) => warn!("failed to load closed-session graveyard: {e}"),
        },
        Err(e) => warn!("could not determine graveyard path: {e}"),
    }

    // The one writer of the checkpoint file: the periodic loop, the shutdown
    // path and a graceful handoff all go through it (issue #207).
    // The final write of a shutdown or handoff consumes it (`None` afterwards).
    let mut checkpointer = kmux_sys::dirs::session_state_path()
        .inspect_err(|e| warn!("could not determine checkpoint path: {e}"))
        .ok()
        .map(Checkpointer::new);

    // Periodic checkpoint task, restarted if it panics (issue #206): without
    // it sessions silently stop being persisted for the daemon's lifetime.
    {
        let persist_app = Arc::clone(&app);
        let periodic = checkpointer.as_ref().map(Checkpointer::periodic);
        tokio::spawn(crate::supervisor::supervise("checkpoint", move || {
            checkpoint_loop(Arc::clone(&persist_app), periodic.clone())
        }));
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
                    kmux_sys::dirs::data_socket_path()?
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
    // Each connection's TLS/QUIC handshake runs in its own task under
    // HANDSHAKE_TIMEOUT, so one stalled peer never delays the next accept.
    let mut listener_handles = Vec::new();
    for listener in bound_listeners {
        let app = Arc::clone(&app);
        let on_session = Arc::new(move |session: IncomingSession| {
            tokio::spawn(dispatch_session(session, Arc::clone(&app)));
        });
        listener_handles.push(tokio::spawn(serve(listener, HANDSHAKE_TIMEOUT, on_session)));
    }

    let shutdown = Arc::new(Notify::new());
    // Signals a graceful live-PTY handoff (issue #35); `handoff_in_progress`
    // guards against concurrent restart commands.
    let restart = Arc::new(Notify::new());
    let handoff_in_progress = Arc::new(AtomicBool::new(false));

    // Spawn idle-shutdown watcher when configured.
    if let Some(warning) = cfg.idle_shutdown_warning() {
        warn!("{warning}");
    }
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
            socket_path: kmux_sys::dirs::socket_path()?,
            pid_path: kmux_sys::dirs::pid_path()?,
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
            handoff_successor: handoff,
        };
        let stop = crate::daemon::termination_signal()?;
        tokio::spawn(async move {
            crate::daemon::serve_control_socket(params, stop).await;
        });

        // A handoff successor must claim the pid file itself (it daemonized
        // without one). Wait for the predecessor to exit so its pid-file cleanup
        // can't clobber ours, then write our pid. The control socket already
        // answers `status` (which reports the live pid), so this is not on the
        // critical path for clients.
        if handoff {
            let pid_path = kmux_sys::dirs::pid_path()?;
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

    // `true` once a graceful handoff committed: we wrote the final checkpoint
    // and the successor owns the live PTYs, so we skip our shutdown checkpoint.
    let mut handed_off = false;
    // A handoff runs as a task of its own, so the signals below are still
    // serviced while it does (issue #207).
    let mut in_flight: Option<HandoffInFlight> = None;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => { info!("Received SIGINT, shutting down"); break; }
            _ = sigterm.recv() => { info!("Received SIGTERM, shutting down"); break; }
            _ = shutdown.notified() => { info!("Shutdown requested via control socket"); break; }
            _ = restart.notified(), if in_flight.is_none() => {
                info!("Graceful restart requested; beginning live PTY handoff");
                in_flight = Some(HandoffInFlight::start(&app, checkpointer.take()));
            }
            (committed, returned) = handoff_finished(&mut in_flight, &app) => {
                in_flight = None;
                checkpointer = returned;
                if committed {
                    handed_off = true;
                    break;
                }
                warn!("handoff failed, resuming normal operation");
                handoff_in_progress.store(false, Ordering::SeqCst);
            }
        }
    }
    // A shutdown signal mid-handoff stops it; it may have committed already.
    if let Some(handoff) = in_flight.take() {
        (handed_off, checkpointer) = handoff.abandon(&app).await;
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
    } else if checkpointer.is_some() {
        let shutdown_state = app.checkpoint_state().await;
        match Checkpointer::write_final_from(&mut checkpointer, shutdown_state).await {
            Ok(_sealed) => info!("session state checkpointed on shutdown"),
            Err(e) => warn!("shutdown checkpoint failed: {e}"),
        }
    }

    for endpoint in quic_endpoints {
        endpoint.close(0u32.into(), b"shutdown");
    }
    Ok(())
}

/// Checkpoint every session through `checkpointer` every 30 s, starting at
/// once, and TTL-sweep the closed-session graveyard, forever. With no
/// checkpointer only the sweep runs. The state is gathered on the runtime and
/// written on the blocking pool, and an unchanged state is not rewritten
/// (issue #207). Holds no state between iterations, so the supervisor can
/// restart it after a panic.
async fn checkpoint_loop(app: Arc<ServerApp>, checkpointer: Option<PeriodicCheckpointer>) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if let Some(checkpointer) = &checkpointer {
            let state = app.checkpoint_state().await;
            if let Err(e) = checkpointer.write_in_background(state).await {
                warn!("periodic checkpoint failed: {e}");
            }
        }
        // TTL-sweep the closed-session graveyard (issue #64); rewrites the
        // graveyard file only if something actually expired.
        app.sweep_graveyard();
    }
}

/// A graceful handoff in flight (issue #35). It runs as a task of its own
/// rather than inline in the main loop, so SIGTERM and SIGINT are serviced
/// while it runs (issue #207).
struct HandoffInFlight {
    task: tokio::task::JoinHandle<Ended>,
    /// Set by the sender the moment the successor's `Ack` arrives.
    committed: Arc<AtomicBool>,
}

/// What a handoff task returns: how it ended, and the checkpointer back
/// unless the handoff's final write consumed it.
type Ended = (anyhow::Result<()>, Option<Checkpointer>);

impl HandoffInFlight {
    /// Start a handoff that owns `checkpointer` until it ends.
    fn start(app: &Arc<ServerApp>, checkpointer: Option<Checkpointer>) -> Self {
        let committed = Arc::new(AtomicBool::new(false));
        let app = Arc::clone(app);
        let flag = Arc::clone(&committed);
        let task = tokio::spawn(async move {
            let mut checkpointer = checkpointer;
            let result = crate::handoff::sender::run(&app, &mut checkpointer, &flag).await;
            (result, checkpointer)
        });
        Self { task, committed }
    }

    /// Stop the handoff, for a shutdown signal that arrived mid-way. Returns
    /// whether it had committed, and the checkpointer if it came back.
    async fn abandon(self, app: &ServerApp) -> (bool, Option<Checkpointer>) {
        self.task.abort();
        let ended = self.task.await;
        settle_handoff(ended, &self.committed, app).await
    }
}

/// Wait for the handoff in flight to end, and return whether it committed,
/// with the checkpointer if it came back. Never resolves while there is none.
async fn handoff_finished(
    in_flight: &mut Option<HandoffInFlight>,
    app: &ServerApp,
) -> (bool, Option<Checkpointer>) {
    let Some(handoff) = in_flight else {
        return std::future::pending().await;
    };
    let ended = (&mut handoff.task).await;
    settle_handoff(ended, &handoff.committed, app).await
}

/// Make the daemon consistent with how a handoff ended, whether the sender
/// finished, failed, panicked or was aborted, and return whether it
/// committed (the successor takes over, so this daemon exits).
///
/// Committed: the successor holds every live fd, so our PTY children are kept
/// alive through our exit even if the sender never got that far. A handoff
/// that returned `Ok` without committing was declined, and the successor
/// restores from our final checkpoint. Anything else leaves this daemon
/// serving: its readers are released, which the sender's own rollback does
/// too unless it panicked or was aborted. Its checkpoint was unsealed as the
/// sender's final checkpoint dropped; a panicked or aborted sender's
/// checkpointer is lost with it, so no final write is left to make.
async fn settle_handoff(
    ended: Result<Ended, tokio::task::JoinError>,
    committed: &AtomicBool,
    app: &ServerApp,
) -> (bool, Option<Checkpointer>) {
    let (result, checkpointer) = match ended {
        Ok((result, checkpointer)) => (Ok(result), checkpointer),
        Err(e) => (Err(e), None),
    };
    if committed.load(Ordering::SeqCst) {
        app.manager.set_all_keep_alive(true).await;
        return (true, checkpointer);
    }
    match result {
        Ok(Ok(())) => return (true, checkpointer),
        Ok(Err(e)) => warn!("handoff rolled back: {e:#}"),
        Err(e) => warn!("handoff task ended early: {e}"),
    }
    app.release_relays().await;
    (false, checkpointer)
}

/// Dispatch a newly accepted session to the appropriate transport handler.
async fn dispatch_session(session: IncomingSession, app: Arc<ServerApp>) {
    use tracing::Instrument;
    let span = session.span.clone();
    let kind = session.kind();
    match session.transport {
        SessionTransport::Quic(conn) => {
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
        // The stream transports: the type admits no QUIC here, so `kind` is
        // one of them.
        SessionTransport::Uds | SessionTransport::Tcp | SessionTransport::TcpTls => {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A handoff in flight that runs `task`, committed or not.
    fn fixture_handoff(
        task: impl Future<Output = anyhow::Result<()>> + Send + 'static,
        committed: bool,
    ) -> HandoffInFlight {
        HandoffInFlight {
            task: tokio::spawn(async move { (task.await, None) }),
            committed: Arc::new(AtomicBool::new(committed)),
        }
    }

    /// The main loop learns how a handoff ended: a declined (or committed)
    /// one hands over, a failed one does not, and with none in flight there
    /// is nothing to learn.
    #[tokio::test]
    async fn a_finished_handoff_reports_whether_it_hands_over() {
        let app = crate::fixtures::fixture_app();
        let mut none = None;
        let waited =
            tokio::time::timeout(Duration::from_millis(50), handoff_finished(&mut none, &app))
                .await;
        assert!(waited.is_err(), "nothing in flight never finishes");

        let mut declined = Some(fixture_handoff(async { Ok(()) }, false));
        assert!(handoff_finished(&mut declined, &app).await.0);
        let mut failed = Some(fixture_handoff(
            async { Err(anyhow::anyhow!("no Ack")) },
            false,
        ));
        assert!(!handoff_finished(&mut failed, &app).await.0);
    }

    /// A handoff stopped by a shutdown signal counts as handed over only if
    /// it had already committed.
    #[tokio::test]
    async fn an_abandoned_handoff_hands_over_only_if_it_had_committed() {
        let app = crate::fixtures::fixture_app();
        let stuck = || std::future::pending::<anyhow::Result<()>>();
        assert!(fixture_handoff(stuck(), true).abandon(&app).await.0);
        assert!(!fixture_handoff(stuck(), false).abandon(&app).await.0);
    }

    /// How a handoff ended decides whether this daemon exits: a commit (even
    /// one whose sender then failed) or a decline hands over; a failure, a
    /// panic or an abort before the commit point leaves it serving, with the
    /// checkpointer it handed back.
    #[tokio::test]
    async fn settling_a_handoff_hands_over_only_when_committed_or_declined() {
        let app = crate::fixtures::fixture_app();
        let tmp = tempfile::tempdir().unwrap();
        let committed = AtomicBool::new(true);
        let not_committed = AtomicBool::new(false);
        let aborted = {
            let task = tokio::spawn(std::future::pending::<Ended>());
            task.abort();
            task.await
        };
        let checkpointer = || Some(Checkpointer::new(tmp.path().join("state.bin")));

        let failed = |c| Ok((Err(anyhow::anyhow!("no Ack")), c));
        assert!(settle_handoff(failed(None), &committed, &app).await.0);
        assert!(
            settle_handoff(Ok((Ok(()), None)), &not_committed, &app)
                .await
                .0
        );

        let (handed_over, back) =
            settle_handoff(failed(checkpointer()), &not_committed, &app).await;
        assert!(!handed_over);
        assert!(back.is_some(), "the rolled-back handoff's checkpointer");
        let (handed_over, back) = settle_handoff(aborted, &not_committed, &app).await;
        assert!(!handed_over);
        assert!(back.is_none());
    }

    /// The first checkpoint is written as soon as the loop starts, and reads
    /// back as a daemon state.
    #[tokio::test]
    async fn checkpoint_loop_writes_a_readable_checkpoint_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.bin");
        let app = Arc::new(crate::fixtures::fixture_app());
        let checkpointer = Checkpointer::new(path.clone());
        let task = tokio::spawn(checkpoint_loop(app, Some(checkpointer.periodic())));

        tokio::time::timeout(Duration::from_secs(10), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a checkpoint is written");
        task.abort();

        let state = crate::persist::restore::read_checkpoint(&path).expect("readable");
        assert!(state.sessions.is_empty());
    }
}
