//! The upstream link to one federated peer (issue #208): opening it, feeding
//! it, keeping it honest, and re-opening it when it drops.
//!
//! A link runs under one supervisor task per peer ([`spawn_link`]). It feeds
//! the upstream `ServerMessage` stream into the hub ([`feed`]), pings the peer
//! and closes the link after [`UPSTREAM_DEADLINE`] of silence. When the link
//! ends — the peer went away, the network dropped, or it went silent — the
//! peer's sessions are marked unreachable (still listed, under the same local
//! words, their viewers kept) instead of closed, and the link is re-opened
//! with [`kmux_client::backoff`]. Once it is back the sessions are reconciled
//! against the peer's list, every proxied pane is re-attached, and every
//! client is sent the session list again. A session is closed only when the
//! peer reports it closed (or no longer lists it), or the user closes the peer.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use kmux_client::backoff::{jitter_seed, next_delay};
use kmux_connect::connect::ConnectResult;
use kmux_connect::ssh::{self, RemoteTarget};
use kmux_connect::tcp_connect::connect_tcp_tls;
use kmux_protocol::messages::{
    AuthFailure, ClientCapabilities, ClientMessage, PeerId, PeerTarget, ServerMessage,
    SessionEntry, refusal_reason,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{info, warn};

use super::{AUTH_TIMEOUT, LIST_TIMEOUT, PeerConnection, TunnelGuard, feed, recv_until};
use crate::app::ServerApp;

/// How often the hub pings its peer, so the peer's silence is noticed even
/// when nothing else flows.
pub(super) const UPSTREAM_PING_INTERVAL: Duration = Duration::from_secs(5);

/// How long the peer may stay silent — not a frame, not a pong — before the
/// hub closes the link and calls the peer unreachable. Three missed pings.
pub(super) const UPSTREAM_DEADLINE: Duration = Duration::from_secs(15);

/// The longest one attempt to re-open a link may take; a black-holed host
/// would otherwise hold the handshake forever.
pub(super) const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);

/// Whether a peer last heard from at `last_inbound` has been silent too long
/// at `now`.
pub(super) fn upstream_silent(last_inbound: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_inbound) > UPSTREAM_DEADLINE
}

/// An open, authenticated upstream link and the peer's session list.
pub(crate) struct Upstream {
    pub(crate) client_tx: mpsc::UnboundedSender<ClientMessage>,
    pub(crate) server_rx: mpsc::UnboundedReceiver<ServerMessage>,
    pub(crate) sessions: Vec<SessionEntry>,
    /// The SSH `-L` tunnel of an SSH peer, killed if the link is dropped
    /// before it is parked on its connection.
    pub(crate) tunnel: TunnelGuard,
}

/// Opens an upstream link: the real connect for a peer target, or a scripted
/// one in tests.
pub(crate) type Connector =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Upstream, String>> + Send>> + Send + Sync>;

/// The connector for `target`, each attempt bounded by
/// [`CONNECT_ATTEMPT_TIMEOUT`]. An SSH peer is re-negotiated each time, so a
/// restarted remote daemon (new token, new port) is found again.
pub(super) fn connector_for(target: PeerTarget) -> Connector {
    Arc::new(move || {
        let target = target.clone();
        Box::pin(async move {
            tokio::time::timeout(CONNECT_ATTEMPT_TIMEOUT, connect_upstream(target))
                .await
                .unwrap_or_else(|_| Err("peer did not answer in time".to_string()))
        })
    })
}

/// A resolved TCP+TLS endpoint for an upstream peer link. Both [`PeerTarget`]
/// variants reduce to this: `Direct` is the endpoint verbatim; `Ssh` is the
/// loopback end of an `-L` tunnel (with the tunnel child retained so it outlives
/// the connection). From here the connect/auth/list path is identical.
struct PeerConnectPlan {
    host: String,
    port: u16,
    /// TOFU identity for cert pinning — for SSH this is the *real* remote
    /// `host:tcp_port`, not the ephemeral loopback the tunnel listens on.
    tofu_key: String,
    token: String,
    accept_invalid: bool,
    ssh_tunnel: Option<tokio::process::Child>,
}

/// Resolve `target` to a TCP+TLS endpoint. A `Direct` peer is that endpoint
/// verbatim; an `Ssh` peer first negotiates a `-L` tunnel (`kmuxd
/// probe-or-start` over SSH, then forward a loopback port).
async fn plan_for(target: PeerTarget) -> Result<PeerConnectPlan, String> {
    match target {
        PeerTarget::Direct {
            host,
            port,
            token,
            accept_invalid_certs,
        } => Ok(PeerConnectPlan {
            tofu_key: format!("{host}:{port}"),
            host,
            port,
            token,
            accept_invalid: accept_invalid_certs,
            ssh_tunnel: None,
        }),
        PeerTarget::Ssh {
            user,
            host,
            ssh_port,
            accept_invalid_certs,
        } => {
            let remote = RemoteTarget {
                user,
                host,
                ssh_port,
            };
            let ssh = ssh::negotiate(&remote)
                .await
                .map_err(|e| format!("SSH peer negotiation failed: {e}"))?;
            Ok(PeerConnectPlan {
                tofu_key: format!("{}:{}", ssh.remote_host, ssh.remote_tcp_port),
                host: "127.0.0.1".to_string(),
                port: ssh.local_tcp_port,
                token: ssh.token,
                accept_invalid: accept_invalid_certs,
                ssh_tunnel: Some(ssh.tunnel_process),
            })
        }
        // Refused before it gets here (`on_open_peer`); kept as an answer
        // rather than a panic.
        PeerTarget::Unknown => Err(crate::app::UNSUPPORTED_PEER_TARGET.to_string()),
    }
}

/// Open and authenticate an upstream link to `target` and fetch its session
/// list.
pub(super) async fn connect_upstream(target: PeerTarget) -> Result<Upstream, String> {
    let plan = plan_for(target).await?;
    // Hold the SSH tunnel in a kill-on-drop guard so any error below tears
    // down the `ssh -L` process — `tokio::process::Child` is not kill-on-drop.
    let tunnel = TunnelGuard(plan.ssh_tunnel);

    // 1. Open the upstream link. `connect_tcp_tls` sends `Auth` itself and
    //    forwards every `ServerMessage` (incl. `AuthResult`) to `server_tx`.
    let (server_tx, mut server_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let client_tx = match connect_tcp_tls(
        plan.host,
        plan.port,
        plan.tofu_key,
        plan.token,
        server_tx,
        ClientCapabilities::default(),
        None,
        plan.accept_invalid,
    )
    .await
    {
        ConnectResult::Connected(tx) => tx,
        ConnectResult::Failed(e) => return Err(format!("peer connect failed: {e}")),
    };

    // 2–3. Authenticate, then fetch the session list.
    let sessions = handshake(
        &client_tx,
        &mut server_rx,
        kmux_connect::tcp_connect::answer_auth_challenge,
    )
    .await?;

    Ok(Upstream {
        client_tx,
        server_rx,
        sessions,
        tunnel,
    })
}

/// Why the peer refused the link, for the log and for `PeerError`: the
/// refusal and, where there is one, what to do about it. A peer refusing the
/// token is up but no longer the run the token was issued by — a `kmuxd`
/// started outright issues a new one — and nothing the hub holds can re-open
/// it (see `docs/architecture-federation.md`, "Re-discovering a restarted
/// peer").
pub(super) fn refused(reason: Option<String>, failure: Option<AuthFailure>) -> String {
    let text = refusal_reason(reason, failure);
    let remedy = match failure {
        Some(AuthFailure::BadToken) => Some(
            "the peer issues a new token when it restarts: open it again with the new one \
             (an SSH peer is re-negotiated and never needs this)",
        ),
        Some(other) => other.hint(),
        None => None,
    };
    match remedy {
        Some(remedy) => format!("peer rejected authentication: {text} ({remedy})"),
        None => format!("peer rejected authentication: {text}"),
    }
}

/// The handshake on an opened link: answer the identity challenge with
/// `answer` (our own key, in production), await the authentication result,
/// then fetch the peer's session list. The hub authenticates upstream as a
/// distinct cryptographic entity (issue #146), so the peer's client list shows
/// it as one connection.
pub(super) async fn handshake(
    client_tx: &mpsc::UnboundedSender<ClientMessage>,
    server_rx: &mut mpsc::UnboundedReceiver<ServerMessage>,
    answer: impl Fn(&mpsc::UnboundedSender<ClientMessage>, &[u8]) -> bool,
) -> Result<Vec<SessionEntry>, String> {
    loop {
        match recv_until(server_rx, AUTH_TIMEOUT, |m| {
            matches!(
                m,
                ServerMessage::AuthChallenge { .. } | ServerMessage::AuthResult { .. }
            )
        })
        .await
        {
            Some(ServerMessage::AuthChallenge { nonce }) => {
                if !answer(client_tx, &nonce) {
                    return Err("failed to answer peer identity challenge".to_string());
                }
            }
            Some(ServerMessage::AuthResult { success: true, .. }) => break,
            Some(ServerMessage::AuthResult {
                success: false,
                reason,
                failure,
                ..
            }) => return Err(refused(reason, failure)),
            _ => return Err("peer did not complete authentication in time".to_string()),
        }
    }

    if client_tx
        .send(ClientMessage::SessionList { request_id: 1 })
        .is_err()
    {
        return Err("peer connection closed before session list".to_string());
    }
    let Some(ServerMessage::SessionListResult { sessions, .. }) =
        recv_until(server_rx, LIST_TIMEOUT, |m| {
            matches!(m, ServerMessage::SessionListResult { .. })
        })
        .await
    else {
        return Err("peer did not return a session list in time".to_string());
    };
    Ok(sessions)
}

/// Why a link ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkEnd {
    /// The peer closed it, or the transport under it failed.
    Closed,
    /// The peer stayed silent past [`UPSTREAM_DEADLINE`].
    Silent,
}

/// Run the link to `peer_id` over `server_rx` for as long as the peer is
/// open: feed it, and re-open it with the connection's connector whenever it
/// drops.
pub(super) fn spawn_link(
    app: Weak<ServerApp>,
    conn: Arc<Mutex<PeerConnection>>,
    peer_id: PeerId,
    server_rx: mpsc::UnboundedReceiver<ServerMessage>,
) -> JoinHandle<()> {
    tokio::spawn(run_link(app, conn, peer_id, server_rx))
}

async fn run_link(
    app: Weak<ServerApp>,
    conn: Arc<Mutex<PeerConnection>>,
    peer_id: PeerId,
    mut server_rx: mpsc::UnboundedReceiver<ServerMessage>,
) {
    loop {
        let end = feed::feed(&app, &conn, &peer_id, &mut server_rx).await;
        warn!(%peer_id, ?end, "federation link down; the peer is unreachable");
        // Nothing reads the old link any more: drop its receiver, so frames do
        // not queue up during the reopen and its reader ends on the next one
        // (a black-holed socket is ended by its TCP keepalive).
        drop(server_rx);
        link_down(&conn);
        broadcast_sessions(&app).await;
        let upstream = reopen(&peer_id, &conn).await;
        let Some(app_now) = app.upgrade() else {
            return;
        };
        let Some(relinked) = relink(&app_now, &conn, &peer_id, upstream) else {
            return;
        };
        server_rx = relinked;
        info!(%peer_id, "federation link restored");
        broadcast_sessions(&app).await;
    }
}

/// The link is down: stop sending on it (requests fail at once rather than
/// wait for a reply that will not come) and mark the peer unreachable. Its
/// sessions, words and proxied panes — and their viewers — stay.
pub(super) fn link_down(conn: &Mutex<PeerConnection>) {
    let mut guard = conn
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.dead = true;
    guard.client_tx = mpsc::unbounded_channel().0;
    guard.pending_creates.clear();
    guard.pending_overviews.clear();
    guard.pending_client_lists.clear();
    guard.pending_acks.clear();
}

/// Retry `conn`'s connector with backoff until it opens a link, reading it
/// afresh each attempt (a reopen may have re-targeted the peer). Closing the
/// peer (by the user, or the daemon shutting down) aborts the link task, and
/// with it this loop.
async fn reopen(peer_id: &str, conn: &Mutex<PeerConnection>) -> Upstream {
    let seed = jitter_seed();
    let mut attempt = 0;
    loop {
        tokio::time::sleep(next_delay(attempt, seed)).await;
        let connector = Arc::clone(
            &conn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .connector,
        );
        match connector().await {
            Ok(upstream) => return upstream,
            Err(e) => warn!(%peer_id, attempt, "re-opening the federation link failed: {e}"),
        }
        attempt = attempt.saturating_add(1);
    }
}

/// Put `upstream` in the dead link's place: reconcile the peer's sessions,
/// send on the new link, park its tunnel, and re-attach every proxied pane
/// for a snapshot — whatever run the peer is. A snapshot is right for a new
/// run, whose seqnos start over, and for the same one: a seqno replay would
/// carry only the diffs, not the scrollback appended while the link was down,
/// and the hub's mirror has no way to fetch that back. Returns what to feed,
/// or `None` when the user closed the peer during the reopen: the new link
/// is then dropped (its tunnel killed) and nothing is registered for it.
///
/// It runs under the membership gate, which `close_peer` also holds, so the
/// peer cannot be closed between the check and the tunnel being parked.
pub(super) fn relink(
    app: &ServerApp,
    conn: &Arc<Mutex<PeerConnection>>,
    peer_id: &str,
    upstream: Upstream,
) -> Option<mpsc::UnboundedReceiver<ServerMessage>> {
    let Upstream {
        client_tx,
        server_rx,
        sessions,
        mut tunnel,
    } = upstream;
    let _membership = app.peer_manager.membership();
    if !app.peer_manager.is_open(peer_id, conn) {
        return None;
    }
    {
        let mut guard = conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.client_tx = client_tx;
        guard.dead = false;
        if let Some(mut old) = guard.ssh_tunnel.take() {
            let _ = old.start_kill();
        }
        guard.ssh_tunnel = tunnel.disarm();
    }
    app.peer_manager
        .reconcile_sessions_gated(app, conn, peer_id, sessions);
    conn.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .reattach_panes();
    Some(server_rx)
}

/// Send every client the session list again: a peer went unreachable or
/// came back, or its sessions changed.
pub(super) async fn broadcast_sessions(app: &Weak<ServerApp>) {
    if let Some(app) = app.upgrade() {
        app.broadcast_session_list().await;
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A peer played over channels, for the federation tests (issue #208).

    use std::time::Duration;

    use kmux_protocol::messages::{ClientMessage, ServerMessage, SessionEventMsg};
    use tokio::sync::{broadcast, mpsc};

    use super::{Connector, Upstream};
    use crate::federation::{TunnelGuard, sample_remote_entry};

    /// The test's ends of a link the hub opened: what the hub sends the peer,
    /// and the sender that plays the peer.
    pub(crate) type PeerEnds = (
        mpsc::UnboundedReceiver<ClientMessage>,
        mpsc::UnboundedSender<ServerMessage>,
    );

    /// A connector whose every attempt opens a link over channels to a peer
    /// listing `remote_word`, handing the test the peer's ends of it.
    pub(crate) fn channel_connector(
        remote_word: &'static str,
    ) -> (Connector, mpsc::UnboundedReceiver<PeerEnds>) {
        let (ends_tx, ends_rx) = mpsc::unbounded_channel();
        let connector: Connector = std::sync::Arc::new(move || {
            let (client_tx, client_rx) = mpsc::unbounded_channel();
            let (server_tx, server_rx) = mpsc::unbounded_channel();
            let _ = ends_tx.send((client_rx, server_tx));
            Box::pin(async move {
                Ok(Upstream {
                    client_tx,
                    server_rx,
                    sessions: vec![sample_remote_entry(remote_word)],
                    tunnel: TunnelGuard(None),
                })
            })
        });
        (connector, ends_rx)
    }

    /// A bound on every wait: on the paused clock a wait that nothing ends
    /// passes it at once rather than hanging the test.
    pub(crate) const WAIT: Duration = Duration::from_secs(120);

    /// The next broadcast `want` picks out; every broadcast before it is
    /// checked not to close a session.
    pub(crate) async fn next_broadcast<T>(
        rx: &mut broadcast::Receiver<ServerMessage>,
        want: impl Fn(&ServerMessage) -> Option<T>,
    ) -> T {
        tokio::time::timeout(WAIT, async {
            loop {
                let msg = rx.recv().await.expect("the broadcast is open");
                assert!(
                    !matches!(
                        msg,
                        ServerMessage::Event {
                            event: SessionEventMsg::SessionClosed { .. }
                        }
                    ),
                    "no session is closed: {msg:?}"
                );
                if let Some(found) = want(&msg) {
                    return found;
                }
            }
        })
        .await
        .expect("the broadcast within the bound")
    }

    /// The next message the hub sends the peer that `want` picks out.
    pub(crate) async fn next_upstream<T>(
        rx: &mut mpsc::UnboundedReceiver<ClientMessage>,
        want: impl Fn(&ClientMessage) -> Option<T>,
    ) -> T {
        tokio::time::timeout(WAIT, async {
            loop {
                let msg = rx.recv().await.expect("the link is open");
                if let Some(found) = want(&msg) {
                    return found;
                }
            }
        })
        .await
        .expect("the message within the bound")
    }

    /// `(word, peer_unreachable)` of a broadcast session list.
    pub(crate) fn listed(msg: &ServerMessage) -> Option<Vec<(String, bool)>> {
        match msg {
            ServerMessage::SessionListResult { sessions, .. } => Some(
                sessions
                    .iter()
                    .map(|e| (e.meta.word_id.clone(), e.peer_unreachable))
                    .collect(),
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kmux_protocol::messages::{ClientId, TermSize};

    use super::testing::*;
    use super::*;
    use crate::fixtures::{fixture_app, make_outbound};

    const SIZE: TermSize = TermSize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };

    /// A link that drops leaves the peer unreachable — its session still
    /// listed, flagged, and closed for no one — and is re-opened on its own:
    /// the session comes back under the same local word, and the proxied pane
    /// a viewer holds is re-attached for a snapshot (issue #208).
    #[tokio::test(start_paused = true)]
    async fn a_dropped_link_leaves_the_peer_unreachable_then_relinks_under_the_same_word() {
        let app = Arc::new(fixture_app());
        let mut broadcasts = app.subscribe_vt_events();
        let (connector, mut opened) = channel_connector("fedremote");
        let (mut upstream, peer) = app.peer_manager.install_channel_peer(
            &app,
            "peer:1",
            "fedlocal",
            "fedremote",
            connector,
        );
        let (data_tx, _data_rx) = mpsc::channel(8);
        assert!(app.federated_attach("fedlocal/0", ClientId(1), data_tx, make_outbound().0, SIZE));
        next_upstream(&mut upstream, |m| {
            matches!(m, ClientMessage::Attach { .. }).then_some(())
        })
        .await;

        drop(peer);
        let down = next_broadcast(&mut broadcasts, listed).await;
        assert_eq!(down, vec![("fedlocal".to_string(), true)]);
        assert!(
            app.is_federated_pane("fedlocal/0"),
            "still routed as federated"
        );

        let (mut upstream, _peer) = tokio::time::timeout(WAIT, opened.recv())
            .await
            .expect("re-opened within the bound")
            .expect("a new link");
        let up = next_broadcast(&mut broadcasts, listed).await;
        assert_eq!(up, vec![("fedlocal".to_string(), false)]);
        let reattached = next_upstream(&mut upstream, |m| match m {
            ClientMessage::Attach {
                pane_id,
                last_seqno,
                ..
            } => Some((pane_id.clone(), *last_seqno)),
            _ => None,
        })
        .await;
        assert_eq!(reattached, ("fedremote/0".to_string(), None));
    }

    /// A peer that answers nothing is pinged, then — past the deadline —
    /// called unreachable, though its link never closed.
    #[tokio::test(start_paused = true)]
    async fn a_silent_peer_is_pinged_then_declared_unreachable() {
        let app = Arc::new(fixture_app());
        let mut broadcasts = app.subscribe_vt_events();
        let started = Instant::now();
        let (mut upstream, _peer) = app.install_channel_peer("fedlocal", "fedremote");

        next_upstream(&mut upstream, |m| {
            matches!(m, ClientMessage::Ping { seq: 0 }).then_some(())
        })
        .await;
        assert!(started.elapsed() >= UPSTREAM_PING_INTERVAL);

        let down = next_broadcast(&mut broadcasts, listed).await;
        assert_eq!(down, vec![("fedlocal".to_string(), true)]);
        assert!(started.elapsed() > UPSTREAM_DEADLINE);
    }

    /// A peer that keeps talking is never called unreachable: every frame
    /// resets the deadline. Each ping carries the next sequence number.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_answers_stays_reachable() {
        let app = Arc::new(fixture_app());
        let (mut upstream, peer) = app.install_channel_peer("fedlocal", "fedremote");
        let mut seqs = Vec::new();
        for _ in 0..5 {
            let seq = next_upstream(&mut upstream, |m| match m {
                ClientMessage::Ping { seq } => Some(*seq),
                _ => None,
            })
            .await;
            peer.send(ServerMessage::Pong { seq }).unwrap();
            seqs.push(seq);
        }
        assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
        let listed = app.list_federated_sessions();
        assert!(!listed[0].peer_unreachable);
    }

    fn auth_result(success: bool, failure: Option<AuthFailure>) -> ServerMessage {
        ServerMessage::AuthResult {
            success,
            reason: failure.map(|f| f.to_string()),
            failure,
            client_id: None,
            server_version: None,
            connection_id: None,
            daemon_instance: None,
            compression: None,
            machine_id: None,
            label: None,
            server_machine_id: None,
            negotiated_protocol: None,
            negotiated_capabilities: kmux_protocol::messages::protocol_capabilities(),
        }
    }

    /// Run [`handshake`] against the scripted peer `frames`, answering each
    /// challenge with `answers` (and recording its nonce). Returns the
    /// outcome, the nonces answered and what the hub sent the peer.
    async fn run_handshake(
        frames: Vec<ServerMessage>,
        answers: bool,
    ) -> (
        Result<Vec<String>, String>,
        Vec<Vec<u8>>,
        Vec<ClientMessage>,
    ) {
        let (client_tx, mut client_rx) = mpsc::unbounded_channel();
        let (server_tx, mut server_rx) = mpsc::unbounded_channel();
        for frame in frames {
            server_tx.send(frame).unwrap();
        }
        let nonces = Mutex::new(Vec::new());
        let result = handshake(&client_tx, &mut server_rx, |_, nonce| {
            nonces.lock().unwrap().push(nonce.to_vec());
            answers
        })
        .await
        .map(|sessions| sessions.into_iter().map(|e| e.meta.word_id).collect());
        drop(server_tx);
        let mut sent = Vec::new();
        while let Ok(msg) = client_rx.try_recv() {
            sent.push(msg);
        }
        (result, nonces.into_inner().unwrap(), sent)
    }

    /// A handshake answers the challenge, takes the result, asks for the
    /// peer's list and returns it.
    #[tokio::test(start_paused = true)]
    async fn a_handshake_answers_the_challenge_then_fetches_the_list() {
        let (result, nonces, sent) = run_handshake(
            vec![
                ServerMessage::AuthChallenge { nonce: vec![7; 4] },
                auth_result(true, None),
                ServerMessage::SessionListResult {
                    request_id: 1,
                    sessions: vec![crate::federation::sample_remote_entry("fedremote")],
                },
            ],
            true,
        )
        .await;
        assert_eq!(result, Ok(vec!["fedremote".to_string()]));
        assert_eq!(nonces, vec![vec![7; 4]]);
        assert!(matches!(
            sent.as_slice(),
            [ClientMessage::SessionList { request_id: 1 }]
        ));
    }

    /// A handshake fails when the challenge cannot be answered, when the
    /// peer refuses (with its reason), or when the peer never answers.
    #[tokio::test(start_paused = true)]
    async fn a_handshake_fails_on_an_unanswered_challenge_a_refusal_or_silence() {
        let challenge = || ServerMessage::AuthChallenge { nonce: vec![1] };
        let (result, _, _) = run_handshake(vec![challenge(), auth_result(true, None)], false).await;
        assert_eq!(
            result,
            Err("failed to answer peer identity challenge".to_string())
        );
        let (result, _, sent) =
            run_handshake(vec![auth_result(false, Some(AuthFailure::BadToken))], true).await;
        assert_eq!(result, Err(refused(None, Some(AuthFailure::BadToken))));
        assert!(sent.is_empty(), "no list asked for");
        let (result, _, _) = run_handshake(vec![auth_result(true, None)], true).await;
        assert_eq!(
            result,
            Err("peer did not return a session list in time".to_string())
        );
        let (result, _, _) = run_handshake(Vec::new(), true).await;
        assert_eq!(
            result,
            Err("peer did not complete authentication in time".to_string())
        );
    }

    /// A refused link says why, and what to do about it where anything can
    /// be done: a rotated token needs the peer opened again with the new one.
    #[test]
    fn a_refusal_names_its_remedy() {
        use kmux_protocol::messages::{PROTOCOL_RANGE, ProtocolRange, ProtocolVersion};

        let token = refused(None, Some(AuthFailure::BadToken));
        assert!(
            token.starts_with("peer rejected authentication: invalid token ("),
            "{token}"
        );
        assert!(token.contains("new token"), "{token}");
        let mismatch = refused(
            None,
            Some(AuthFailure::ProtocolMismatch {
                client: PROTOCOL_RANGE,
                daemon: ProtocolRange::exact(ProtocolVersion::new(2, 0, 0)),
            }),
        );
        assert!(mismatch.contains("ranges overlap"), "{mismatch}");
        assert_eq!(
            refused(Some("go away".into()), None),
            "peer rejected authentication: go away",
            "an older peer's text, with nothing to add"
        );
        assert_eq!(
            refused(None, Some(AuthFailure::IdentityRejected)),
            "peer rejected authentication: identity verification failed"
        );
    }

    /// A peer the user closed while its link was being re-opened is not
    /// relinked, and a session list arriving for it registers nothing: no
    /// word is drawn, and no tunnel is parked, for a peer that is gone.
    #[tokio::test(start_paused = true)]
    async fn a_peer_closed_during_its_reopen_is_left_closed() {
        let app = Arc::new(fixture_app());
        let (_upstream, _peer) = app.install_channel_peer("fedlocal", "fedremote");
        let conn = Arc::clone(&app.peer_manager.peers.lock().unwrap()["peer:1"]);
        app.close_peer("peer:1");

        let (connector, _opened) = channel_connector("newremote");
        let upstream = connector().await.expect("opens");
        assert!(relink(&app, &conn, "peer:1", upstream).is_none());
        app.peer_manager.reconcile_sessions(
            &app,
            &conn,
            "peer:1",
            vec![crate::federation::sample_remote_entry("other")],
        );
        let guard = crate::federation::lock(&conn);
        assert!(!guard.remote_to_local.contains_key("other"));
        assert!(!guard.remote_to_local.contains_key("newremote"));
        drop(guard);
        assert!(app.list_federated_sessions().is_empty());
    }

    /// A closed peer's link is not re-opened.
    #[tokio::test(start_paused = true)]
    async fn a_closed_peer_is_not_reopened() {
        let app = Arc::new(fixture_app());
        let (connector, mut opened) = channel_connector("fedremote");
        let (_upstream, peer) = app.peer_manager.install_channel_peer(
            &app,
            "peer:1",
            "fedlocal",
            "fedremote",
            connector,
        );
        app.close_peer("peer:1");
        drop(peer);
        tokio::time::sleep(UPSTREAM_DEADLINE * 4).await;
        assert!(opened.try_recv().is_err(), "no attempt to re-open");
    }

    /// The peer is silent only strictly past the deadline.
    #[test]
    fn upstream_silent_only_past_the_deadline() {
        let heard = Instant::now();
        for (after, silent) in [
            (Duration::ZERO, false),
            (UPSTREAM_DEADLINE, false),
            (UPSTREAM_DEADLINE + Duration::from_millis(1), true),
            (UPSTREAM_DEADLINE * 10, true),
        ] {
            assert_eq!(upstream_silent(heard, heard + after), silent, "{after:?}");
        }
        assert!(
            !upstream_silent(heard + Duration::from_secs(1), heard),
            "a frame newer than `now` is not silence"
        );
    }
}
