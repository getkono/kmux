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

use kmux_protocol::messages::{
    ClientCapabilities, ClientMessage, ConnectionId, ServerMessage, TransportKind,
};
use kmux_protocol::transport::bootstrap::EndpointAdvert;
use tokio::sync::mpsc;
use tracing::{debug, info};

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
                .map(|t| t.elapsed() < FAILURE_WINDOW)
                .unwrap_or(false);
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
    pub fn rank(&self, endpoints: &[EndpointHealth]) -> Vec<(i32, usize)> {
        let mut scored: Vec<(i32, usize)> = endpoints
            .iter()
            .enumerate()
            .map(|(i, h)| (self.score(h), i))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0)); // descending

        info!(
            target: "kmux::transport::scorer",
            scores = ?scored
                .iter()
                .map(|(s, i)| format!("{}:{s}", endpoints[*i].kind))
                .collect::<Vec<_>>(),
            chosen = %scored
                .first()
                .map(|(_, i)| endpoints[*i].kind.to_string())
                .unwrap_or_default(),
            "scorer decision"
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
}

// ─── TransportSupervisor ─────────────────────────────────────────────────────

/// Background task: probes candidate transports and signals hot-swap when a
/// better one becomes available.
///
/// Spawned after bootstrap by calling `tokio::spawn(supervisor.run())`.
/// Exits when `upgrade_tx` is dropped (caller shut down).
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
}

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

    /// Run the supervisor loop (blocking until upgrade_tx is dropped).
    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(PROBE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Wait for the next probe tick while concurrently draining
            // RTT samples. The RTT stream never cancels the tick — both
            // can make progress in the same iteration.
            loop {
                match self.rtt_rx.as_mut() {
                    Some(rx) => tokio::select! {
                        _ = interval.tick() => break,
                        maybe_sample = rx.recv() => match maybe_sample {
                            Some(s) => self.apply_rtt(s),
                            None => {
                                // Sender dropped: disable RTT ingress and
                                // continue on pure tick cadence.
                                self.rtt_rx = None;
                            }
                        }
                    },
                    None => {
                        interval.tick().await;
                        break;
                    }
                }
            }

            if self.upgrade_tx.is_closed() {
                debug!("Supervisor: upgrade_tx closed, stopping");
                return;
            }

            let ranked = self.scorer.rank(&self.endpoints);

            // Find the best non-active endpoint to probe.
            let probe_idx = ranked
                .iter()
                .find(|(_, i)| self.endpoints[*i].kind != self.active_transport)
                .map(|(_, i)| *i);

            let Some(target_idx) = probe_idx else {
                debug!("Supervisor: no candidate to probe");
                continue;
            };

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
                    debug!(transport = %kind, error = %e, "Supervisor: probe failed");
                    self.endpoints[target_idx].record_failure();
                }
            }
        }
    }
}

// ─── probe_transport ─────────────────────────────────────────────────────────

/// Attempt a connection on `kind` using the given address and return the sender.
///
/// `address` is `"host:port"` for QUIC/TCP+TLS or an absolute path for UDS.
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

    match kind {
        TransportKind::Quic => {
            let (host, port) = parse_host_port(address)
                .ok_or_else(|| format!("cannot parse QUIC address: {address}"))?;
            match crate::connect::connect(
                host,
                port,
                token.to_string(),
                accept_invalid_certs,
                server_tx,
                capabilities.clone(),
                Some(connection_id),
            )
            .await
            {
                ConnectResult::Connected(s) => Ok(s),
                ConnectResult::Failed(e) => Err(e),
            }
        }
        TransportKind::TcpTls => {
            let (host, port) = parse_host_port(address)
                .ok_or_else(|| format!("cannot parse TCP+TLS address: {address}"))?;
            let tofu_key = format!("{host}:{port}");
            match crate::tcp_connect::connect_tcp_tls(
                host,
                port,
                tofu_key,
                token.to_string(),
                server_tx,
                capabilities.clone(),
                Some(connection_id),
                accept_invalid_certs,
            )
            .await
            {
                ConnectResult::Connected(s) => Ok(s),
                ConnectResult::Failed(e) => Err(e),
            }
        }
        TransportKind::Uds => {
            // For UDS the address is the socket path.
            match crate::tcp_connect::connect_uds(
                std::path::Path::new(address),
                token.to_string(),
                server_tx,
                capabilities.clone(),
                Some(connection_id),
            )
            .await
            {
                ConnectResult::Connected(s) => Ok(s),
                ConnectResult::Failed(e) => Err(e),
            }
        }
        TransportKind::Tcp => Err("plain TCP is not supported; use TCP+TLS".into()),
    }
}

/// Parse `"host:port"` from an address string.
fn parse_host_port(address: &str) -> Option<(String, u16)> {
    let (host, port_str) = address.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    Some((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::TransportKind;
    use kmux_protocol::transport::bootstrap::EndpointAdvert;

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
        let ranked = scorer.rank(&endpoints);
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
}
