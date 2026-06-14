# Daemon federation (issue #121)

Status: **foundation landed; kmuxd integration designed, not yet implemented.**

## Goal

Today every GUI window opens its own network connection to `kmuxd`, which may be
remote. N windows on a remote host = N TLS/QUIC/SSH connections. Federation makes
the **local `kmuxd` the single per-user hub**: it hosts local PTY sessions *and*
opens **one upstream connection per distinct remote `kmuxd`**, proxying that peer's
sessions to local GUIs. GUIs only ever speak the local Unix socket and shed the
network stack; the remote connection persists across GUI restarts.

See `docs/architecture-frontend.md` for the client layering this builds on.

## What has landed

- **`kmux-connect` crate** — the connect/negotiate mechanism (bootstrap strategies,
  transports, `TransportSupervisor`, TOFU, daemon lifecycle) extracted from
  `kmux-client` so `kmuxd` can reuse it for **outbound peer links**. `kmux-client`
  re-exports it; no consumer changed.
- **`CellGrid::to_snapshot()`** (`crates/kmux-client/src/grid/mod.rs`) — inverse of
  `apply_snapshot`; lets a cached grid mirror be re-serialised into a `GridSnapshot`
  for a newly-attaching GUI with no upstream round-trip. Round-trip tested.
- **Federation wire protocol, `PROTOCOL_VERSION = 25`** (`crates/kmux-protocol`):
  - `ClientMessage::OpenPeer { request_id, target: PeerTarget }` / `ClosePeer { request_id, peer }`
  - `ServerMessage::PeerOpened` / `PeerClosed` / `PeerError`
  - `PeerTarget { user, host, ssh_port, accept_invalid_certs }` with `peer_id()` → `"user@host[:port]"`
  - `kmuxd` dispatch + `kmux-client` handler carry **graceful stub arms** today
    (OpenPeer → `PeerError "not supported yet"`); the real logic replaces them.

## kmuxd integration design

`kmuxd`'s per-pane `PaneRelay` already does everything a proxy's *downstream* needs:
`clients: HashMap<ClientId, ClientSender>`, `effective_size()` (min across clients),
`broadcast_to_clients()` fan-out, per-client `paused`/`force_full_snapshot`/`capabilities`,
and `InputMode::Locked(ClientId)` (`crates/kmuxd/src/app/mod.rs`, `relay.rs`,
`app/attach.rs`, `app/io.rs`). The seam is the pane's **source**: local panes read a
PTY → ghostty `TermState` → diff → `broadcast_to_clients()`; a *peer-backed* pane's
source is an upstream `ServerMessage` stream.

**Recommended approach — a `PeerManager` owned by `ServerApp`, reusing `kmux-client`
for the upstream**, with dispatch branching federated vs. local:

1. **`PeerManager` (new, `crates/kmuxd/src/federation/`)**, `Arc` on `ServerApp`
   (mirrors the existing `Arc<ServerApp>` sharing). Keyed by `PeerId`. Each
   `PeerConnection` owns:
   - an upstream link from `kmux_connect::pipeline::run_bootstrap` (the
     `client_tx: ClientMessage` sink + `srv_rx: ServerMessage` source),
   - the federated session registry: `local WordId ↔ (peer, remote WordId)`,
   - per-pane `CellGrid` mirrors (apply upstream snapshot/diff; `to_snapshot()` for
     late local attaches),
   - the downstream fan-out: which local `ClientId`s view each federated pane.
2. **`ServerApp::open_peer(target)`** (replaces the dispatch stub): bootstrap upstream,
   `SessionList` the remote, register its sessions locally with **freshly-drawn local
   `WordId`s** (reuse the `WordlistSampler`), reply `PeerOpened`. Behind a
   `federation` cargo feature until PR5.
3. **Dispatch branching** (`client_handler/dispatch.rs`, injection points identified):
   `Attach` / `PtyInput` / `PtyKey*` / `Resize` / `RequestInputLock` / `Detach` for a
   federated pane route to `PeerManager` (translate local→remote id, forward upstream)
   instead of `app.*`. Track per-connection federated attachments in `SharedClientState`.
4. **Upstream feed loop**: per `PeerConnection`, drain `srv_rx`; for each
   `TerminalSnapshot/Update/CursorUpdate/ScrollbackAppend` translate remote→local
   `pane_id` and push to the local viewers' `data_tx` (the same channel `attach()`
   wires); fan `Event`/`LayoutUpdate`/`SessionListResult` to all viewers.
5. **`list_sessions` merge**: append `PeerManager`'s federated `SessionEntry`s
   (local `WordId`, `name` decorated with the peer, e.g. `eagle @ box`).
6. **Persistence**: exclude federated sessions (`crates/kmuxd/src/persist/`) — they
   live on the remote and re-appear on reconnect; persisting them creates ghost panes.

### ID namespacing

`WordId`/`PaneId` are `String`. Federated sessions get **locally-assigned** `WordId`s
(no collision with local or other peers), with the daemon holding the bidirectional
map. The GUI sees only local ids and needs no federation awareness beyond issuing
`OpenPeer`; the peer origin is conveyed through the decorated session `name`.

## PR breakdown (remaining)

- **PR3** — `PeerManager` + `open_peer` (upstream connect + remote `SessionList` +
  local registration) + dispatch branching + upstream feed loop, behind `federation`.
  End-to-end: one GUI attaches to one remote session through the local daemon.
- **PR4** — multi-GUI reconciliation: upstream pane size = `effective_size` (min over
  local viewers); pause upstream only when **all** local viewers paused; capability
  union upstream / filter downstream; input-lock arbitration across local viewers;
  per-viewer snapshot minting via `to_snapshot()`.
- **PR5** — lean the GUI: always UDS-local to `kmuxd`; the parsed `--server` target
  becomes an `OpenPeer`; feature-gate `quinn`/`rustls`/`ssh` **out** of the GUI build
  (they move to `kmuxd`'s peer role). CI check the GUI no longer links the net stack.
- **PR6** — hardening: upstream reconnect/backoff (reuse `kmux-connect` `recovery`),
  idle peer drop, peer-down isolation from local panes, version/profile guards on peer
  links, namespacing edge cases.

## Open decision — federation addressing & testability

`PeerTarget` is **SSH-only** today (`kmuxd probe-or-start` over SSH, then TCP+TLS over
the tunnel), matching the existing remote-connection model (`kmux-connect`'s only
remote handshake entry is `prepare_ssh`). This makes **end-to-end CI tests hard**: the
existing `crates/kmuxd/tests/handoff_e2e.rs` spawns a daemon and connects over UDS/TCP
with no SSH, and two loopback `kmuxd`s can't easily SSH to each other.

Two ways forward (needs a maintainer call before PR3 implementation):

- **(A) Direct endpoint in `PeerTarget`** — add a `direct: Option<{host, port, token}>`
  variant so a peer can be reached over TCP+TLS without SSH (LAN + tests). Lets the
  E2E test spawn two loopback daemons and federate over TCP. Small additive protocol
  change (another `PROTOCOL_VERSION` bump). **Recommended** — unblocks rigorous testing
  and is independently useful (LAN federation without SSH).
- **(B) SSH-only + an `sshd`-on-loopback test harness** — keep addressing as-is and
  stand up a throwaway `sshd` in the integration test. Heavier CI dependency; closer to
  the real path.

Until this is decided, PR3 cannot be implemented with the end-to-end test coverage the
project expects (`mise run test` is a pre-push gate). The recommendation is **(A)**.
