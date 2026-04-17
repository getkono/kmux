# Connection Subsystem

This document is the technical reference for the kmux connection subsystem as implemented across Phases 1–7. It covers the two-phase connection model, all bootstrap strategies, the transport supervisor, the wire format, server configuration, and the decision logic governing transport selection.

---

## Table of Contents

1. [Two-Phase Connection Model](#two-phase-connection-model)
2. [SessionContext](#sessioncontext)
3. [EndpointAdvert](#endpointadvert)
4. [Phase A — Bootstrap](#phase-a--bootstrap)
   - [Bootstrap Race](#bootstrap-race)
   - [Strategy 1: UdsLocalBootstrap](#strategy-1-udslocalbootstrap)
   - [Strategy 2: QuicDirectBootstrap](#strategy-2-quicdirectbootstrap)
   - [Strategy 3: TlsTcpDirectBootstrap](#strategy-3-tlstcpdirectbootstrap)
   - [Strategy 4: SshBootstrap](#strategy-4-sshbootstrap)
5. [Phase B — TransportSupervisor](#phase-b--transportsupervisor)
   - [UpgradeSignal](#upgradesignal)
   - [Scorer Formula](#scorer-formula)
   - [Supervisor Behavior](#supervisor-behavior)
6. [Supported Transports](#supported-transports)
   - [Wire Format](#wire-format)
   - [Listener Trait](#listener-trait)
   - [UDS Auth and Permissions](#uds-auth-and-permissions)
   - [TLS Trust (TOFU)](#tls-trust-tofu)
7. [Endpoint URL Scheme](#endpoint-url-scheme)
8. [Server Configuration](#server-configuration)
   - [Audience Enum](#audience-enum)
9. [Server Announcement Flow](#server-announcement-flow)
10. [probe-or-start JSON Format](#probe-or-start-json-format)
11. [Protocol Version Gate](#protocol-version-gate)
12. [ConnectionId and Session Resumption](#connectionid-and-session-resumption)
13. [Explicit Decision Table](#explicit-decision-table)
14. [Sequence Diagrams](#sequence-diagrams)
15. [Troubleshooting](#troubleshooting)
16. [Key File Index](#key-file-index)

---

## Two-Phase Connection Model

A kmux connection has two independent phases:

**Phase A — Bootstrap** (one-shot, small data): Obtain an authenticated `SessionContext` via any reachable path. The goal is to acquire a `SessionContext` as fast and reliably as possible. This phase is short-lived and tolerates any viable path, including SSH tunnels.

**Phase B — Data plane** (long-lived, high-throughput): A live transport chosen and continuously supervised for RTT, connectivity, and reliability. Transports are scored continuously and can be swapped on the fly without user intervention or session loss.

The two phases are deliberately independent. The transport used to bootstrap does not constrain which transport becomes the data plane. A session that bootstrapped over SSH can immediately switch its data plane to QUIC once the supervisor determines QUIC is available and higher-scored.

---

## SessionContext

`SessionContext` is the output of Phase A and the input to Phase B. It carries everything needed to begin the data plane and re-authenticate after transport swaps.

**Defined in:** `crates/kmux-protocol/src/transport/bootstrap.rs`

```rust
pub struct SessionContext {
    pub token: String,
    pub connection_id: ConnectionId,
    pub server_endpoints: Vec<EndpointAdvert>,
    pub bootstrap_transport: TransportKind,
    pub send: mpsc::UnboundedSender<ClientMessage>,
}
```

| Field | Description |
|-------|-------------|
| `token` | Auth token used on all subsequent transport connections |
| `connection_id` | Server-assigned session identity; preserved across transport swaps |
| `server_endpoints` | Advertised endpoints filtered by audience for this caller |
| `bootstrap_transport` | The transport kind that won the bootstrap race |
| `send` | Active sender on the bootstrap transport (used until a higher-scored transport is promoted) |

---

## EndpointAdvert

Represents one advertised transport endpoint. The supervisor uses these to probe and score candidate transports during Phase B.

**Defined in:** `crates/kmux-protocol/src/transport/bootstrap.rs`

```rust
pub struct EndpointAdvert {
    pub kind: TransportKind,
    pub address: String,  // "host:port" for QUIC/TLS-TCP, absolute path for UDS
}
```

---

## Phase A — Bootstrap

Implemented in `crates/kmux-client/src/bootstrap.rs`.

### Bootstrap Race

`bootstrap_race` runs all applicable strategies concurrently using `futures::stream::FuturesUnordered`. The first strategy to return `Ok(SessionContext)` wins; all remaining in-flight strategies are dropped immediately.

Error handling during the race:

| Error | Behaviour |
|-------|-----------|
| `BootstrapError::NotAvailable` | Silently skipped; strategy is not applicable for this target |
| `BootstrapError::VersionMismatch` | Immediately fatal; propagated without trying remaining strategies |
| Any other error | Recorded with a per-strategy description; remaining strategies continue |
| All strategies exhausted | `AllFailed(Vec<String>)` with per-strategy failure descriptions |

`VersionMismatch` is never retried. If any strategy detects a protocol version mismatch, the entire bootstrap halts.

### Strategy 1: UdsLocalBootstrap

- Calls `daemon::ensure_daemon()` to start the local daemon process if it is not already running.
- Connects to the data socket at `$XDG_RUNTIME_DIR/kmux/daemon-data.sock`. Debug builds (`cfg(debug_assertions)`) resolve this subdirectory to `kmux-debug/` instead, so a `cargo run` daemon can coexist with an installed release daemon on the same machine. Applies to every file under the runtime dir — control socket, data socket, PID, and auth token.
- Wins in microseconds when the daemon is already running locally.
- Returns `NotAvailable` when the target is not the local host.

### Strategy 2: QuicDirectBootstrap

- Performs a direct QUIC handshake to `host:port`.
- Uses the existing `connect::connect()` function.
- Returns a `SessionContext` with a placeholder `connection_id` that is updated when `AuthResult` arrives.
- Returns `NotAvailable` when no QUIC endpoint is resolvable for the target.

### Strategy 3: TlsTcpDirectBootstrap

- Performs a direct TLS-over-TCP handshake to `host:port`.
- Uses `tcp_connect::connect_tcp_tls()`.
- Useful when UDP is blocked by a firewall or when the QUIC strategy times out first.
- Returns `NotAvailable` when no TCP+TLS endpoint is resolvable for the target.

### Strategy 4: SshBootstrap

This strategy is the escalation path when direct transports are unreachable or when the target is only reachable via SSH.

1. Runs `ssh user@host kmuxd probe-or-start` to obtain a JSON connection info blob (token, endpoints, protocol version).
2. Verifies `protocol_version` from the JSON response (see [Protocol Version Gate](#protocol-version-gate)).
3. Establishes an SSH `-L` tunnel: `ssh -L 0:127.0.0.1:{tcp_port} -N user@host`.
4. Detects the allocated local port from SSH stderr by matching the pattern:
   `debug1: Local forwarding listening on 127.0.0.1 port NNNNN.`
5. Connects TLS-TCP to `127.0.0.1:{local_port}`. Plaintext is never used inside SSH tunnels.
6. Sends `Auth` with the token and obtains `AuthResult`.

SSH bootstrap has a 10-second budget to start and respond. If `probe-or-start` starts a stopped daemon, it polls the control socket with 200 ms retries up to 50 times before giving up.

---

## Phase B — TransportSupervisor

Implemented in `crates/kmux-client/src/supervisor.rs`. This replaces the former `quic_probe.rs`.

### UpgradeSignal

```rust
pub struct UpgradeSignal {
    pub new_kind: TransportKind,
    pub sender: mpsc::UnboundedSender<ClientMessage>,
}
```

When the supervisor promotes a new transport, it sends an `UpgradeSignal` to `SessionManager`. The `SessionManager.apply_transport_upgrade(sender, kind)` method atomically swaps the active sender and drops the old one.

### Scorer Formula

Every candidate transport is assigned a score at each evaluation cycle. The transport with the highest score becomes the data plane (subject to hysteresis).

```
score(transport) =
    locality_bonus(transport)        // +1000 for UDS when target is local
  + robustness_weight(transport)     // UDS 30, QUIC 20, TLS-TCP 10
  + server_priority(transport)       // from EndpointAdvert.priority (admin-set)
  - latency_ms_ewma(transport)       // α=0.2; unknown → 500ms assumed
  - failure_penalty(transport)       // +100 per recent failure; decays via record_success()
  - oscillation_penalty(transport)   // +200 if swapped away in last 60s (hysteresis)
```

**Constants:**

| Constant | Value |
|----------|-------|
| `LOCALITY_BONUS` | 1000 |
| `OSCILLATION_PENALTY` | 200 |
| `OSCILLATION_WINDOW` | 60 s |
| `PROBE_INTERVAL` | 30 s |
| `UNKNOWN_LATENCY_PENALTY` | 500 |

The locality bonus ensures UDS is overwhelmingly preferred for local targets, even with poor (but measured) RTT on other transports.

### Supervisor Behavior

- Every 30 seconds, the supervisor probes all non-active candidate transports.
- A successful probe updates the EWMA latency for that transport and may issue an `UpgradeSignal`.
- RTT is measured at the application layer using `Ping`/`Pong` messages. ICMP is never used.
- A failing probe adds a failure penalty of 100 to the transport's score. Penalty decays when `record_success()` is called.
- If the current transport stalls, it is marked `Degraded`, re-scored, and may be swapped. A `warn!` log is emitted.
- Failed probes use an exponential backoff: 30 s → 60 s → 5 min.
- The oscillation penalty prevents flapping: a transport swapped away within the last 60 seconds receives a −200 penalty, blocking a return swap even if its raw score would otherwise win.
- If all candidate transports fail, the session is disconnected and the supervisor enters backoff before retrying.

---

## Supported Transports

Three data transports are supported, all implemented under `crates/kmux-protocol/src/transport/`:

| Transport | Feature flag | Use case |
|-----------|-------------|----------|
| QUIC | `quic` | Preferred on internet/VPN; multiplexed streams, 0-RTT reconnect |
| TCP+TLS | `tcp-tls` | LAN and UDP-blocked networks; SSH tunnel inner layer |
| UDS | `uds` | Local same-host IPC; lowest overhead |

### Wire Format

All transports share the same wire format: postcard serialization with length-prefix framing, implemented in `crates/kmux-protocol/src/codec.rs` (formerly `frame.rs`). The `read_frame` and `write_frame` functions are generic over any `AsyncRead + AsyncWrite` pair, making the codec transport-agnostic.

### Listener Trait

The server accepts connections through a uniform `Listener` trait defined in `crates/kmux-protocol/src/transport/mod.rs`:

```rust
pub trait Listener: Send {
    fn kind(&self) -> TransportKind;
    async fn accept(&mut self) -> Result<IncomingSession, AcceptError>;
}

pub struct IncomingSession {
    pub read: Box<dyn AsyncRead + Unpin + Send>,
    pub write: Box<dyn AsyncWrite + Unpin + Send>,
    pub extra: Box<dyn Any + Send>,  // QUIC carries quinn::Connection
    pub kind: TransportKind,
    pub span: tracing::Span,
}
```

The server dispatches all transports through a single `dispatch_session` → `run_client_session` path in `crates/kmuxd/src/client_handler/session.rs`. Transport-specific setup (e.g., stream opening for QUIC) occurs before the session handler is invoked; the handler itself is generic.

### UDS Auth and Permissions

- The data socket lives at `$XDG_RUNTIME_DIR/kmux/daemon-data.sock`. This is separate from the control socket at `daemon.sock`.
- The socket is created with mode 0600 via umask, restricting access to the owning user.
- Authentication still uses a token in the first frame for protocol uniformity. When `allow_peer_cred = true` is set in `kmuxd.toml`, the server also accepts a matching peer UID as sufficient proof.

### TLS Trust (TOFU)

TLS certificate trust uses a Trust-on-First-Use (TOFU) model implemented in `crates/kmux-protocol/src/tls/tofu.rs`. The trust store lives at `~/.config/kmux/known_hosts.toml`.

Trust resolution flow:

1. Try system roots. If the certificate validates against system roots, pin it quietly and proceed.
2. If system validation fails and a pin exists in `known_hosts.toml`: compare SHA-256 fingerprints. A mismatch is a hard failure (possible MITM).
3. If system validation fails and no pin exists: auto-pin with a `tracing::warn!` and proceed (first connection to a self-signed or private-CA server).

The `--accept-invalid-certs` flag bypasses all checks. This is intended only for development.

---

## Endpoint URL Scheme

Defined in `crates/kmux-protocol/src/endpoint.rs`:

| Form | Meaning |
|------|---------|
| `quic://host:8443` | QUIC (preferred internet) |
| `tcp+tls://host:8444` | TCP+TLS (UDP-blocked fallback) |
| `unix:///run/user/1000/...` | UDS (local only) |
| `ssh://[user@]host[:port]` | Bootstrap via SSH probe-or-start |
| `user@host[:port]` | Sugar for `ssh://` |
| `host:port` | Sugar for `quic://host:port` |
| `@alias` | Lookup in `hosts.toml` |

---

## Server Configuration

The server configuration file (`kmuxd.toml`) is located at `$XDG_CONFIG_HOME/kmuxd/kmuxd.toml` or `/etc/kmuxd/kmuxd.toml`. The schema and resolution logic are implemented in `crates/kmuxd/src/config.rs`.

```toml
version = 1
runtime_dir = "auto"   # resolves to $XDG_RUNTIME_DIR/kmux

[tls]
cert = "/etc/kmuxd/cert.pem"
key  = "/etc/kmuxd/key.pem"
# self_signed = true    # development only

[[listen]]
kind = "quic"           # "quic" | "tcp+tls" | "unix"
bind = "::"
port = 8443
enabled = true
audience = "any"        # "any" | "lan" | "local" | "ssh-only"
priority = 0

[[listen]]
kind = "tcp+tls"
bind = "127.0.0.1"
port = 8444
audience = "ssh-only"  # only visible via SSH bootstrap

[[listen]]
kind = "unix"
path = "auto"          # resolves to runtime_dir/daemon-data.sock
audience = "local"

[advertise]
public_host = "prod.example.com"   # substituted in advertised addresses

[auth]
token_file = "auto"
allow_peer_cred = true    # UDS: accept peer uid match in lieu of token
```

**Default config (when no file is found):** QUIC on `[::]:8443` (audience: any), TCP+TLS on `[::]:8444` (audience: any), UDS auto (audience: local).

The server is the sole authority on which endpoints are visible to which callers. Clients do not probe or guess; they use what the server announces.

### Audience Enum

The `audience` field on each listener controls which callers receive that endpoint in their `EndpointAdvert` list. Filtering is implemented in `crates/kmuxd/src/announce.rs`.

| Value | Visible to |
|-------|------------|
| `any` | Always announced to all callers |
| `local` | Only UDS control-socket clients or loopback peers |
| `lan` | Only RFC-1918 / link-local peers (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16) |
| `ssh-only` | Only inside SSH `probe-or-start` JSON responses |

---

## Server Announcement Flow

1. Client connects on any transport and sends `ClientMessage::Auth`.
2. Server responds with `ServerMessage::AuthResult` containing `success`, `connection_id`, and the filtered endpoint list for this caller.
3. The daemon control socket (`serve_control_socket` in `crates/kmuxd/src/daemon.rs`) additionally returns a `StatusResponse` JSON blob including `protocol_version`, `kmuxd_version`, `token`, and `endpoints`. This is used by the local UDS bootstrap strategy.
4. `kmuxd probe-or-start` (used by the SSH bootstrap strategy) returns a similar JSON blob to stdout.

---

## probe-or-start JSON Format

```json
{
  "protocol_version": 13,
  "kmuxd_version": "0.1.0",
  "quic_port": 8443,
  "tcp_port": 8444,
  "token": "...",
  "endpoints": [
    {"kind": "quic",    "address": "host:8443"},
    {"kind": "tcp+tls", "address": "host:8444"}
  ]
}
```

The `endpoints` array reflects the audience-filtered view for an SSH caller — typically including only transports marked `ssh-only` or `any` on the remote server. The client selects the best endpoint from this list to establish the TLS-TCP tunnel.

---

## Protocol Version Gate

Every `StatusResponse` carries `protocol_version`. The current value is `13`, defined as `PROTOCOL_VERSION` in `crates/kmux-protocol/src/messages/mod.rs`.

- The SSH bootstrap (`crates/kmux-client/src/ssh/negotiate.rs`) reads `protocol_version` from the `probe-or-start` JSON and compares it to `PROTOCOL_VERSION`.
- A mismatch produces `SshError::VersionMismatch` → `BootstrapError::VersionMismatch`.
- `bootstrap_race` propagates `VersionMismatch` immediately without attempting other strategies.
- Daemons that predate the `protocol_version` field and omit it are accepted with a `tracing::warn!`. This grace period exists for rollout compatibility and may be removed in a future version.

---

## ConnectionId and Session Resumption

`ConnectionId` is a server-assigned `u64` provided in the first `AuthResult`. It is the stable identity of a session across transport swaps.

When the supervisor promotes a new transport:

1. The client opens a connection on the new transport.
2. It sends `ClientMessage::Auth` with the existing `token` and `connection_id`.
3. The server calls `build_attach_replay` to reconstruct session state and resume the session.
4. The server responds with `AuthResult { success: true }`.
5. `apply_transport_upgrade` atomically replaces the active sender; the old transport connection is dropped.

No session state (panes, scrollback, environment) is lost during transport swaps.

---

## Explicit Decision Table

| Situation | Action |
|-----------|--------|
| Target is local AND server announces `unix://` | UDS immediately, no race |
| Target is local AND no `unix://` announced | Race QUIC-loopback ‖ TLS-TCP-loopback |
| Tier 1 direct all fail AND SSH target known | Escalate to SSH bootstrap (logged) |
| Tier 1 direct all fail AND no SSH info | Hard-fail `NoViablePath` |
| Server announces only `tcp+tls://127.0.0.1:N` | Must use SSH bootstrap; data plane = TLS-TCP-over-forward |
| Protocol version mismatch | Halt with `VersionMismatch { client, server }` — no retry |
| Remote `kmuxd` not installed (SSH) | Halt with `RemoteNotInstalled` |
| Remote `kmuxd` installed but off (SSH) | `probe-or-start` starts it; bootstrap continues |
| ICMP blocked | Irrelevant — RTT measured via `Ping`/`Pong` inside kmux protocol |
| Current transport stalls | Supervisor marks `Degraded`, re-scores, possibly swaps; emits `warn!` log |
| `UpgradeProbe` fails | Adds failure penalty; retry after 30s/60s/5min backoff |
| Hysteresis active (swapped within 60s) | Oscillation penalty; swap refused even if score says upgrade |
| All candidate transports fail | Session disconnected; supervisor retries in backoff |

---

## Sequence Diagrams

### (a) Direct Bootstrap with SSH Fallback

```
Client                          Server
  |                               |
  |--[QUIC handshake]------------>|  (2s budget, concurrent with TLS-TCP attempt)
  |<--[QUIC refused/timeout]------|
  |                               |
  |--[ssh user@host kmuxd probe-or-start]-->
  |<--[JSON: token, endpoints]------------|
  |                               |
  |--[ssh -L 0:127.0.0.1:8444 -N]-->  (tunnel established)
  |--[TLS-TCP connect 127.0.0.1:{local}]-->|
  |--[Auth{token, connection_id}]-------->|
  |<--[AuthResult{success, conn_id}]------|
  | (Session active over TLS-TCP tunnel)
```

### (b) SSH Bootstrap Starting a Stopped Daemon

```
Client          SSH subprocess          Remote kmuxd
  |                  |                       |
  |--probe-or-start->|                       |
  |                  |--stat daemon.sock---->|  (no response)
  |                  |--spawn kmuxd --daemon->|
  |                  |--poll control socket-->|  (retry 200ms × 50)
  |                  |<--{token, ports}-------|
  |<--JSON-----------|
```

### (c) Transport Hot-Swap (QUIC to TLS-TCP Downgrade)

```
Client                          Server
  | (data plane: QUIC)            |
  |                               |
  | Supervisor: QUIC RTT spikes   |
  | Scorer: TLS-TCP now higher    |
  |                               |
  |--[TLS-TCP Auth{conn_id}]----->|  (background, new transport)
  |<--[AuthResult{success}]-------|
  |<--[ChannelSwitched]-----------|
  |                               |
  | apply_transport_upgrade()     |
  | (drop old QUIC sender)        |
  | (data plane: TLS-TCP)         |
```

---

## Liveness and Recovery

The data plane is kept honest by a bidirectional application-layer
ping/pong — **no ICMP, no TCP keepalives**. This matches the server-
announces principle: everything runs over the real protocol, so we
observe the same failure modes the user observes.

### Ping cadence

Both endpoints mirror the same policy (`crates/kmux-client/src/liveness.rs`,
`crates/kmuxd/src/client_handler/session.rs`):

| Direction | Interval | Timeout |
|-----------|----------|---------|
| server → client `Ping` | 5 s | — |
| client → server `Ping` | 5 s | 15 s silence → declare dead |

The client resets its timeout on *any* inbound frame (not just `Pong`),
so a chatty session never spuriously times out. `client_ping_due` on
the liveness tracker is sampled every second from the event loop —
timers are cheap and the loop is already running for rendering.
Worst-case detection of a hung daemon (kill -STOP, blackholed path) is
bounded at ~15 s.

### ConnectionState machine

```
          ┌─────┐
          │Idle │──── initial connect ───▶┐
          └─────┘                         │
                                          ▼
                              ┌─────────────────┐
                              │   Handshaking   │
                              └─────────────────┘
                                  │         │
                     auth ok      │         │ bootstrap failed
                                  ▼         ▼
                    ┌───────────────────┐  ┌──────────────────┐
                    │Connected{transport}│  │Disconnected{...}│
                    └───────────────────┘  └──────────────────┘
                          │       ▲                ▲
        liveness timeout /│       │                │ user chooses y/Enter
        server close /    │       │                │
        tunnel died       │       │                │
                          ▼       │                │
                    ┌──────────────────┐           │
                    │Disconnected{...} │───────────┘
                    └──────────────────┘
```

There is exactly one source of truth (`ConnectionState`); the TUI
badge, the session manager's legacy `connected: bool`, and the disconnect
overlay all read from it.

### Freeze-and-confirm UX

When a drop is detected (channel closed, ping timeout, SSH tunnel died)
the event loop does **not** auto-retry. Instead:

1. `SessionManager::mark_connection_lost_with(reason)` moves the state
   to `Disconnected { reason }`.
2. `Mode::Disconnected { reason }` is set in the TUI. The event loop
   continues to render, but key input stops forwarding to PTYs — only
   `y` / `Enter` (confirm reconnect) and `q` (quit) are handled.
3. A centred overlay shows the reason and the prompt. Pressing `y`
   calls `App::attempt_reconnect`, which re-runs the original bootstrap
   flow (SSH re-negotiation + `connect_via_ssh_session`, or a fresh
   `mgr.connect` for direct/local targets).

`Ctrl+Alt+R` from `Mode::Normal` triggers the same reconnect path
without waiting for the liveness timeout — useful when the link feels
degraded but has not yet tripped the 60 s timer.

### "Server is down" case

The issue originally asked for ICMP-based "restart daemon over SSH."
That is replaced by two existing pieces:

- If the user originally connected via SSH (`self.ssh_target` is set),
  reconnect re-runs `ssh::negotiate`, which invokes `kmuxd
  probe-or-start` on the remote host. `probe-or-start` starts the
  daemon if it is missing, matching the issue's intent.
- If the user connected directly and the server is gone, the bootstrap
  race fails and the overlay reason reads `reconnect failed: …`. The
  user must fix the daemon manually or use the server picker to switch
  to an SSH target. This is deliberate — we never silently initiate
  OS-level actions the user did not authorise.

### Tracing

Disconnect and reconnect events are emitted with structured fields on
the connection span:

```
WARN connection dropped connection_id=… transport=QUIC reason="ping timeout"
INFO reconnect requested connection_id=…
```

Filter with `RUST_LOG=kmux_client=debug,kmux=info` to watch the
lifecycle.

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| `VersionMismatch { client: X, server: Y }` | `kmuxd` and `kmux` versions mismatch | Update both to the same version |
| `RemoteNotInstalled` | `kmuxd` not in `$PATH` on remote | Install `kmuxd` on the remote host |
| SSH bootstrap times out (10s) | Daemon fails to start | Check `~/.local/share/kmux/kmuxd.log` on the remote |
| QUIC connection refused, TLS-TCP works | UDP blocked by firewall | Normal; `TransportSupervisor` will stick with TLS-TCP |
| TLS fingerprint mismatch | Server cert rotated | Delete the stale entry from `~/.config/kmux/known_hosts.toml` |
| All transports fail | No network path | Check firewall; verify `kmuxd` is listening on correct ports |
| Sessions lost after restart | Checkpoint failed | Check disk space; `$XDG_DATA_HOME/kmux/session_state.bin` |

---

## Key File Index

| File | Role |
|------|------|
| `crates/kmux-protocol/src/transport/bootstrap.rs` | `Bootstrap` trait, `SessionContext`, `EndpointAdvert`, `BootstrapError` |
| `crates/kmux-protocol/src/transport/mod.rs` | `Listener` trait, `IncomingSession`, `PaneAttacher` |
| `crates/kmux-protocol/src/transport/quic.rs` | QUIC listener and client helpers |
| `crates/kmux-protocol/src/transport/tcp_tls.rs` | TLS-TCP listener and client helpers |
| `crates/kmux-protocol/src/transport/uds.rs` | UDS listener and client helpers |
| `crates/kmux-protocol/src/tls/tofu.rs` | TOFU certificate pinning |
| `crates/kmux-protocol/src/endpoint.rs` | `Endpoint` URL parser |
| `crates/kmux-protocol/src/codec.rs` | `read_frame`/`write_frame` (postcard + length-prefix) |
| `crates/kmux-client/src/bootstrap.rs` | Bootstrap strategies and `bootstrap_race` |
| `crates/kmux-client/src/supervisor.rs` | `TransportSupervisor`, `TransportScorer`, `UpgradeSignal` |
| `crates/kmux-client/src/ssh/negotiate.rs` | SSH `probe-or-start` and tunnel setup |
| `crates/kmuxd/src/config.rs` | `kmuxd.toml` schema and `ServerConfig::resolve()` |
| `crates/kmuxd/src/announce.rs` | Audience-aware endpoint advertisement |
| `crates/kmuxd/src/startup.rs` | Listener loop over `ServerConfig.listeners` |
| `crates/kmuxd/src/daemon.rs` | Control socket server and `StatusResponse` |
| `crates/kmuxd/src/client_handler/session.rs` | `run_client_session` (generic over all transports) |
