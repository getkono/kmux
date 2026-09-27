# Connection Subsystem

This document is the technical reference for how kmux processes connect: which
process dials which, over which transport, how a link is supervised, resumed and
re-established, and how the daemon bounds what a slow or silent peer can cost.
The protocol those links carry — frames, messages, state machines, timing and
errors — is specified in [protocol.md](protocol.md), which this document does
not restate.

> See also [connection-pause.md](connection-pause.md) for bandwidth-saving connection pausing (issue #68): how a paused client stops receiving terminal output and catches up to the final state on resume.

---

## Table of Contents

1. [Architecture: GUI → local daemon → federated peer](#architecture-gui--local-daemon--federated-peer)
2. [Bootstrap](#bootstrap)
   - [Local daemon](#local-daemon)
   - [SSH](#ssh)
3. [Transport supervision](#transport-supervision)
4. [Supported Transports](#supported-transports)
   - [Listener Trait](#listener-trait)
   - [UDS Auth and Permissions](#uds-auth-and-permissions)
   - [TLS Trust (TOFU)](#tls-trust-tofu)
5. [Endpoint URL Scheme](#endpoint-url-scheme)
6. [Server Configuration](#server-configuration)
7. [probe-or-start and the status reply](#probe-or-start-and-the-status-reply)
8. [ConnectionId and Session Resumption](#connectionid-and-session-resumption)
9. [Transport override](#transport-override-issue-69)
10. [Liveness and Recovery](#liveness-and-recovery)
11. [Troubleshooting](#troubleshooting)
12. [Per-Connection Metrics](#per-connection-metrics)
13. [Daemon Binary Resolution](#daemon-binary-resolution)
14. [Idle Shutdown](#idle-shutdown)
15. [Server-Side Flow Control and Deadlines](#server-side-flow-control-and-deadlines)
16. [Key File Index](#key-file-index)

---

## Architecture: GUI → local daemon → federated peer

```text
 GUI (GTK / Swift)            local kmuxd (hub)                  remote kmuxd (peer)
 ─────────────────            ─────────────────                  ───────────────────
 FrontendDriver ──UDS──────▶  client_handler ── federation ──▶  client_handler
   SessionManager              local sessions    link: TCP+TLS     its sessions
                               proxied sessions  (Direct, or through
                                                  an SSH -L tunnel)
```

- **A GUI never dials a remote.** A GUI always bootstraps to its **local**
  daemon over its Unix socket (`ResolvedTarget::LocalDaemon`,
  `AppCore::current_target`), starting the daemon if it is not running, and
  reconnects to it the same way.
- **A remote host is a federated peer of the local daemon.** The GUI sends
  `OpenPeer`; the local `kmuxd` opens and keeps the one upstream link to the
  remote `kmuxd` (SSH-negotiated, or direct TCP+TLS) and proxies its sessions,
  under local session words, to every local client. Losing, re-opening and
  closing that link is described in
  [architecture-federation.md](architecture-federation.md).
- **Remote transports are the federation link's and the CLI's.** A hub
  reaches its peer with the SSH negotiation below (or directly) and TCP+TLS;
  the `remote`-feature CLI paths (`kmux --dry-run` / `--test` against
  `user@host`, `kmux daemon logs --server`) use the SSH bootstrap, and the
  `--test` diagnostic also runs the `TransportSupervisor`. None of this is part
  of the GUI's own connection.

Daemon data-plane ports (QUIC, TCP+TLS) are ephemeral — bound on port `0`, the
OS assigns them, and they are advertised in the SSH `probe-or-start` reply. They
never appear on a user-facing CLI surface. The only user-typeable port is the
**SSH** port (`host:2222` or `--ssh-port 2222`).

---

## Bootstrap

`kmux_connect::pipeline::run_bootstrap` takes a `ResolvedTarget` and returns a
`BootstrapOutcome` — an authenticated sender, the transport, the
`connection_id`, and the daemon run it reached (`daemon_instance`). It is the one
code path the GUI, the `--dry-run` / `--test` diagnostics and the CLI use; each
step is reported to a `BootstrapObserver`. There is no race between strategies:
the target decides the path.

| `ResolvedTarget` | Path |
|---|---|
| `LocalDaemon` | [Local daemon](#local-daemon): ensure it runs, then UDS |
| `Ssh { target, accept_invalid_certs }` | [SSH](#ssh): `probe-or-start`, an `-L` tunnel, TCP+TLS through it |

Both end the same way: `Auth` → `AuthChallenge` → `AuthProof` → `AuthResult`
([protocol.md#handshake](protocol.md#handshake)), waiting at most
`AUTH_REPLY_TIMEOUT` for the result. A refusal becomes
`BootstrapError::VersionMismatch` (a protocol mismatch, decided on
`AuthResult.failure`) or `BootstrapError::Auth`; a link that closes before the
daemon answers is `BootstrapError::Connect`, which a retry may get past.

### Local daemon

1. Query the control socket (`$XDG_RUNTIME_DIR/kmux/daemon.sock`) for the
   daemon's status; start it (`kmux_connect::daemon::ensure_daemon`, see
   [Daemon Binary Resolution](#daemon-binary-resolution)) if nothing answers.
2. Refuse, before opening the data plane, a daemon whose `protocol_range` does
   not overlap ours (`kmux_protocol::compat::protocol_match`). A debug and a
   release build never meet here: each resolves its own runtime directory.
3. Connect to the data socket (`daemon-data.sock`) and authenticate with the
   token the status reply carried.

Debug builds (`cfg(debug_assertions)`) resolve the runtime directory to
`kmux-debug/` instead of `kmux/`, so a `cargo run` daemon can coexist with an
installed release daemon ([profile-isolation.md](profile-isolation.md)).

### SSH

Implemented in `crates/kmux-connect/src/ssh/negotiate.rs`.

1. Run `ssh user@host kmuxd probe-or-start` to obtain a JSON reply (token,
   endpoints, protocol range, daemon version; see
   [probe-or-start](#probe-or-start-and-the-status-reply)).
2. Verify that `protocol_range` overlaps the client's supported range. A reply
   carrying only the frozen legacy `protocol_version` sentinel is refused
   (`SshError::VersionMismatch`).
3. **Pre-allocate a free local TCP port** by binding `127.0.0.1:0`, capturing the port, and dropping the listener. The kernel-chosen port is then passed verbatim to `-L`.
4. Spawn `ssh -L <localport>:127.0.0.1:<remoteport> -N user@host` with `-o ExitOnForwardFailure=yes` so the process exits immediately if the remote forward can't be established.
5. **Verify the tunnel by TCP-connecting to the local port** with exponential-backoff retries (40 ms → 500 ms cap, 15 s deadline). Concurrently watch the ssh child for an early exit; if it dies before the local port becomes connectable, surface the captured stderr.
6. Connect TCP+TLS to `127.0.0.1:<localport>`, pinning the certificate to the *real* remote `host:tcp_port`. Plaintext is never used inside SSH tunnels.
7. Authenticate with the token from step 1.

A federation hub reaching an `Ssh` peer runs the same negotiation on every
(re-)open, which is how it finds a restarted peer's new token and ports.

#### Why we don't parse `ssh -v` stderr

Earlier revisions read `debug1: Local forwarding listening on 127.0.0.1 port NNNNN.` out of the tunnel's stderr to learn the local port (the spec used `-L 0:127.0.0.1:<remote>` to let the kernel pick). That parser scanned for the substring `port <digits>` line-by-line — but `ssh -v` emits `debug1: Connecting to <host> [<ip>] port 22.` *before* the forwarding line, so the parser returned `22`. The client then TLS-handshook against the local sshd, which produced an opaque "ssh negotiation" error and no log entry on the daemon. Pre-allocating the port and probing TCP for readiness eliminates the entire class of fragile-stderr-scraping bugs.

#### Stderr capture

Tunnel stderr is always piped and drained off-thread into a 50-line ring buffer, mirrored to `tracing::debug!`. Every `SshError` variant that involves an ssh subprocess (`ProbeFailed`, `TunnelDiedEarly`, `TunnelUnreachable`) embeds the captured stderr tail directly in its `Display` so the user sees `Permission denied (publickey)`, `Host key verification failed`, `Connection timed out`, etc. without needing `RUST_LOG=debug`.

#### Error classification

`ProbeFailureKind` classifies probe-stage exits by ssh's exit code:

| Exit code | Kind | Meaning |
|-----------|------|---------|
| 127 | `RemoteDaemonNotInstalled` | Remote shell could not exec `kmuxd` (PATH issue or not installed). |
| 255 | `SshFailed` | ssh-internal failure: auth, network, host-key, host-down. The captured stderr disambiguates these. |
| any other | `RemoteDaemonStartFailed` | kmuxd ran but probe-or-start exited non-zero. |

#### SSH option set

The shared `build_ssh_cmd` helper applies these options to every probe and tunnel invocation:

| Option | Why |
|--------|-----|
| `BatchMode=yes` | Fail fast instead of prompting; no terminal is available to prompt on. Auth must be configured via ssh-agent / key files. |
| `StrictHostKeyChecking=accept-new` | TOFU on first connection, refuse on mismatch. Mirrors the data-plane TOFU model. |
| `ConnectTimeout=10` | Bound network failures so unreachable hosts surface a clear error instead of hanging. |

Tunnel-only options:

| Option | Why |
|--------|-----|
| `ExitOnForwardFailure=yes` | Tunnel process exits immediately if the remote forward can't be set up. |
| `ServerAliveInterval=15` / `ServerAliveCountMax=3` | Keepalive so the tunnel detects black-holed networks within ~45 s instead of sitting idle. |

#### Timeouts

| Phase | Timeout |
|-------|---------|
| `kmuxd probe-or-start` (whole invocation) | 20 s (`PROBE_TIMEOUT`) |
| Local-tunnel-port readiness | 15 s (`TUNNEL_READY_TIMEOUT`) |

`kmuxd probe-or-start` itself polls its control socket for up to 10 s waiting for a fresh daemon to come up; the 20 s ssh-side cap leaves room for SSH handshake and authentication.

---

## Transport supervision

An SSH-bootstrapped link can run a `TransportSupervisor`
(`crates/kmux-connect/src/supervisor.rs`) — the CLI's `--test` does — which
probes candidate transports (`EndpointAdvert { kind, address }`,
`crates/kmux-sys/src/transport/mod.rs`) and promotes a better one. The
candidates are the tunnelled TCP+TLS link itself and direct QUIC to the
remote's `quic_port` from the `probe-or-start` reply; the reply's endpoint list
is not read.

A **probe** connects on the candidate transport and authenticates with the
link's `connection_id` and the daemon run that assigned it; it succeeds only if
the daemon *resumed* that connection in that run (`probe_verdict`) — a daemon
restarted since would register the probe afresh, with none of the link's pane
streams. A successful probe is sent as an `UpgradeSignal { new_kind, sender }`;
the driver sends `ChannelReady` on the new sender and calls
`SessionManager::apply_transport_upgrade`, which makes it the live sender and
drops the old one (the daemon answers `ChannelSwitched`). A refused probe's
`AuthResult` never reaches the session manager.

Each candidate is scored every `PROBE_INTERVAL` (30 s):

```
score(transport) =
    locality_bonus(transport)        // LOCALITY_BONUS_UDS (1000) for UDS when the target is local
  + robustness_weight(transport)     // UDS 30, QUIC 20, TCP+TLS 10
  - latency_ms_ewma(transport)       // α = 0.2; LATENCY_UNKNOWN_MS (500) until measured
  - failure_penalty(transport)       // FAILURE_PENALTY_PER (100) per failure within FAILURE_WINDOW (300 s)
  - oscillation_penalty(transport)   // OSCILLATION_PENALTY (200) if swapped away within OSCILLATION_WINDOW (60 s)
```

RTT is measured with the protocol's own `Ping` / `Pong`, never ICMP. The
oscillation penalty keeps a transport that was just swapped away from being
swapped straight back.

---

## Supported Transports

Three data transports, all implemented under `crates/kmux-sys/src/transport/`:

| Transport | Feature flag | Use case |
|-----------|-------------|----------|
| QUIC | `quic` | Internet/VPN; one unidirectional stream per attached pane |
| TCP+TLS | `tcp-tls` | LAN, UDP-blocked networks, the inner layer of an SSH tunnel, federation links |
| UDS | `uds` | Local same-host IPC (the GUI's link); lowest overhead |

All three carry the same frames ([protocol.md#frames](protocol.md#frames)).

### Listener Trait

The server accepts connections through a uniform `Listener` trait defined in `crates/kmux-sys/src/transport/mod.rs`:

```rust
pub trait Listener: Send {
    fn kind(&self) -> TransportKind;
    /// Resolves as soon as the transport has a connection, before any handshake.
    async fn accept(&mut self) -> Result<PendingSession, AcceptError>;
}

impl PendingSession {
    /// Runs the TLS / QUIC handshake (none for UDS and plain TCP).
    pub async fn establish(self, timeout: Duration) -> Result<IncomingSession, AcceptError>;
}

pub struct IncomingSession {
    pub read: Box<dyn AsyncRead + Unpin + Send>,
    pub write: Box<dyn AsyncWrite + Unpin + Send>,
    pub peer: PeerInfo,
    pub span: tracing::Span,
    pub transport: SessionTransport,  // QUIC carries its quinn::Connection
}
```

`serve(listener, HANDSHAKE_TIMEOUT, HandshakeLimits::DAEMON, on_session)` is the accept loop: it spawns each pending connection's `establish` into that connection's own task, so a peer that stalls mid-handshake delays nobody else, and drops it after `HANDSHAKE_TIMEOUT` (`kmux_protocol::timing::TRANSPORT_HANDSHAKE_TIMEOUT`, 10 s).

**Handshake admission (issue #207, `crates/kmux-sys/src/transport/admission.rs`).** Every listener binds `0.0.0.0`, so anyone who can reach the port can start a handshake. The accept loop bounds how many run at once: `MAX_PENDING_HANDSHAKES` (64) in all and `MAX_PENDING_HANDSHAKES_PER_SOURCE` (8) from one source, where a source is an IPv4 address or an IPv6 /64. A connection that finds either bound reached is **refused at once**: the TCP socket is closed, the QUIC attempt is answered with a refusal. The loop never waits for a slot, so nothing queues behind a full listener, neither in the kernel's backlog nor in the QUIC endpoint, where each waiting attempt holds its buffered initial packets. One source that opens connections and never finishes them fills its own eight slots and nobody else's; starving real clients takes many distinct sources. A slot is given back when its handshake finishes, fails or times out.

QUIC can be sent from a spoofed address, which would let an attacker charge its handshakes to someone else's source. So once `QUIC_RETRY_ABOVE` (8) handshakes are in flight, a QUIC client that has not yet proved its address is sent a stateless Retry (QUIC address validation) before it is counted: the daemon keeps no state for it, and only a client that receives packets at its claimed address comes back, with a token that validates it. Below that load a connect skips the extra round trip. `HandshakeLimits::new` refuses a zero bound with a typed `HandshakeLimitsError`, since a listener allowed no handshakes could serve nobody.

The server dispatches all transports through a single `dispatch_session` → `run_client_session` path in `crates/kmuxd/src/client_handler/session.rs`. Transport-specific setup (e.g., stream opening for QUIC) occurs before the session handler is invoked; the handler itself is generic.

### UDS Auth and Permissions

- The data socket lives at `$XDG_RUNTIME_DIR/kmux/daemon-data.sock`. This is separate from the control socket at `daemon.sock`.
- The socket's mode is set to 0600 right after it is bound, restricting access to the owning user.
- Authentication is the same handshake as on every transport: the token and the identity proof. There is no peer-UID shortcut: the retired `[auth] allow_peer_cred` key never had one behind it, and a `kmuxd.toml` that still sets it loads with a warning (issue #227).

### TLS Trust (TOFU)

TLS certificate trust uses a Trust-on-First-Use (TOFU) model: the pin store is `crates/kmux-sys/src/tls/tofu.rs`, the verification flow `crates/kmux-sys/src/tls/verifier.rs`. The trust store lives at `~/.config/kmux/known_hosts.toml`.

Trust resolution flow:

1. Try system roots. If the certificate validates against system roots, pin it quietly and proceed.
2. If system validation fails and a pin exists in `known_hosts.toml`: compare SHA-256 fingerprints. A mismatch is a hard failure (possible MITM).
3. If system validation fails and no pin exists: auto-pin with a `tracing::warn!` and proceed (first connection to a self-signed or private-CA server).

The `--accept-invalid-certs` flag bypasses all checks. This is intended only for development.

---

## Endpoint URL Scheme

Parsed by `kmux_protocol::Endpoint` (`crates/kmux-protocol/src/endpoint.rs`). The GUI and the CLI resolve a user-typed target to an SSH peer or target; the direct forms are for the CLI and for tests:

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

The server configuration file (`kmuxd.toml`) is located at `$XDG_CONFIG_HOME/kmuxd/kmuxd.toml` or `/etc/kmuxd/kmuxd.toml`. The schema and resolution logic are implemented in `crates/kmuxd/src/config.rs` (`deny_unknown_fields`: a typo is an error).

```toml
version = 1
runtime_dir = "auto"   # resolves to $XDG_RUNTIME_DIR/kmux

[tls]
cert = "/etc/kmuxd/cert.pem"   # optional; omit cert+key for a self-signed cert
key  = "/etc/kmuxd/key.pem"

[[listen]]
kind = "quic"           # "quic" | "tcp+tls" | "unix"
bind = "0.0.0.0"
enabled = true
audience = "any"        # "any" | "lan" | "local" | "ssh-only"

[[listen]]
kind = "tcp+tls"
bind = "127.0.0.1"
audience = "ssh-only"  # only visible via SSH bootstrap

[[listen]]
kind = "unix"
path = "auto"          # resolves to runtime_dir/daemon-data.sock
audience = "local"

[advertise]
public_host = "prod.example.com"   # substituted in advertised addresses

[auth]
token_file = "auto"
```

A listener's **port** is not configurable here: every listener binds port `0`
and the kernel picks, unless `kmuxd` is started by hand with `--port` /
`--tcp-port` (the daemon lifecycle, a handoff successor included, always starts
it with `--port 0`). `priority` is parsed but not advertised, so the client's
scorer never sees it.

**Default config (when no file is found):** QUIC and TCP+TLS on `0.0.0.0` (audience: any), UDS auto (audience: local).

The server is the sole authority on which endpoints are visible to which callers. Clients do not probe or guess; they use what the server announces.

### Audience Enum

The `audience` field on each listener controls which callers receive that endpoint in their endpoint list. Filtering is implemented in `crates/kmuxd/src/announce.rs`.

| Value | Visible to |
|-------|------------|
| `any` | Always announced to all callers |
| `local` | Only UDS control-socket clients or loopback peers |
| `lan` | Only RFC-1918 / link-local peers (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16) |
| `ssh-only` | Only SSH `probe-or-start` replies: the status reply carries the SSH view as `ssh_endpoints` beside the local view (`endpoints`), and the probe hands it back; for a daemon that predates the field, the probe builds the SSH view from its ports |

---

## probe-or-start and the status reply

The daemon's control socket answers `{"command":"status"}` with a
`StatusResponse` (`kmux_protocol::control_rpc`): the ports, the token, the pid,
`protocol_range`, the build (`kmuxd_build`, `build_profile`), uptime, session
count, and the endpoints twice: `endpoints` is the local view (no `ssh-only`
listener), `ssh_endpoints` the SSH view (no `local` one). A local bootstrap
reads it before opening the data plane.

`kmuxd probe-or-start` (run over SSH) queries it — starting the daemon first if
none answers — and prints, as `endpoints`, the SSH view (built from the ports
for a daemon whose reply predates `ssh_endpoints`):

```json
{
  "protocol_version": 41,
  "protocol_range": {"min": {"major": 1, "minor": 0, "patch": 0}, "max": {"major": 1, "minor": 1, "patch": 0}},
  "kmuxd_version": "0.2.0",
  "quic_port": 43117,
  "tcp_port": 38809,
  "token": "...",
  "endpoints": [
    {"kind": "QUIC",    "address": "host:43117"},
    {"kind": "TCP+TLS", "address": "host:38809"}
  ]
}
```

- `protocol_range` is what a client negotiates against; the integer
  `protocol_version` is frozen at 41 for JSON consumers of the retired integer
  scheme, never used to decide compatibility, and a reply carrying only it is
  refused ([architecture-protocol-versioning.md](architecture-protocol-versioning.md)).
  The `protocol_range` printed is the probing `kmuxd` binary's own, which is the
  running daemon's unless the binary on `$PATH` was replaced while it ran.
- The client tunnels to `tcp_port`, and a transport supervisor probes QUIC on
  `quic_port`; the endpoint list is diagnostic only. The subsequent `Auth` negotiates the range again, so a stale or
  forged reply cannot enable anything the daemon does not speak.

---

## ConnectionId and Session Resumption

`ConnectionId` is a server-assigned `u64` provided in the first `AuthResult`. It names a client's *registration* on the daemon — its `ClientId` (which its pane attachments and input locks are keyed by), label and identity — and a client presents it again whenever it opens a new channel: a transport swap, or a reconnect (issue #208).

**Daemon runs.** Every successful `AuthResult` also carries `daemon_instance` (`DaemonInstanceId`), drawn at random when a `kmuxd` process starts. Everything a run numbers — connection ids, client ids, pane seqnos — starts over in the next run, and a restarted daemon may reuse a pid, so "the same daemon" means the same instance id. A handoff successor is a new process, and so a new run.

**Resume.** A new channel's `Auth` carries the existing `connection_id` and the run that assigned it (`resume_instance`; the client keeps both, `SessionManager::resume_from`). After the identity proof, `ServerApp::register_client` resumes the registration only if all hold:

- `resume_instance` is this run's instance id (or absent, from a client that predates it) — the same number in another run names someone else's registration, if anyone's;
- the `connection_id` names a live registration — the daemon still holds the old channel (a transport swap, or a half-open link whose loop has not yet hit its pong deadline); and
- the verified `machine_id` equals the one the registration was made with. A different machine holding the token takes nothing over.

Otherwise the channel registers a fresh connection with fresh ids. `RegisteredClient::resume` (`app::Resume`) says which — `Resumed`, `OtherRun`, `UnknownConnection`, `OtherMachine`, or `NotRequested` for a first connection — and the daemon's `client authenticated` log line carries it as `resume=…` with the `generation`. An old channel whose loop has already ended has released its registration, so a reconnect after it is a fresh connection — which costs nothing on screen, because replay is per pane (below).

**Generations.** Each registration carries a generation, bumped by every resume. A channel's loop records the generation it registered with, and when it ends (`client_handler::release_channel`):

1. it detaches only the pane attachments made **through this channel** (`ServerApp::detach_channel`, matched by the channel's control lane) — a pane the resuming channel re-attached under the same `ClientId` is left alone; and
2. it releases the registration only if its generation is still current (`ServerApp::release_connection`). A superseded channel ending leaves the registration to the channel that resumed it.

**Replay.** Pane streams are not moved by the daemon: a pane stays attached through the channel that attached it, and a new channel re-attaches it. Replay is per pane, so it is the same for a resumed and a fresh connection: a client that re-attaches with `Attach { last_seqno }`, the last seqno it applied, is answered by `compute_replay` from the pane's retained diffs — exactly the missed `TerminalUpdate`s when the buffer still covers `last_seqno` (and the backlog is under the coalescing threshold), otherwise `SyncReset` + a fresh `TerminalSnapshot`. A `last_seqno` past the pane's current seqno was issued by another run and is answered with `SyncReset` too, never with an empty delta that would leave the old run's screen up.

**Transport swap.** When the supervisor promotes a new transport, the resumed channel sends `ChannelReady`, the daemon answers `ChannelSwitched { old_transport }`, and `apply_transport_upgrade` replaces the active sender; the old transport is dropped, and its loop's teardown is the superseded case above. A probe the daemon registered afresh (its `AuthResult` names another `connection_id`: the daemon restarted since) fails instead of swapping, since every pane stream is on the old channel.

**Missed server events.** Every connection forwards two server-wide broadcasts (PTY lifecycle events; VT, layout and tab events). A connection that falls behind one is sent an unsolicited `SessionListResult` with `request_id = RESYNC_REQUEST_ID` (`u64::MAX`) — every session with its tabs and layouts — and the forwarder carries on. Every session list — this resync and every answer to `SessionList` (`ServerApp::send_session_list`) — is queued while the session map is read-locked, so a local session created, closed or re-laid-out at the same moment either shows in it or has its own reply and event queued after it. The client treats any session list as the whole truth: sessions it no longer lists are closed as if their `SessionClosed` had arrived, and the viewed tab of a local session is reconciled against its listed layout. A federated session's view is left as shown: the hub's cached entry for it follows the peer's layout updates and re-lists the peer on a tab or session change, but only once that event has crossed the federation link, so it can trail what the peer's own events have already told the client. Federated sessions join every list under the federation membership gate, which each add or remove of a peer's session holds together with its own event, so a list never undoes a federated close either (see [architecture-federation.md](architecture-federation.md)).

---

## Transport override (issue #69)

Transport selection is normally **auto**: the `TransportSupervisor` periodically scores and hot-swaps to the best transport ([Transport supervision](#transport-supervision)). The user can pin it instead — useful when the heuristic picks badly for a given network.

- **Selection mechanism.** The `/transport [auto|quic|tcp-tls|uds|tcp]` command sets the override (`auto` clears it). Double-clicking the protocol indicator (the header transport button on GTK, the connection badge on macOS) opens the command pre-filled, so the choice is a pick from the completer.
- **Indicator.** While overridden, the protocol text renders in a distinct style — amber on GTK, an orange tint on macOS — so it's obvious the transport is pinned rather than auto-chosen.
- **Mechanism.** The choice lives on `SessionManager::transport_override` (the source of truth; it persists across reconnects). It is seeded into each freshly-spawned supervisor (`SupervisorParams::forced`) and pushed live over an `override_rx` channel so a runtime change takes effect at once. When pinned, the supervisor **skips the periodic heuristic entirely** and only ensures the forced transport is active (probing/swapping to it once if needed; a no-op if it is already active or has no advertised endpoint, e.g. UDS on a remote host).
- **Scope.** This is client-local: no message or capability is involved. It only affects connections that run a supervisor (the SSH/remote path); the GUI's local UDS link has no alternative transport to switch to.

---

## Liveness and Recovery

The data plane is kept honest by a bidirectional application-layer
ping/pong — **no ICMP, no TCP keepalives**. This matches the server-
announces principle: everything runs over the real protocol, so we
observe the same failure modes the user observes.

### Ping cadence

Both ends ping every `PING_INTERVAL` and reset their clock on *any* inbound
frame, not only a `Pong`: the daemon closes a connection `PONG_DEADLINE` after
an unanswered ping it wrote, and a client (or a federation hub) calls its link
dead after `SILENCE_TIMEOUT` of silence. The values and what each bounds are in
[protocol.md#timing](protocol.md#timing), single-sourced in
`kmux_protocol::timing`. A client samples `client_ping_due` once a second from
its event loop, so a hung daemon (kill -STOP, a black-holed path) is noticed
within about `SILENCE_TIMEOUT`.

### ConnectionState machine

The states and their transitions are specified, each with the test that pins
it, in [protocol.md#state-machines](protocol.md#state-machines); in outline:

```
      ┌─────┐
      │Idle │── first connect ──▶┌─────────────┐── bootstrap failed ──▶┌────────────────────┐
      └─────┘                    │ Handshaking │                       │ Disconnected{…}    │
                                 └─────────────┘                       │ (Mode::Disconnected│
                                        │ auth ok                      │  manual Reconnect) │
                                        ▼                              └────────────────────┘
                              ┌────────────────────┐                     │ Reconnect / Ctrl+Alt+R
                ┌────────────▶│Connected{transport}│◀── auth ok ─ Handshaking ◀┘
                │             └────────────────────┘
                │                       │ server closed / SILENCE_TIMEOUT /
                │ attempt               │ SSH tunnel died
                │ succeeds              ▼
                │             ┌────────────────────┐  attempt fails:
                └─────────────│Reconnecting{attempt}│◀─ attempt+1 after
                              └────────────────────┘   next_delay(attempt)
```

There is exactly one source of truth (`ConnectionState`); the header
badge (`badge_label`: `RECONNECTING #n`), the session manager's legacy
`connected: bool`, and the connection banner all read from it.

### Automatic reconnect (issue #208)

A link that drops once it is up is re-established on its own; nothing
asks the user to confirm. `FrontendDriver` (kmux-app, so GTK and Swift
share it) holds the policy, all of it pure and tested with injected time
in `driver/reconnect.rs`:

1. **Detection** — the server channel closed, the liveness timeout, or
   the SSH tunnel died — while the state is `Connected`. The mode (and so
   the UI) is left as it is; the state becomes `Reconnecting { attempt: 1 }`.
2. **Schedule** (`Reconnect`). Attempt *n* starts
   `kmux_client::backoff::next_delay(n − 1, seed)` after the drop or the
   previous failure: `BACKOFF_MIN` doubling to `BACKOFF_MAX`, less up to
   `BACKOFF_JITTER_CAP_PERMILLE` of jitter from a per-link seed
   ([protocol.md#timing](protocol.md#timing)). Each attempt is a `BootstrapPhase::Resume { attempt }`
   bootstrap, which re-presents the `connection_id` (with the run that
   assigned it) and shows no
   connecting overlay. A failure a retry may get past (the daemon is
   starting, restarting, or not answering) schedules the next attempt, for
   as long as it takes; a **refusal** — the daemon's protocol range or the
   token (`BootstrapTaskResult::Refused`) — ends the retries in
   `Mode::Disconnected`.
3. **On success** every visible pane is re-attached. If the link reached
   the **same daemon run** as before (the same `AuthResult.daemon_instance`;
   `SessionManager::link_reached_same_daemon` — a pid cannot tell, since a
   restarted daemon may reuse one),
   a pane in sync is re-attached with `Attach { last_seqno }` — the last
   seqno it applied — so the daemon replays exactly what it missed, or
   resets it with a snapshot past its retained diffs (see
   [ConnectionId and Session Resumption](#connectionid-and-session-resumption)).
   A new run (a restart, a handoff) numbers its panes' diffs from scratch,
   so every pane is then attached afresh. Then the input held during the
   outage is sent, in order.
4. **Input during the outage** (`OutageInput`). Keystrokes and pastes
   typed while reconnecting are held — up to `OUTAGE_INPUT_CAPACITY` (256)
   keystrokes, a paste counting as one — and flushed in order on
   reconnect. What does not fit is dropped and counted, and so is the whole
   buffer when the link reached another daemon run (its shells are not the
   ones typed at), when the oldest input is more than
   `OUTAGE_INPUT_MAX_AGE` (30 s) old, or when the retries end in a refusal.
   Raw input writes (mouse reports) are not held. UI commands issued
   meanwhile (a split, a new tab) are not held either.
5. **What the user sees** (`connection_banner`). A banner, rendered by
   GTK's `adw::Banner` and Swift's `ConnectionBanner` (FFI
   `KmuxDriver::connection_banner`): `Reconnecting… attempt n, next try in
   s s`, becoming `Daemon unreachable · retrying (attempt n)` after
   `UNREACHABLE_AFTER_ATTEMPTS` (5) failures, followed by the queued and
   dropped keystroke counts and the last attempt's error, with a
   **Reconnect now** button. For `DROPPED_NOTICE` (8 s) after the link is
   back it reports the keystrokes the outage dropped. The driver repaints
   whenever the banner changes (each countdown second, an attempt
   starting, the notice expiring), so a frontend that reads it on repaint
   stays current.

**Reconnect now** — the banner button, the header connection button, the
reconnect accelerator (Ctrl+Shift+R on GTK, ⌘R on macOS), the
`/reconnect` command — starts the waiting attempt at once. With no outage
under way it is the manual reconnect: a fresh bootstrap behind the
connecting overlay. (`Ctrl+Alt+R` is the same action in the kmux-app key
resolver.)

`Mode::Disconnected` remains for the cases no retry fixes by itself: the
first connect failed, the handshake was refused, a manual reconnect
failed, or the user cancelled or disconnected. Input to panes is frozen
there; the **Reconnect** button or accelerator reconnects. The `y`/Enter
confirmation prompt is gone.

A re-federated peer's `PeerOpened` after an automatic reconnect does not
re-run the first-connect session auto-select, so the session picker does
not pop over the session in use.

### "Server is down" case

The GUI's target is always its local daemon, and a local bootstrap
auto-starts it (`kmux_connect::daemon::ensure_daemon`), so an automatic attempt also
restarts a daemon that crashed. It restarts one stopped on purpose too: the
daemon has no way to tell its clients a stop is deliberate, so `kmux daemon
stop` while a GUI is open is followed by a fresh daemon within a second. To
stop for good, quit the GUI first (or `kmux client stop`). A remote host that is down is the
federation link's concern: see
[architecture-federation.md](architecture-federation.md).

### Tracing

Disconnect and reconnect events are emitted with structured fields:

```
WARN connection dropped; reconnecting connection_id=… transport=UDS reason="server closed connection"
INFO automatic reconnect attempt connection_id=… attempt=2
INFO reconnect requested connection_id=…   (a manual reconnect)
```

Filter with `RUST_LOG=kmux_client=debug,kmux=info` to watch the
lifecycle.

---

## Troubleshooting

All `SshError` variants render with multi-line context (argv, exit, stderr tail). In the GUI, a remote host's failure is the federation link's: `OpenPeer` answers `PeerError` with the reason, and a refused re-open names what to do ([architecture-federation.md](architecture-federation.md#re-discovering-a-restarted-peer)).

**Where a dev build logs:** debug builds (`cargo run`, `./kmux`) isolate their state under `kmux-debug/`, so they log to `~/.local/state/**kmux-debug**/client.log` — *not* the release `kmux/client.log`. A GUI error that seems "missing from the client log" is usually being written to the debug file while you watch the release one. Run `kmux debug paths` (with the binary in question) to print the exact resolved client/daemon log, runtime, and state paths plus the `kmuxd` an auto-spawn would launch; `mise run tail-client-log` / `tail-daemon-log` follow *both* profiles' logs at once.

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| `protocol version mismatch: client=X, daemon=Y` (with an upgrade hint) | the two builds' protocol ranges do not overlap | Update the older side until they overlap |
| `kmuxd not found on remote host` (exit 127) | `kmuxd` not in `$PATH` on remote | Install `kmuxd` on the remote host |
| `SSH connection failed` with stderr `Permission denied (publickey)` | Key not available to ssh-agent / wrong identity | Add the key to `ssh-agent` or configure `IdentityFile` in `~/.ssh/config` |
| `SSH connection failed` with stderr `Host key verification failed` | Remote host key changed | Update `~/.ssh/known_hosts` (e.g. `ssh-keygen -R host`) |
| `SSH connection failed` with stderr `Connection timed out` | Host unreachable / firewall | Check network; verify the host is accessible |
| `SSH tunnel exited before becoming ready` with `Permission denied` | Same as auth above, but the probe succeeded and tunnel auth failed (e.g. agent expired) | Re-add identity to ssh-agent |
| `SSH tunnel never accepted a local connection` | sshd-side restriction (`AllowTcpForwarding no`, `PermitOpen` mismatch) | Adjust sshd config on remote |
| `remote daemon returned malformed JSON` | Old kmuxd or kmuxd printed to stdout | Update kmuxd; check `~/.local/state/kmux/kmuxd.log` on remote |
| QUIC connection refused, TLS-TCP works | UDP blocked by firewall | Normal; `TransportSupervisor` will stick with TLS-TCP |
| TLS fingerprint mismatch | Server cert rotated | Delete the stale entry from `~/.config/kmux/known_hosts.toml` |
| All transports fail | No network path | Check firewall; verify `kmuxd` is listening on correct ports |
| Sessions lost after restart | Checkpoint failed | Check disk space; `$XDG_STATE_HOME/kmux/sessions/state.bin` (`kmux debug paths` prints it) |
| (dev) GUI shows "daemon start failed" / never connects, nothing in `kmux/client.log` | Debug build logs + spawns under `kmux-debug/`; resolution must reach `target/debug/kmuxd`, not a release `kmuxd` on `$PATH` | Use `./kmux` (it builds + pins `KMUX_KMUXD=target/debug/kmuxd`); inspect with `kmux debug paths` + `mise run tail-client-log` |

### Dry-run diagnostics (`--dry-run`, `--test`)

When a connection misbehaves, re-run with one of these flags to print a
step-by-step trace of the bootstrap on stdout and exit. Both flags run the
*same* [`run_bootstrap`](../crates/kmux-connect/src/pipeline.rs) code path
that the GUI uses — a successful `--dry-run` therefore proves the real
flow works, and a failure shows exactly which step failed.

| Flag | Behavior |
|------|----------|
| `--dry-run` / `-n` | Runs the bootstrap, sends one `Ping`, expects `Pong` within 5 s, prints `[RESULT]`, exits. |
| `--test` | Superset of `--dry-run`: additionally runs the live `TransportSupervisor` for 10 s so transport scoring and any hot-swap upgrade are observable. |
| `--dry-run --test` | Prints `warning: --test implies --dry-run` on stderr and behaves as `--test`. |

Sample output for `kmux --dry-run` against a local daemon:

```
kmux dry-run for local-daemon
[PARSE     ] target=local-daemon (0.00s)
[DAEMON    ] querying control socket /run/user/1000/kmux/daemon.sock (0.00s)
[DAEMON    ] already running pid=12345 quic_port=43117 tcp_port=38809 (0.00s)
[HANDSHAKE ] UDS /run/user/1000/kmux/daemon-data.sock (0.01s)
[AUTH     ] Auth sent (protocol=1.0.0..=1.1.0, conn_id=None) (0.02s)
[AUTH     ] AuthResult success=true conn_id=42 server_version=0.1.0 (0.03s)
[PING     ] seq=0; waiting up to 5s (0.03s)
[PING     ] OK - RTT 0.42 ms (0.03s)
[RESULT   ] connected via UDS; bootstrap 32 ms, ping 0.42 ms (0.03s)
```

The raw `kmuxd probe-or-start` JSON is printed verbatim for SSH targets,
with `"token":"..."` automatically redacted to `"token":"***"`. New
observer events can be added at `pipeline::BootstrapEvent` without
breaking existing consumers — the enum is `#[non_exhaustive]`.

---

## Per-Connection Metrics

Every connection that completes auth is given an `Arc<ConnectionMetrics>` allocated in `run_client_session` before the first frame arrives. Counters are `AtomicU64` and updated on the hot path — no locks, no allocation after creation.

| Field | Type | Description |
|-------|------|-------------|
| `bytes_in` | `AtomicU64` | Total bytes read from the client (frame header included). |
| `bytes_out` | `AtomicU64` | Total bytes written to the client on the wire (frame header included; **post-compression**). |
| `bytes_out_uncompressed` | `AtomicU64` | What `bytes_out` would have been with no compression. `bytes_out / bytes_out_uncompressed` is the realised compression ratio. |
| `msgs_in` | `AtomicU64` | Number of complete frames received from the client. |
| `msgs_out` | `AtomicU64` | Number of complete frames sent to the client. |
| `last_activity_ms` | `AtomicU64` | Epoch-ms timestamp of the last received frame; `0` means no frame yet. |
| `last_pong_ms` | `AtomicU64` | Epoch-ms timestamp of the last `Pong` received from this client; `0` = never. |
| `last_rtt_ms` | `AtomicU64` | Most recent round-trip time in ms; `u64::MAX` = no measurement yet. |
| `last_ping_sent` | `Mutex<Option<(u64, Instant)>>` | The `(seq, sent_at)` of the last `Ping` sent, used to match the next `Pong`. |

**Metric continuity across channel switches.** When a client reconnects on a new transport (e.g. the `TransportSupervisor` upgrades UDS → QUIC), `ServerApp::register_client` is called with the existing `ConnectionId`. The daemon finds the existing `ConnectionState`, updates its `transport` label, and keeps the *original* `Arc<ConnectionMetrics>` in the registration, so the listing keeps the counts from before the switch. The new channel counts into its own `Arc`, which the caller passed and the registration does not store: after a switch the listed counters stop advancing (a known gap).

**Where counters accumulate.** `bytes_in` and `msgs_in` are incremented in the `run_client_session` read loop immediately after each `read_frame` returns. `bytes_out` (actual wire bytes), `bytes_out_uncompressed`, and `msgs_out` are incremented in the writer task after each `write_frame_compressed` succeeds (it returns the wire-byte count). `last_activity_ms` is stamped to `epoch_millis()` on every inbound frame. `last_rtt_ms` and `last_pong_ms` are updated in the Pong dispatch branch when the received sequence number matches `last_ping_sent`.

### `kmux daemon sessions`

```
kmux daemon sessions [--all] [--format <table|json>]
```

Queries the daemon's Unix control socket (`{"command":"sessions"}`) and renders a live view of all sessions with their attached connections:

```
SESSION   ID      CONN  TRANSPORT  UPTIME    LAST PING  RTT      IN        OUT
work      eagle   #5    QUIC       12m 3s    2s         1.8ms    1.2 MiB   45.6 MiB
work      eagle   #9    UDS        4m 10s    1s         0.2ms    210 KiB   8.1 MiB
research  hippo   -     -          -         -          -        -         -
```

- Without `--all`, only sessions with at least one active connection are shown. Auth'd-but-not-yet-attached connections appear in a separate `(unattached)` bucket at the bottom.
- `--all` also shows sessions with zero attached connections (connection columns show `-`).
- `--format json` prints the full `SessionsResponse` struct as pretty-printed JSON.

Each row in the table is one *connection* (not one session). A session with multiple attached clients (e.g. the same pane viewed from two terminals) produces multiple rows with the same `SESSION`/`ID` values.

**Implementation.** `ServerApp::snapshot_sessions_with_connections` holds both the sessions and connections read locks simultaneously to avoid tearing, then joins them: it builds a `ClientId → ConnectionInfo` map from the connections table, iterates session panes to collect attached `ClientId`s, and groups `ConnectionInfo`s per session. Any `ClientId` not found in any pane appears in `unattached`.

### Connection inspector (GUI, issue #60)

The desktop clients expose the *client side* of the same picture as an overlay — the sibling of the metrics inspector. Toggle it from the command palette (`/connection`, alias `/conn`), the menu (**Connection**), or the accelerator (`Ctrl+Shift+I` on GTK, `⌘⇧I` on macOS).

The body is rendered from a single toolkit-neutral snapshot, `kmux_app::core::ConnectionInfo`, built by `AppCore::connection_info()` from the live `SessionManager`. `kmux-gtk` renders it directly (`dialogs::connection_content`); `kmux-swift` maps it to `FfiConnectionDetails` across the `kmux-ffi` boundary (`KmuxDriver::connection_details`) and renders `ConnectionView`. The snapshot carries:

- **Server / endpoint** — the user-facing target (`localhost` or `user@host`) plus the resolved data-plane `host:port`, and (remote only) whether TLS certs are verified.
- **State / transport** — the `ConnectionState` badge (`CONNECTED · QUIC`…) and the active transport channel.
- **Identity** — the server-assigned `connection_id` and `client_id`, the daemon's binary version, and the negotiated wire protocol version.
- **Latency** — the active transport's RTT summary (EWMA + recent avg/max + sample count), sourced from the client's `RttTracker` (the same Ping/Pong measurements that feed the transport scorer).
- **Traffic** — per-transport byte/message totals from the client metrics (`NetworkMetrics::snapshot_by_transport`).

Because it reads only already-collected client state, the inspector adds no
protocol traffic or capability; it does bump `KMUX_FFI_ABI_VERSION` (new FFI
records + getters).

---

## Daemon Binary Resolution

Every place that spawns `kmuxd` uses one of two strategies. They are intentionally different because the caller's context differs.

### Client-side (`kmux`, `kmux daemon start/restart`, auto-spawn)

`find_server_binary()` in `crates/kmux-connect/src/daemon/lifecycle.rs`, in precedence order:

1. **`KMUX_KMUXD` env override** — an explicit path to a `kmuxd` binary (honored only when it points at a real file; a stale value falls through). Mirrors `KMUX_BIN` / `KMUX_APP`.
2. **Sibling of the running executable** — `current_exe().parent() / "kmuxd"`. The primary path in all installed layouts (the GUI exe and `kmuxd` ship side by side).
3. **Debug builds only — `target/<profile>/kmuxd`.** A path baked at build time from the crate's `OUT_DIR` (see `crates/kmux-connect/build.rs`). This makes a debug client prefer the matching debug daemon over any installed **release** `kmuxd` on `$PATH`, which it could never talk to (the two profiles use [separate runtime dirs](#runtime-directory-isolation), so a release daemon's socket never appears where the debug client polls). It also covers the macOS Swift dev app, whose `current_exe()` lives in `kmux-swift/.build/` with no `kmuxd` sibling.
4. **`$PATH` walk** — iterates `PATH` components looking for `kmuxd`. Fallback for unusual install layouts.
5. Error if none resolve (the message suggests setting `KMUX_KMUXD`).

| Build mode | `kmuxd` resolved from |
|------------|----------------------|
| Debug, `./kmux` | `KMUX_KMUXD=target/debug/kmuxd` (set + built by the task) |
| Debug, bare `cargo run -p kmux-gtk` / `swift run` | sibling `target/debug/kmuxd`, else the build-time `target/debug/kmuxd` |
| Prod install | sibling of the GUI exe (bundled beside it; also on `$PATH`) |
| Custom layout | `KMUX_KMUXD`, then sibling, then `$PATH` |

> Why the debug-profile preference matters: before it, a debug GUI with no `kmuxd` sibling fell through to `$PATH` and auto-spawned an installed **release** `~/.cargo/bin/kmuxd`. That daemon writes its socket under `kmux/` while the debug client polls `kmux-debug/`, so the client never sees it and times out — i.e. "the dev build doesn't start the daemon". Prod is unaffected (`mise run install` bundles a matching release `kmuxd` beside the GUI).

Once the binary is located, `start_daemon_in()` spawns it with the canonical argv:

```
kmuxd --daemon --bind 0.0.0.0 --port 0
```

This constant is defined in `crates/kmux-protocol/src/control_rpc.rs::DAEMON_BOOT_ARGS` and is the single source of truth for every spawn site.

### Server-side (`kmuxd probe-or-start`)

When the SSH bootstrap invokes `kmuxd probe-or-start` on a remote host and the daemon is not running, `cleanup_and_start_daemon()` in `crates/kmuxd/src/main.rs` re-execs the same binary that SSH found:

```rust
std::env::current_exe()   // the running kmuxd binary
```

This means `probe-or-start` always restarts the exact binary that was already on the remote `$PATH`. No additional lookup is performed. It uses the same `DAEMON_BOOT_ARGS` argv.

The remote `kmuxd` must be on the login `$PATH`. Non-interactive SSH sessions typically receive only `/usr/bin:/bin` (plus whatever `/etc/ssh/sshrc` or PAM adds). If `kmuxd` is installed at `~/.local/bin/kmuxd`, ensure the remote `/etc/environment` or `~/.profile` exports that path.

### Dev entrypoint (`./kmux daemon <args>`)

`./kmux daemon <args>` rebuilds kmux + kmuxd (debug) and delegates to `kmux daemon <args>` via `cargo run -p kmux` (a CLI subcommand never loads a UI toolkit), which uses `find_server_binary()` as above (e.g. `./kmux daemon start` / `./kmux daemon restart` / `./kmux daemon stop`). The debug build's `target/debug/kmux` picks up `target/debug/kmuxd` as its sibling, and `mise run dev` also pins `KMUX_KMUXD=target/debug/kmuxd`. No separate binary resolution logic exists in the entrypoint itself.

### Runtime directory isolation

Debug builds use `$XDG_RUNTIME_DIR/kmux-debug/` for all runtime files (control socket, data socket, PID, token). Release builds use `$XDG_RUNTIME_DIR/kmux/`. This prevents a debug daemon from conflicting with a simultaneously running release daemon on the same machine.

---

## Idle Shutdown

Idle shutdown is **off by default** (`idle_shutdown_secs = 0`, issue #207).
`kmuxd` is meant to run for months: a laptop sleep or a network drop that
disconnects every client must not end it, and ending it ends every shell it
hosts. An operator who wants a daemon that exits once nobody is attached opts in
in `kmuxd.toml`:

```toml
[daemon]
# Exit after this many seconds with no client connected. 0 (the default) disables it.
idle_shutdown_secs = 300
```

A `kmuxd.toml` written by an older daemon's first run pins `idle_shutdown_secs = 30`
(the template serialized every default). Such files are not migrated; instead
the daemon logs a warning at startup whenever idle shutdown is on. Delete the
line, or set it to `0`, to get the new behaviour. Templates written from now on
show every default commented out, so they pin nothing.

### Mechanism

`ServerApp` maintains a `tokio::sync::watch` channel that broadcasts the live connection count. When `idle_shutdown_secs > 0`, `startup.rs` spawns an idle-watcher task that:

1. Waits for the count to change.
2. When the count drops to 0, starts a debounce timer of `idle_shutdown_secs`.
3. If a client connects before the timer fires, the timer is cancelled and the loop restarts.
4. If the timer fires, the watcher signals the shared `shutdown` `Notify` — the same one used by SIGINT, SIGTERM, and the `stop` control command. The daemon exits cleanly (session-state checkpoint included).

### Debounce rationale

A client reconnecting after a drop, or switching transports, is disconnected for a moment before it is back. Keep the window well above that gap — tens of seconds at least — so a reconnect never races the timer.

### What survives an idle shutdown

An idle shutdown is an ordinary daemon exit, so the **processes do not survive
it**: every PTY master closes when the daemon exits, so each shell gets `SIGHUP`
and its jobs go with it, exactly as on `kmux daemon stop`. What survives is the
**checkpoint**: sessions are written to `$XDG_STATE_HOME/kmux/sessions/state.bin`
(or `$HOME/.local/state/kmux/sessions/`) on the way out, the same file the periodic
checkpoint keeps current. The next daemon reads it and **respawns** each pane —
a fresh shell in the same working directory, seeded with the old screen and
scrollback behind a "[kmux: session restored]" separator. Only a graceful
restart (`kmux daemon restart`, [daemon-handoff.md](daemon-handoff.md)) keeps
the running processes.

---

## Server-Side Flow Control and Deadlines

One slow pane, slow client or misbehaving peer must not stall the daemon or
grow its memory without bound (issue #206). Every queue a client or a pane's
program can fill — a connection's outbound queue, a pane's input queue and its
terminal-query replies — is bounded, and every wait on a peer has a deadline.
Two internal channels are not yet bounded: the daemon's request channel to a
`kmux-vt-worker` and a federation link's upstream channels; both are fed at the
rate the daemon itself produces, not by a peer.

### Outbound queue (`crates/kmuxd/src/outbound.rs`)

Everything the daemon sends a TCP/UDS client — replies, events, pings, and the
pane data `TcpAttacher` forwards — goes through one bounded FIFO
(`OUTBOUND_CAPACITY`, 1024 messages) that the connection's writer task drains.
Two lanes share it, so order is kept:

- **Control** (`OutboundTx::send`) is never dropped and may use the whole
  queue. If even that is full, the client has stopped reading: the connection
  is closed (`CloseReason::OutboundOverflow`) instead of a message being lost,
  and the client reconnects and resyncs.
- **Pane data** (`OutboundTx::try_send_data`) may not use the last quarter of
  the queue, which stays free for control. When a frame does not fit, that
  pane's stream is **lagged**: its frames are dropped (its per-pane channel is
  kept drained, so the relay never marks the client `Lagged` itself) until the
  writer has drained the queue to half. Then the stream sends `SyncReset` +
  `TerminalSnapshot` — the shape of `AttachResult::SyncReset` — with a fresh
  snapshot and its seqno, skips incremental frames the snapshot already covers,
  and goes live. The client ends up with a correct grid without a round trip.
  A pane the daemon cannot snapshot (a federated one) is sent `Lagged`
  instead, and the client re-attaches.

Server-wide VT events a flood of output can raise one per escape sequence —
`PaneBell`, `PaneTitleChanged`, `PaneProgressChanged` — reach every connection,
attached to the pane or not, so they ride the pane-data lane and are dropped
when it is congested; on the control lane a `cat` of a binary file would fill a
slow client's queue and close it. Layout, tab-lifecycle and clipboard events
stay control. Events a federation link relays from a remote daemon take the
same lanes (`forward_vt_event`).

A log dump (`FetchLogs`) can exceed the queue, so it is sent from its own task
that waits for room — and, like pane data, leaves the control reserve free, so
a dump never starves replies or pings.

A resync snapshot is labelled with the seqno read *before* it is taken: a diff
emitted in between is then forwarded again after the snapshot (re-applying its
absolute cell writes in order ends at the same grid) rather than labelled as
covered and lost. QUIC pane streams are unaffected: each rides its own
flow-controlled unidirectional stream. No wire message or capability was added,
so `PROTOCOL_RANGE` is unchanged.

### Deadlines

The values are single-sourced in `kmux_protocol::timing` and specified in
[protocol.md#timing](protocol.md#timing).

| What | Deadline | On expiry |
|------|----------|-----------|
| TLS / QUIC handshake | `TRANSPORT_HANDSHAKE_TIMEOUT` 10 s, in the connection's own task; at most 64 at once per listener and 8 per source, the rest refused at once | connection dropped; other accepts unaffected |
| `Auth` + `AuthProof` after connect | `AUTH_DEADLINE` 30 s | `CloseReason::AuthDeadline` |
| Any inbound frame after the oldest unanswered `Ping` is written | `PONG_DEADLINE` 30 s | `CloseReason::PongDeadline` |
| Each frame write, each flush | `FRAME_WRITE_TIMEOUT` 30 s | `CloseReason::WriteTimeout` |
| Each frame write and each flush on a QUIC pane stream | `PANE_STREAM_STALL_TIMEOUT` 330 s | that stream is reset (application code 1) and the pane resynced; the connection stays up |

Answering `Ping` with `Pong` has always been part of the protocol, and every
long-lived peer does (`kmux-client`, federation links); what is new is that the
daemon enforces it, so no message or capability changed. The one peer that did
not answer, `kmux daemon logs -f --server`, answers from this change on; one
from an older build is closed after about 35 s of a quiet follow (accepted
skew: the command is re-run).

A listener whose `accept` fails (out of file descriptors) pauses 100 ms before
the next attempt instead of spinning.

A QUIC pane stream (`pane_uni_writer` in `crates/kmuxd/src/connection.rs`)
has a write timeout too (issue #207). A client that stops reading one pane's
stream fills that stream's flow-control window, and the write used to wait
forever, pinning the pane's writer task and its queue. A stall now costs that
stream alone: it is reset (application code 1), its task ends and the pane's
queue closes with it, and the client is sent `Lagged` for the pane on the
control stream. When it reads that, it re-attaches the pane and gets the usual
`SyncReset` + snapshot on a fresh stream. The connection and every other pane
carry on.

The stall timeout, `PANE_STREAM_STALL_TIMEOUT`, is 330 s: 30 s past the QUIC
idle timeout (300 s, with keep-alives every 15 s). A stream also stops taking
frames when nothing reaches the client at all, a laptop asleep or a network
gone, and that is the connection's business: it rides out any outage shorter
than its idle timeout and closes after one longer, which fails every stream's
write at once. With the stall timeout past the idle timeout, an outage never
resets a pane stream the connection would have kept, so sleep or a dropped
network costs no pane a resync. What remains is a client that answers
keep-alives but has not read this stream for five and a half minutes. The
`FIN` that ends a stream is not timed: quinn queues it and returns at once.

The client reads every pane stream as soon as it arrives, each in its own
task (`kmux_connect::connect::accept_pane_streams`), and grants the server
`MAX_PANE_STREAMS` (4096) concurrent uni streams (issue #208). It used to
cap its readers at 64 and leave QUIC's default credit of 100, so with more
than 64 attached panes the extras went unread: an idle one showed nothing,
a busy one filled its window and was reset and re-attached every 330 s,
and past 100 the daemon's `open_uni` waited for credit with no timeout.

The pong clock starts when the writer puts the `Ping` on the wire, not when it
is queued: a ping waiting behind a log dump or pane data on a slow link has not
reached the client, and a peer that stops reading is caught by the write
timeout instead.

The auth and pong checks are pure functions of instants (`auth_verdict`,
`pong_verdict` in `crates/kmuxd/src/client_handler/liveness.rs`), evaluated
once a second by a per-connection watchdog. The watchdog, the writer and the
outbound queue all close a connection the same way — through its close signal,
which the read loop selects on — and the reason is logged.

### Per-pane input

Client input is queued, not awaited: see
[daemon-lifecycle.md §9.3a](daemon-lifecycle.md#93a-client-input-appiors-engine).
A pane whose program stops reading fills its own bounded input queue and
refuses further input with an error; no other pane or session waits on it.
The replies the emulator generates for the program's terminal queries (DSR,
DA, …) queue on a bounded channel too (`PTY_RESPONSE_CAPACITY`, 64): a program
that floods queries without reading its input has further replies dropped.

---

## Key File Index

| File | Role |
|------|------|
| `crates/kmux-protocol/src/codec.rs` | Frames: `read_frame` / `write_frame*`, codec tags, size limits |
| `crates/kmux-protocol/src/timing.rs` | Every protocol interval and deadline |
| `crates/kmux-protocol/src/control_rpc.rs` | Control-socket RPC types (`StatusResponse`, `SessionsResponse`, …) and `DAEMON_BOOT_ARGS` |
| `crates/kmux-protocol/src/endpoint.rs` | `Endpoint` URL parser |
| `crates/kmux-sys/src/transport/mod.rs` | `Listener` trait, `PendingSession`, `IncomingSession`, `EndpointAdvert`, `serve` accept loop |
| `crates/kmux-sys/src/transport/admission.rs` | Handshake admission bounds |
| `crates/kmux-sys/src/transport/{quic,tcp_tls,uds}.rs` | The three listeners |
| `crates/kmux-sys/src/tls/tofu.rs` | TOFU certificate pinning |
| `crates/kmux-connect/src/pipeline.rs` | `run_bootstrap`, `ResolvedTarget`, `BootstrapOutcome`, `BootstrapError` |
| `crates/kmux-connect/src/{connect,tcp_connect}.rs` | Client connects: QUIC; UDS and TCP+TLS; the `Auth` frame |
| `crates/kmux-connect/src/supervisor.rs` | `TransportSupervisor`, `TransportScorer`, `UpgradeSignal`, `probe_verdict` |
| `crates/kmux-connect/src/ssh/negotiate.rs` | SSH `probe-or-start` and tunnel setup |
| `crates/kmux-connect/src/daemon/` | Control-socket client and the local daemon's lifecycle (`ensure_daemon`, `find_server_binary`) |
| `crates/kmux-client/src/session_manager/connection.rs` | `connect`, `apply_outcome`, `resume_from`, `link_reached_same_daemon`, `apply_transport_upgrade` |
| `crates/kmux-client/src/liveness.rs` | Client ping and silence tracking |
| `crates/kmux-app/src/driver/reconnect.rs` | Automatic reconnect policy, outage input, the banner |
| `crates/kmuxd/src/config.rs` | `kmuxd.toml` schema and `ServerConfig::resolve()` |
| `crates/kmuxd/src/announce.rs` | Audience-aware endpoint advertisement |
| `crates/kmuxd/src/startup.rs` | Listener loop over `ServerConfig.listeners` |
| `crates/kmuxd/src/app/mod.rs` | `ServerApp`, `register_client` (resume), `ConnectionMetrics`, `snapshot_sessions_with_connections` |
| `crates/kmuxd/src/daemon.rs` | Control socket server (`serve_control_socket`, status/stop/sessions commands) |
| `crates/kmuxd/src/client_handler/session.rs` | `run_client_session` (generic over all transports; byte/activity instrumentation; writer with `FRAME_WRITE_TIMEOUT`) |
| `crates/kmuxd/src/client_handler/dispatch/auth.rs` | The handshake |
| `crates/kmuxd/src/client_handler/liveness.rs` | Auth and pong deadlines, the per-connection watchdog |
| `crates/kmuxd/src/outbound.rs` | Bounded outbound queue, control/data lanes, lagged pane-stream resync, close signal |
| `crates/kmux-app/src/subcommands/render.rs` | Centralised `tabled`-based table/JSON rendering for all CLI list output |
