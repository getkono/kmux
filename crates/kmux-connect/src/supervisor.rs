//! Transport supervisor: scores, probes, and signals hot-swap of transports.
//!
//! Phase 6: replaces `quic_probe.rs` with a generalised, scorer-driven
//! background task that considers all advertised endpoints (UDS, QUIC, TCP+TLS)
//! and continuously probes for better transports.
//!
//! ## Scoring formula
//!
//! ```text
//! score =
//!     locality_bonus          // +1000 for UDS when target is local
//!   + robustness_weight       // UDS=30, QUIC=20, TCP+TLS=10
//!   + server_priority         // admin-controlled override
//!   - latency_ms_ewma         // RTT measurement; unknown → 500 ms
//!   - failure_penalty         // 100 per recent failure (5-min window)
//!   - oscillation_penalty     // 200 if swapped AWAY within last 60 s
//! ```
//!
//! Every decision is logged at `INFO` to `kmux::transport::scorer`.

use std::time::{Duration, Instant};

#[cfg(feature = "remote")]
use kmux_protocol::messages::{ClientCapabilities, ConnectionId, ServerMessage};
use kmux_protocol::messages::{ClientMessage, TransportKind};
use kmux_sys::transport::EndpointAdvert;
use tokio::sync::mpsc;
#[cfg(feature = "remote")]
use tokio::sync::oneshot;
#[cfg(feature = "remote")]
use tokio::time::timeout;
use tracing::info;
#[cfg(feature = "remote")]
use tracing::{debug, warn};

// ─── Scoring constants ────────────────────────────────────────────────────────

const LOCALITY_BONUS_UDS: i32 = 1000;
const ROBUSTNESS_UDS: i32 = 30;
const ROBUSTNESS_QUIC: i32 = 20;
const ROBUSTNESS_TCP_TLS: i32 = 10;
const LATENCY_UNKNOWN_MS: i32 = 500;
const FAILURE_PENALTY_PER: i32 = 100;
const OSCILLATION_PENALTY: i32 = 200;
const FAILURE_WINDOW: Duration = Duration::from_secs(300); // 5 min
const OSCILLATION_WINDOW: Duration = Duration::from_secs(60); // 60 s
pub const PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// How long a probe waits for `AuthResult` after the underlying connect call
/// returns. The probe is considered successful only after the server has
/// affirmatively authenticated on the new transport — opening the stream is
/// not enough (a misconfigured server, expired token, or auth mismatch
/// would otherwise look like a successful upgrade and silently disconnect
/// the live channel when the rejected `AuthResult` arrives).
#[cfg(feature = "remote")]
const PROBE_AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap on the underlying connect call. Quinn's handshake otherwise
/// respects `QUIC_IDLE_TIMEOUT_SECS` (300 s), so a port that silently
/// drops UDP would block the supervisor loop for five minutes per probe
/// — long enough that no second scorer log ever reaches the user. With
/// this timeout, a stuck probe surfaces as a `warn!` within ~10 s and
/// the next 30 s tick gets a chance to run.
#[cfg(feature = "remote")]
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

// ─── EndpointHealth ──────────────────────────────────────────────────────────

/// Mutable per-endpoint health state used by the scorer.
#[derive(Debug, Clone)]
pub struct EndpointHealth {
    pub kind: TransportKind,
    /// Connection address: `"host:port"` for QUIC/TLS-TCP, absolute path for UDS.
    pub address: String,
    /// EWMA of round-trip time in ms. `None` = never measured (→ 500 ms assumed).
    pub rtt_ewma_ms: Option<f64>,
    /// Number of failures recorded.
    pub failure_count: u32,
    /// Timestamp of the most recent failure.
    pub last_failure: Option<Instant>,
    /// Timestamp when we last swapped AWAY from this transport (hysteresis timer).
    pub last_swap_away: Option<Instant>,
    /// Server-assigned priority from `EndpointAdvert` (default 0).
    pub server_priority: i32,
}

impl EndpointHealth {
    pub fn new(advert: &EndpointAdvert) -> Self {
        Self {
            kind: advert.kind,
            address: advert.address.clone(),
            rtt_ewma_ms: None,
            failure_count: 0,
            last_failure: None,
            last_swap_away: None,
            server_priority: 0,
        }
    }

    /// Update RTT EWMA with a new measurement (α = 0.2).
    pub fn record_rtt(&mut self, rtt_ms: f64) {
        const ALPHA: f64 = 0.2;
        self.rtt_ewma_ms = Some(match self.rtt_ewma_ms {
            Some(old) => old * (1.0 - ALPHA) + rtt_ms * ALPHA,
            None => rtt_ms,
        });
    }

    /// Record a connect or I/O failure.
    pub fn record_failure(&mut self) {
        self.failure_count += 1;
        self.last_failure = Some(Instant::now());
    }

    /// Record a successful send (decays failure count by 1).
    pub fn record_success(&mut self) {
        self.failure_count = self.failure_count.saturating_sub(1);
    }

    /// Start hysteresis timer: we just swapped AWAY from this transport.
    pub fn mark_swap_away(&mut self) {
        self.last_swap_away = Some(Instant::now());
    }
}

// ─── TransportScorer ──────────────────────────────────────────────────────────

/// Deterministic transport scorer.
///
/// No ICMP or OS-level probes are used. All inputs are application-layer
/// measurements. Every decision is emitted as a `tracing::info!` log.
pub struct TransportScorer {
    /// Whether the connection target has been detected as local.
    pub is_local: bool,
}

impl TransportScorer {
    pub fn new(is_local: bool) -> Self {
        Self { is_local }
    }

    /// Compute score for a single endpoint. Higher = more preferred.
    pub fn score(&self, health: &EndpointHealth) -> i32 {
        let mut score = 0i32;

        // Locality bonus: UDS always wins for local connections.
        if self.is_local && health.kind == TransportKind::Uds {
            score += LOCALITY_BONUS_UDS;
        }

        // Robustness weight (transport-inherent quality).
        score += match health.kind {
            TransportKind::Uds => ROBUSTNESS_UDS,
            TransportKind::Quic => ROBUSTNESS_QUIC,
            TransportKind::TcpTls => ROBUSTNESS_TCP_TLS,
            TransportKind::Tcp => ROBUSTNESS_TCP_TLS - 5, // legacy, discouraged
        };

        // Server-assigned priority.
        score += health.server_priority;

        // Latency penalty.
        let latency_ms = health.rtt_ewma_ms.unwrap_or(LATENCY_UNKNOWN_MS as f64) as i32;
        score -= latency_ms;

        // Failure penalty: only count failures within the 5-minute window.
        if health.failure_count > 0 {
            let in_window = health
                .last_failure
                .is_some_and(|t| t.elapsed() < FAILURE_WINDOW);
            if in_window {
                score -= (health.failure_count as i32) * FAILURE_PENALTY_PER;
            }
        }

        // Oscillation penalty: penalise if we recently swapped away from this.
        if let Some(swapped_at) = health.last_swap_away
            && swapped_at.elapsed() < OSCILLATION_WINDOW
        {
            score -= OSCILLATION_PENALTY;
        }

        score
    }

    /// Score all endpoints and return `(score, index)` pairs sorted best-first.
    ///
    /// Logs the full scoreboard at `info` for transparency (no hidden states).
    /// `active` is the transport currently carrying traffic; it is included
    /// in the log so readers can tell apart "scorer's preference" from
    /// "what's actually in use right now". The two only converge after a
    /// successful probe + upgrade.
    pub fn rank(&self, endpoints: &[EndpointHealth], active: TransportKind) -> Vec<(i32, usize)> {
        let mut scored: Vec<(i32, usize)> = endpoints
            .iter()
            .enumerate()
            .map(|(i, h)| (self.score(h), i))
            .collect();
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));

        let top_ranked = scored
            .first()
            .map(|(_, i)| endpoints[*i].kind.to_string())
            .unwrap_or_default();
        info!(
            target: "kmux::transport::scorer",
            scores = ?scored
                .iter()
                .map(|(s, i)| format!("{}:{s}", endpoints[*i].kind))
                .collect::<Vec<_>>(),
            top_ranked = %top_ranked,
            active = %active,
            "scorer ranking (top_ranked is the candidate to probe; active is the live transport)"
        );

        scored
    }
}

// ─── UpgradeSignal ────────────────────────────────────────────────────────────

/// Signal from `TransportSupervisor`: a better transport is ready to become active.
///
/// The caller should send `ChannelReady` on `sender` and then call the
/// appropriate `apply_*` method on `SessionManager`.
pub struct UpgradeSignal {
    /// The transport kind that is now preferred.
    pub new_kind: TransportKind,
    /// Authenticated sender on the new transport channel.
    pub sender: mpsc::UnboundedSender<ClientMessage>,
}

// ─── RttSample ────────────────────────────────────────────────────────────────

/// One RTT observation forwarded from the `SessionManager` (on every `Pong`)
/// into the supervisor so the scorer operates on live measurements instead
/// of `LATENCY_UNKNOWN_MS` for the currently-active transport.
#[derive(Debug, Clone, Copy)]
pub struct RttSample {
    pub kind: TransportKind,
    pub rtt_ms: f64,
}

// ─── SupervisorParams ─────────────────────────────────────────────────────────

/// Parameters for `TransportSupervisor::new`.
#[cfg(feature = "remote")]
pub struct SupervisorParams {
    /// All endpoints the server has advertised.
    pub endpoints: Vec<EndpointAdvert>,
    /// Session identity for transport resumption.
    pub connection_id: ConnectionId,
    pub token: String,
    pub capabilities: ClientCapabilities,
    pub accept_invalid_certs: bool,
    /// Transport currently active (so the supervisor probes non-active ones).
    pub active_transport: TransportKind,
    /// Whether the target host is local (affects locality scoring).
    pub is_local: bool,
    pub server_tx: mpsc::UnboundedSender<ServerMessage>,
    pub upgrade_tx: mpsc::Sender<UpgradeSignal>,
    /// Stream of RTT observations pushed by the `SessionManager` on every
    /// `Pong`. When `None`, the supervisor falls back to `LATENCY_UNKNOWN_MS`
    /// for all endpoints.
    pub rtt_rx: Option<mpsc::UnboundedReceiver<RttSample>>,
    /// Transport override seeded at spawn (issue #69). `Some(kind)` pins the
    /// transport and disables the heuristic; `None` is normal auto mode.
    /// Persists across reconnects because the `SessionManager` re-seeds it
    /// each time a supervisor is spawned.
    pub forced: Option<TransportKind>,
    /// Live override updates: `Some(kind)` engages a forced transport, `None`
    /// returns to auto. A command on this channel takes effect immediately
    /// (it wakes the probe loop), so the indicator and behaviour update at once.
    pub override_rx: Option<mpsc::UnboundedReceiver<Option<TransportKind>>>,
}

// ─── TransportSupervisor ─────────────────────────────────────────────────────

/// Background task: probes candidate transports and signals hot-swap when a
/// better one becomes available.
///
/// Spawned after bootstrap by calling `tokio::spawn(supervisor.run())`.
/// Exits when `upgrade_tx` is dropped (caller shut down).
#[cfg(feature = "remote")]
pub struct TransportSupervisor {
    endpoints: Vec<EndpointHealth>,
    connection_id: ConnectionId,
    token: String,
    capabilities: ClientCapabilities,
    accept_invalid_certs: bool,
    active_transport: TransportKind,
    scorer: TransportScorer,
    server_tx: mpsc::UnboundedSender<ServerMessage>,
    upgrade_tx: mpsc::Sender<UpgradeSignal>,
    rtt_rx: Option<mpsc::UnboundedReceiver<RttSample>>,
    /// Active transport override (issue #69). `Some` disables the heuristic.
    forced: Option<TransportKind>,
    override_rx: Option<mpsc::UnboundedReceiver<Option<TransportKind>>>,
}

#[cfg(feature = "remote")]
impl TransportSupervisor {
    pub fn new(params: SupervisorParams) -> Self {
        let endpoints = params.endpoints.iter().map(EndpointHealth::new).collect();
        Self {
            endpoints,
            connection_id: params.connection_id,
            token: params.token,
            capabilities: params.capabilities,
            accept_invalid_certs: params.accept_invalid_certs,
            active_transport: params.active_transport,
            scorer: TransportScorer::new(params.is_local),
            server_tx: params.server_tx,
            upgrade_tx: params.upgrade_tx,
            rtt_rx: params.rtt_rx,
            forced: params.forced,
            override_rx: params.override_rx,
        }
    }

    /// Apply a single RTT observation for the endpoint matching `sample.kind`.
    /// Only updates the active transport — other kinds only get measurements
    /// when they themselves become active after a swap.
    fn apply_rtt(&mut self, sample: RttSample) {
        if let Some(ep) = self.endpoints.iter_mut().find(|e| e.kind == sample.kind) {
            ep.record_rtt(sample.rtt_ms);
        }
    }

    /// Ensure the forced transport (issue #69) is the active one. No-op when
    /// already active or when the forced transport has no advertised endpoint
    /// (e.g. UDS on a remote host — we log and stay put). On success it swaps
    /// via `upgrade_tx`, exactly like a heuristic upgrade.
    async fn ensure_active(&mut self, kind: TransportKind) {
        if self.active_transport == kind {
            return;
        }
        let Some(idx) = self.endpoints.iter().position(|e| e.kind == kind) else {
            warn!(
                forced = %kind,
                active = %self.active_transport,
                "Supervisor: forced transport has no advertised endpoint; staying put",
            );
            return;
        };
        let address = self.endpoints[idx].address.clone();
        info!(forced = %kind, "Supervisor: forcing transport switch");
        match probe_transport(
            kind,
            &address,
            &self.token,
            self.connection_id,
            &self.capabilities,
            self.accept_invalid_certs,
            self.server_tx.clone(),
        )
        .await
        {
            Ok(sender) => {
                if let Some(old) = self
                    .endpoints
                    .iter_mut()
                    .find(|e| e.kind == self.active_transport)
                {
                    old.mark_swap_away();
                }
                self.active_transport = kind;
                if self
                    .upgrade_tx
                    .send(UpgradeSignal {
                        new_kind: kind,
                        sender,
                    })
                    .await
                    .is_err()
                {
                    debug!("Supervisor: upgrade_tx closed during forced swap");
                }
            }
            Err(e) => {
                self.endpoints[idx].record_failure();
                warn!(
                    transport = %kind,
                    address = %address,
                    error = %e,
                    "Supervisor: forced transport probe failed; staying on {}",
                    self.active_transport,
                );
            }
        }
    }

    /// Run the supervisor loop (blocking until `upgrade_tx` is dropped).
    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(PROBE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Own the receivers locally so the wait-loop's `select!` borrows them
        // without conflicting with the `&mut self` method calls below.
        let mut override_rx = self.override_rx.take();
        let mut rtt_rx = self.rtt_rx.take();

        // A transport override seeded at spawn (persisted across reconnects)
        // takes effect immediately, before the first probe tick.
        if let Some(kind) = self.forced {
            self.ensure_active(kind).await;
        }

        loop {
            // Wait for the next wake: a probe tick, or a live override change.
            // RTT samples are drained without ending the wait.
            loop {
                tokio::select! {
                    _ = interval.tick() => break,
                    cmd = recv_opt(&mut override_rx) => match cmd {
                        Some(target) => {
                            if target != self.forced {
                                info!(?target, "Supervisor: transport override changed");
                            }
                            self.forced = target;
                            break;
                        }
                        // Sender dropped: stop accepting override updates.
                        None => override_rx = None,
                    },
                    sample = recv_opt(&mut rtt_rx) => match sample {
                        Some(s) => self.apply_rtt(s),
                        // Sender dropped: fall back to pure tick cadence.
                        None => rtt_rx = None,
                    },
                }
            }

            if self.upgrade_tx.is_closed() {
                debug!("Supervisor: upgrade_tx closed, stopping");
                return;
            }

            // Override engaged → never run the heuristic; just keep the forced
            // transport active (probe + swap to it if we have drifted away).
            if let Some(kind) = self.forced {
                self.ensure_active(kind).await;
                continue;
            }

            let ranked = self.scorer.rank(&self.endpoints, self.active_transport);

            // Find the best non-active endpoint to probe.
            let probe_idx = ranked
                .iter()
                .find(|(_, i)| self.endpoints[*i].kind != self.active_transport)
                .map(|(_, i)| *i);

            let Some(target_idx) = probe_idx else {
                debug!("Supervisor: no candidate to probe");
                continue;
            };

            // Avoid endlessly probing a candidate the scorer has already
            // demoted below the active transport. We still allow the *first*
            // probe of an unmeasured candidate (no RTT and no failures yet)
            // so we have a chance to measure it — otherwise the candidate's
            // unknown-latency penalty would lock it out forever even when
            // it's actually faster.
            let candidate_score = self.scorer.score(&self.endpoints[target_idx]);
            let active_score = self
                .endpoints
                .iter()
                .find(|e| e.kind == self.active_transport)
                .map(|e| self.scorer.score(e));
            let candidate_measured = self.endpoints[target_idx].rtt_ewma_ms.is_some()
                || self.endpoints[target_idx].failure_count > 0;
            if let Some(active_score) = active_score
                && candidate_measured
                && candidate_score <= active_score
            {
                debug!(
                    transport = %self.endpoints[target_idx].kind,
                    candidate_score,
                    active = %self.active_transport,
                    active_score,
                    "Supervisor: skipping probe (candidate scores no better than active)",
                );
                continue;
            }

            let kind = self.endpoints[target_idx].kind;
            let address = self.endpoints[target_idx].address.clone();

            debug!(transport = %kind, address = %address, "Supervisor: probing candidate");

            let result = probe_transport(
                kind,
                &address,
                &self.token,
                self.connection_id,
                &self.capabilities,
                self.accept_invalid_certs,
                self.server_tx.clone(),
            )
            .await;

            match result {
                Ok(sender) => {
                    // Mark the previously-active transport for hysteresis.
                    if let Some(old) = self
                        .endpoints
                        .iter_mut()
                        .find(|e| e.kind == self.active_transport)
                    {
                        old.mark_swap_away();
                    }
                    self.active_transport = kind;

                    info!(
                        new_transport = %kind,
                        "Supervisor: probe succeeded; signalling channel switch"
                    );

                    if self
                        .upgrade_tx
                        .send(UpgradeSignal {
                            new_kind: kind,
                            sender,
                        })
                        .await
                        .is_err()
                    {
                        debug!("Supervisor: upgrade_tx closed after probe, stopping");
                        return;
                    }

                    // Pause before probing again to avoid immediate oscillation.
                    tokio::time::sleep(PROBE_INTERVAL * 2).await;
                }
                Err(e) => {
                    let prior_failures = self.endpoints[target_idx].failure_count;
                    self.endpoints[target_idx].record_failure();
                    // Surface the *first* failure of a fresh streak at warn so
                    // users can see why an upgrade isn't happening, without
                    // spamming the log every 30s while the condition persists.
                    // After 5 retries we re-warn so a user who tails the log
                    // mid-session still sees it.
                    let new_count = self.endpoints[target_idx].failure_count;
                    if prior_failures == 0 || new_count.is_multiple_of(5) {
                        warn!(
                            transport = %kind,
                            address = %address,
                            error = %e,
                            attempt = new_count,
                            "Supervisor: transport upgrade probe failed; staying on {}",
                            self.active_transport,
                        );
                    } else {
                        debug!(
                            transport = %kind,
                            error = %e,
                            attempt = new_count,
                            "Supervisor: probe failed (suppressed; will warn again at attempt {})",
                            new_count.next_multiple_of(5),
                        );
                    }
                }
            }
        }
    }
}

/// Receive from an optional channel, or pend forever when it is `None` (so a
/// `tokio::select!` arm over a dropped/absent receiver simply never fires).
#[cfg(feature = "remote")]
async fn recv_opt<T>(rx: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

// ─── probe_transport ─────────────────────────────────────────────────────────

/// Spawn a forwarding task that captures the first `AuthResult` arriving on
/// the new transport's `server_tx`, then forwards every server message
/// (including the captured `AuthResult`) to `outer_tx`.
///
/// Returns the wrapped sender to hand to the connect call and a oneshot
/// receiver that resolves with the auth outcome (`Ok` on success,
/// `Err(reason)` on auth failure). The forwarder exits when the underlying
/// transport's reader task drops its sender.
#[cfg(feature = "remote")]
fn spawn_auth_forwarder(
    mut intercept_rx: mpsc::UnboundedReceiver<ServerMessage>,
    auth_tx: oneshot::Sender<Result<(), String>>,
    outer_tx: mpsc::UnboundedSender<ServerMessage>,
    client_tx: mpsc::UnboundedSender<ClientMessage>,
) {
    tokio::spawn(async move {
        let mut auth_tx = Some(auth_tx);
        while let Some(msg) = intercept_rx.recv().await {
            // Answer the identity challenge in-band (issue #146); never forwarded.
            if let ServerMessage::AuthChallenge { nonce } = &msg {
                crate::tcp_connect::answer_auth_challenge(&client_tx, nonce);
                continue;
            }
            if let (Some(_), ServerMessage::AuthResult { .. }) = (&auth_tx, &msg) {
                let captured = match &msg {
                    ServerMessage::AuthResult { success: true, .. } => Ok(()),
                    ServerMessage::AuthResult {
                        success: false,
                        reason,
                        ..
                    } => Err(reason.clone().unwrap_or_else(|| "rejected".into())),
                    _ => unreachable!("matched AuthResult above"),
                };
                if let Some(tx) = auth_tx.take() {
                    let _ = tx.send(captured);
                }
            }
            if outer_tx.send(msg).is_err() {
                break;
            }
        }
    });
}

/// Attempt a connection on `kind` using the given address and return the
/// sender. The probe is only considered successful after `AuthResult { success: true }`
/// is observed on the new transport — opening the stream alone is not enough.
///
/// `address` is `"host:port"` for QUIC/TCP+TLS or an absolute path for UDS.
#[cfg(feature = "remote")]
async fn probe_transport(
    kind: TransportKind,
    address: &str,
    token: &str,
    connection_id: ConnectionId,
    capabilities: &ClientCapabilities,
    accept_invalid_certs: bool,
    server_tx: mpsc::UnboundedSender<ServerMessage>,
) -> Result<mpsc::UnboundedSender<ClientMessage>, String> {
    use crate::connect::ConnectResult;

    let (intercept_tx, intercept_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let (auth_tx, auth_rx) = oneshot::channel::<Result<(), String>>();

    let token = token.to_string();
    let address = address.to_string();
    let capabilities = capabilities.clone();
    let connect_fut = async move {
        match kind {
            TransportKind::Quic => {
                let (host, port) = parse_host_port(&address)
                    .ok_or_else(|| format!("cannot parse QUIC address: {address}"))?;
                Ok(crate::connect::connect(
                    host,
                    port,
                    token,
                    accept_invalid_certs,
                    intercept_tx,
                    capabilities,
                    Some(connection_id),
                )
                .await)
            }
            TransportKind::TcpTls => {
                let (host, port) = parse_host_port(&address)
                    .ok_or_else(|| format!("cannot parse TCP+TLS address: {address}"))?;
                let tofu_key = format!("{host}:{port}");
                Ok(crate::tcp_connect::connect_tcp_tls(
                    host,
                    port,
                    tofu_key,
                    token,
                    intercept_tx,
                    capabilities,
                    Some(connection_id),
                    accept_invalid_certs,
                )
                .await)
            }
            TransportKind::Uds => {
                // For UDS the address is the socket path.
                Ok(crate::tcp_connect::connect_uds(
                    std::path::Path::new(&address),
                    token,
                    intercept_tx,
                    capabilities,
                    Some(connection_id),
                )
                .await)
            }
            TransportKind::Tcp => Err("plain TCP is not supported; use TCP+TLS".to_string()),
        }
    };

    // Cap the connect call so a stuck handshake (e.g. UDP filtered between
    // client and server) surfaces as a probe failure instead of stalling
    // the supervisor for quinn's idle-timeout window.
    let connect_result = match timeout(PROBE_CONNECT_TIMEOUT, connect_fut).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(format!(
                "connect did not complete within {}s",
                PROBE_CONNECT_TIMEOUT.as_secs()
            ));
        }
    };

    let sender = match connect_result {
        ConnectResult::Connected(s) => s,
        ConnectResult::Failed(e) => return Err(e),
    };

    // Forward the server stream and answer the identity challenge (issue #146):
    // spawned here because it needs `sender` (client_tx) to reply. Any frames the
    // daemon already sent buffer in `intercept_rx` until this drains them.
    spawn_auth_forwarder(intercept_rx, auth_tx, server_tx, sender.clone());

    // Wait for the server to confirm authentication on the new channel.
    // Dropping `sender` on Err closes the new transport's writer task, which
    // in turn lets the reader task exit cleanly so no resources leak.
    match timeout(PROBE_AUTH_TIMEOUT, auth_rx).await {
        Ok(Ok(Ok(()))) => Ok(sender),
        Ok(Ok(Err(reason))) => {
            drop(sender);
            Err(format!("auth rejected: {reason}"))
        }
        Ok(Err(_)) => {
            drop(sender);
            Err("auth forwarder dropped before AuthResult".into())
        }
        Err(_) => {
            drop(sender);
            Err(format!(
                "no AuthResult within {}s",
                PROBE_AUTH_TIMEOUT.as_secs()
            ))
        }
    }
}

/// Parse `"host:port"` from an address string.
#[cfg(feature = "remote")]
fn parse_host_port(address: &str) -> Option<(String, u16)> {
    let (host, port_str) = address.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    Some((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::TransportKind;
    use kmux_sys::transport::EndpointAdvert;

    fn make_health(kind: TransportKind) -> EndpointHealth {
        EndpointHealth {
            kind,
            address: "host:8443".into(),
            rtt_ewma_ms: None,
            failure_count: 0,
            last_failure: None,
            last_swap_away: None,
            server_priority: 0,
        }
    }

    // ── Scorer: base weights ──────────────────────────────────────────────────

    #[test]
    fn uds_wins_locally() {
        let scorer = TransportScorer::new(true);
        let uds = make_health(TransportKind::Uds);
        let quic = make_health(TransportKind::Quic);
        assert!(scorer.score(&uds) > scorer.score(&quic));
    }

    #[test]
    fn quic_beats_tcp_tls_remote() {
        let scorer = TransportScorer::new(false);
        let quic = make_health(TransportKind::Quic);
        let tcp = make_health(TransportKind::TcpTls);
        assert!(scorer.score(&quic) > scorer.score(&tcp));
    }

    #[test]
    fn uds_no_locality_bonus_when_remote() {
        let scorer = TransportScorer::new(false);
        let uds = make_health(TransportKind::Uds);
        let quic = make_health(TransportKind::Quic);
        // Without locality bonus UDS has weight 30, QUIC has 20, but both have
        // unknown latency penalty so UDS still wins on robustness alone.
        assert!(scorer.score(&uds) > scorer.score(&quic));
        // But the locality bonus (1000) is not present.
        assert!(scorer.score(&uds) < LOCALITY_BONUS_UDS);
    }

    // ── Scorer: failure penalty ───────────────────────────────────────────────

    #[test]
    fn failure_penalty_applies_within_window() {
        let scorer = TransportScorer::new(false);
        let mut h = make_health(TransportKind::Quic);
        h.record_failure();
        h.record_failure();
        let score_with_failures = scorer.score(&h);

        let clean = make_health(TransportKind::Quic);
        let score_clean = scorer.score(&clean);

        assert!(score_with_failures < score_clean);
        assert_eq!(score_clean - score_with_failures, 2 * FAILURE_PENALTY_PER);
    }

    #[test]
    fn failure_penalty_decays_after_success() {
        let scorer = TransportScorer::new(false);
        let mut h = make_health(TransportKind::Quic);
        h.record_failure();
        h.record_failure();
        h.record_success(); // decays by 1
        assert_eq!(h.failure_count, 1);
        let score_one_fail = scorer.score(&h);

        let mut h2 = make_health(TransportKind::Quic);
        h2.record_failure();
        let score_one_fail_fresh = scorer.score(&h2);

        // Both should have the same failure count, and since both failures are
        // within the window, both should have the same score.
        assert_eq!(score_one_fail, score_one_fail_fresh);
    }

    // ── Scorer: oscillation penalty ───────────────────────────────────────────

    #[test]
    fn oscillation_penalty_applies_after_swap_away() {
        let scorer = TransportScorer::new(false);
        let mut h = make_health(TransportKind::Quic);
        h.mark_swap_away();

        let clean = make_health(TransportKind::Quic);
        assert!(scorer.score(&h) < scorer.score(&clean));
        assert_eq!(scorer.score(&clean) - scorer.score(&h), OSCILLATION_PENALTY);
    }

    // ── Scorer: rank ──────────────────────────────────────────────────────────

    #[test]
    fn rank_returns_best_first() {
        let scorer = TransportScorer::new(true);
        let endpoints = vec![
            make_health(TransportKind::TcpTls),
            make_health(TransportKind::Quic),
            make_health(TransportKind::Uds),
        ];
        let ranked = scorer.rank(&endpoints, TransportKind::TcpTls);
        // UDS should be first for local target.
        assert_eq!(endpoints[ranked[0].1].kind, TransportKind::Uds);
    }

    // ── EndpointHealth RTT ────────────────────────────────────────────────────

    #[test]
    fn rtt_ewma_converges() {
        let mut h = make_health(TransportKind::Quic);
        h.record_rtt(100.0);
        h.record_rtt(100.0);
        // EWMA should stay close to 100.
        let rtt = h.rtt_ewma_ms.unwrap();
        assert!((rtt - 100.0).abs() < 1.0);
    }

    // ── parse_host_port ───────────────────────────────────────────────────────

    #[test]
    fn parse_host_port_ok() {
        let result = parse_host_port("host.example:8443");
        assert_eq!(result, Some(("host.example".into(), 8443)));
    }

    #[test]
    fn parse_host_port_no_port_returns_none() {
        assert!(parse_host_port("host.example").is_none());
    }

    // ── SupervisorParams / constructor ────────────────────────────────────────

    #[test]
    fn supervisor_new_populates_endpoints() {
        let (srv_tx, _srv_rx) = mpsc::unbounded_channel();
        let (up_tx, _up_rx) = mpsc::channel(1);
        let adverts = vec![
            EndpointAdvert {
                kind: TransportKind::Quic,
                address: "host:8443".into(),
            },
            EndpointAdvert {
                kind: TransportKind::TcpTls,
                address: "host:8444".into(),
            },
        ];
        let sup = TransportSupervisor::new(SupervisorParams {
            endpoints: adverts,
            connection_id: ConnectionId(1),
            token: "tok".into(),
            capabilities: ClientCapabilities::default(),
            accept_invalid_certs: false,
            active_transport: TransportKind::Quic,
            is_local: false,
            server_tx: srv_tx,
            upgrade_tx: up_tx,
            rtt_rx: None,
            forced: None,
            override_rx: None,
        });
        assert_eq!(sup.endpoints.len(), 2);
        assert_eq!(sup.active_transport, TransportKind::Quic);
    }

    // ── apply_rtt ─────────────────────────────────────────────────────────────

    #[test]
    fn apply_rtt_updates_matching_endpoint_only() {
        let (srv_tx, _srv_rx) = mpsc::unbounded_channel();
        let (up_tx, _up_rx) = mpsc::channel(1);
        let adverts = vec![
            EndpointAdvert {
                kind: TransportKind::Quic,
                address: "host:8443".into(),
            },
            EndpointAdvert {
                kind: TransportKind::TcpTls,
                address: "host:8444".into(),
            },
        ];
        let mut sup = TransportSupervisor::new(SupervisorParams {
            endpoints: adverts,
            connection_id: ConnectionId(1),
            token: "tok".into(),
            capabilities: ClientCapabilities::default(),
            accept_invalid_certs: false,
            active_transport: TransportKind::Quic,
            is_local: false,
            server_tx: srv_tx,
            upgrade_tx: up_tx,
            rtt_rx: None,
            forced: None,
            override_rx: None,
        });
        sup.apply_rtt(RttSample {
            kind: TransportKind::Quic,
            rtt_ms: 42.0,
        });
        let quic = sup
            .endpoints
            .iter()
            .find(|e| e.kind == TransportKind::Quic)
            .unwrap();
        let tcp = sup
            .endpoints
            .iter()
            .find(|e| e.kind == TransportKind::TcpTls)
            .unwrap();
        assert_eq!(quic.rtt_ewma_ms, Some(42.0));
        assert!(tcp.rtt_ewma_ms.is_none());
    }

    // ── transport override (issue #69) ────────────────────────────────────────

    fn forced_supervisor(
        active: TransportKind,
        forced: TransportKind,
        endpoints: Vec<EndpointAdvert>,
    ) -> (TransportSupervisor, mpsc::Receiver<UpgradeSignal>) {
        let (srv_tx, _srv_rx) = mpsc::unbounded_channel();
        let (up_tx, up_rx) = mpsc::channel(1);
        let sup = TransportSupervisor::new(SupervisorParams {
            endpoints,
            connection_id: ConnectionId(1),
            token: "tok".into(),
            capabilities: ClientCapabilities::default(),
            accept_invalid_certs: false,
            active_transport: active,
            is_local: false,
            server_tx: srv_tx,
            upgrade_tx: up_tx,
            rtt_rx: None,
            forced: Some(forced),
            override_rx: None,
        });
        (sup, up_rx)
    }

    #[tokio::test]
    async fn ensure_active_is_noop_when_already_on_forced_transport() {
        let (mut sup, mut up_rx) = forced_supervisor(
            TransportKind::Quic,
            TransportKind::Quic,
            vec![EndpointAdvert {
                kind: TransportKind::Quic,
                address: "host:8443".into(),
            }],
        );
        sup.ensure_active(TransportKind::Quic).await;
        assert!(
            up_rx.try_recv().is_err(),
            "no swap when already on the forced transport"
        );
        assert_eq!(sup.active_transport, TransportKind::Quic);
    }

    #[tokio::test]
    async fn ensure_active_stays_put_when_forced_transport_unavailable() {
        // UDS is not advertised (e.g. a remote host): we must not probe a
        // non-existent endpoint and must keep the working transport.
        let (mut sup, mut up_rx) = forced_supervisor(
            TransportKind::Quic,
            TransportKind::Uds,
            vec![EndpointAdvert {
                kind: TransportKind::Quic,
                address: "host:8443".into(),
            }],
        );
        sup.ensure_active(TransportKind::Uds).await;
        assert!(up_rx.try_recv().is_err());
        assert_eq!(
            sup.active_transport,
            TransportKind::Quic,
            "stays on the available transport"
        );
    }

    // ── spawn_auth_intercept ───────────────────────────────────────────────────

    /// Build a successful `AuthResult` for the forwarder tests (issue #146 added
    /// the identity fields, defaulted to `None` here).
    fn ok_auth_result() -> ServerMessage {
        ServerMessage::AuthResult {
            success: true,
            reason: None,
            client_id: None,
            server_version: None,
            connection_id: None,
            compression: None,
            machine_id: None,
            label: None,
            server_machine_id: None,
            negotiated_protocol: Some(kmux_protocol::messages::PROTOCOL_VERSION),
            negotiated_capabilities: kmux_protocol::messages::protocol_capabilities(),
        }
    }

    /// Spawn a forwarder wired to fresh channels; returns the intercept sink, the
    /// auth receiver, and the outer receiver. The client sink is dropped (these
    /// tests do not exercise the challenge path).
    #[cfg(feature = "remote")]
    fn forwarder_under_test() -> (
        mpsc::UnboundedSender<ServerMessage>,
        oneshot::Receiver<Result<(), String>>,
        mpsc::UnboundedReceiver<ServerMessage>,
    ) {
        let (outer_tx, outer_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let (intercept_tx, intercept_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let (auth_tx, auth_rx) = oneshot::channel::<Result<(), String>>();
        let (client_tx, _client_rx) = mpsc::unbounded_channel::<ClientMessage>();
        spawn_auth_forwarder(intercept_rx, auth_tx, outer_tx, client_tx);
        (intercept_tx, auth_rx, outer_rx)
    }

    #[tokio::test]
    async fn auth_forwarder_signals_success_and_forwards_msg() {
        let (intercept_tx, auth_rx, mut outer_rx) = forwarder_under_test();

        intercept_tx.send(ok_auth_result()).unwrap();

        // Auth oneshot resolves Ok.
        assert!(matches!(auth_rx.await, Ok(Ok(()))));
        // The AuthResult is also forwarded to the outer channel so the
        // SessionManager continues to see every server message.
        assert!(matches!(
            outer_rx.recv().await,
            Some(ServerMessage::AuthResult { success: true, .. })
        ));
    }

    #[tokio::test]
    async fn auth_forwarder_signals_failure_with_reason() {
        let (intercept_tx, auth_rx, _outer_rx) = forwarder_under_test();

        intercept_tx
            .send(ServerMessage::AuthResult {
                success: false,
                reason: Some("bad token".into()),
                client_id: None,
                server_version: None,
                connection_id: None,
                compression: None,
                machine_id: None,
                label: None,
                server_machine_id: None,
                negotiated_protocol: None,
                negotiated_capabilities: Vec::new(),
            })
            .unwrap();

        match auth_rx.await {
            Ok(Err(reason)) => assert_eq!(reason, "bad token"),
            other => panic!("expected Err(bad token), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auth_forwarder_resolves_only_first_auth_result() {
        // The supervisor's probe contract: only the *first* AuthResult counts.
        // Any subsequent server message (including a stray second AuthResult)
        // must keep being forwarded but must not re-fire the auth oneshot.
        let (intercept_tx, auth_rx, mut outer_rx) = forwarder_under_test();

        intercept_tx.send(ok_auth_result()).unwrap();
        assert!(matches!(auth_rx.await, Ok(Ok(()))));

        // A subsequent non-AuthResult message must still be forwarded.
        intercept_tx.send(ServerMessage::Ping { seq: 1 }).unwrap();
        // Drain the first AuthResult that was forwarded.
        outer_rx.recv().await.unwrap();
        assert!(matches!(
            outer_rx.recv().await,
            Some(ServerMessage::Ping { seq: 1 })
        ));
    }
}
