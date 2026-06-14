# Daemon federation (issue #121)

Status: **PR3 + PR4 core landed — multiple GUIs share one proxied pane over a
single upstream link, with smallest-wins sizing and zero-round-trip late attach.
GUI lean-down (PR5), federation hardening (PR6), and the remaining reconciliation
facets (pause-union, capability merge, input-lock, session-event forwarding)
remain.**

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
- **Federation wire protocol, `PROTOCOL_VERSION = 26`** (`crates/kmux-protocol`):
  - `ClientMessage::OpenPeer { request_id, target: PeerTarget }` / `ClosePeer { request_id, peer }`
  - `ServerMessage::PeerOpened` / `PeerClosed` / `PeerError`
  - `PeerTarget::{Ssh { user, host, ssh_port, accept_invalid_certs }, Direct { host, port,
    token, accept_invalid_certs }}` with `peer_id()` → `"user@host[:port]"` / `"host:port"`.
- **`kmuxd` federation subsystem (PR3)** — `crates/kmuxd/src/federation/` behind the
  default-on `federation` cargo feature:
  - `PeerManager` on `ServerApp` keyed by `PeerId`; `open_peer` connects upstream via
    `kmux_connect::tcp_connect::connect_tcp_tls` (the `Direct` endpoint), authenticates,
    fetches the remote `SessionList`, and registers each remote session under a
    **freshly-drawn local `WordId`** (from the same `WordlistSampler` as local sessions,
    so no collisions), holding the bidirectional `remote_word ↔ local_word` map.
  - **Dispatch branching** (`client_handler/dispatch.rs`) routes `Attach` / `PtyInput` /
    `PtyKey*` / `PtyPaste` / `Resize` / `Detach` for a federated pane to the peer (ID
    translated, forwarded upstream) instead of the local relay; `SessionList` merges the
    proxied sessions (peer-decorated names). The dispatch layer carries **no `#[cfg]`** —
    every branch goes through always-compiled `ServerApp` wrappers (`app/peer_api.rs`).
  - A per-peer **feed loop** drains the upstream `ServerMessage` stream, rewrites each
    frame's pane ID remote→local, and fans pane content out to that pane's local viewers;
    it answers upstream `Ping`s with `Pong`.
  - Proxied panes are kept **entirely out** of `ServerApp.sessions` (which is strictly
    PTY-backed), so no fake `PaneRelay`/`PtyWriter`/`term_state` ever exists.
  - Verified by `crates/kmuxd/tests/federation_e2e.rs`: two real loopback daemons
    federate over `Direct` TCP+TLS; a mock GUI attaches to the remote session through the
    local daemon and both directions flow (remote output reaches the GUI under a local
    pane ID; GUI input runs on the remote PTY).

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

## PR breakdown

- **PR3 — landed.** `PeerManager` + `open_peer` (upstream connect + remote `SessionList`
  + local registration) + dispatch branching + upstream feed loop, behind the default-on
  `federation` feature. End-to-end: one GUI attaches to one remote session through the
  local daemon (single viewer). Two carry-overs to later PRs, out of PR3's single-viewer
  scope: the feed loop does not yet forward session-scoped events (titles, layout,
  lifecycle) — pane content only (PR4); and federated sessions are held only in memory by
  `PeerManager`, never in `ServerApp.sessions`, so they are already excluded from the
  PTY-only persistence path — no `persist/` change was needed (the "ghost panes" risk in
  the design note below does not arise).
- **PR4 core — landed.** Multiple local GUIs share one proxied pane over a single
  upstream link. Per-pane state grew from a flat viewer set to a `ProxiedPane`
  holding per-viewer sizes, a `CellGrid` **mirror** (fed by the feed loop from
  upstream snapshots/diffs/cursor/scrollback), and the upstream seqno + size:
  - **smallest-wins sizing** — the upstream pane size is `min` over local viewers;
    attach/resize/detach recompute it and forward **at most one** upstream `Resize`,
    only when it changes (vs. PR3's verbatim per-client forwarding);
  - **single upstream attach** — only the **first** viewer of a pane forwards `Attach`
    upstream; the **last** to leave forwards `Detach`;
  - **zero-round-trip late attach** — a second viewer is served a snapshot minted from
    the live mirror via `to_snapshot()` (stamped with the mirror's seqno so its later
    diffs line up), no upstream round-trip.
  - Verified by `two_guis_share_one_proxied_pane_with_smallest_wins` in
    `federation_e2e.rs`: a smaller second viewer shrinks the shared pane (the larger
    viewer receives a resized-down snapshot), and the late viewer sees the shared
    content.
- **PR4 facet — session-event forwarding — landed.** The feed loop now forwards
  session-scoped traffic, not just pane content: `Event { SessionEventMsg }` and
  `LayoutUpdate` have their embedded word/pane ID translated remote→local and are
  fanned out to every viewer under that word, so a GUI viewing a federated session
  sees its **title / layout / tab / lifecycle** updates (E2E: an OSC-2 title change on
  the remote pane arrives as `PaneTitleChanged` for the local pane). `Signal` and
  `FetchHistory` for a federated pane are forwarded upstream too (the `HistoryLines`
  reply is pane-scoped, so the feed loop routes it back to the requesting viewer).
- **PR4 remaining facets** (independent, lower-risk; a naive forward would break
  multi-viewer correctness, so each needs real arbitration/filtering state): pause
  upstream only when **all** local viewers are paused (issue #68 interplay); capability
  union upstream / filter downstream; and input-lock arbitration across local viewers.
- **PR5** — lean the GUI: always UDS-local to `kmuxd`; the parsed `--server` target
  becomes an `OpenPeer`; feature-gate `quinn`/`rustls`/`ssh` **out** of the GUI build
  (they move to `kmuxd`'s peer role). CI check the GUI no longer links the net stack.
- **PR6 — peer-down isolation + version guard — landed.** When the upstream link
  closes (remote daemon gone, network dropped), the feed loop **isolates** the
  failure: it sends every viewer a `SessionClosed` for its proxied session (so the
  GUI cleans up instead of hanging), drops the panes/sessions, and marks the
  connection `dead`. Locally-hosted PTY panes are untouched (separate relay). A dead
  peer is **reaped lazily** on the next `open_peer` to the same address — releasing
  its local words and clearing the word index — so re-federation starts clean
  (`open_peer` holds the `&ServerApp` needed to return words to the pool, avoiding a
  `Weak<ServerApp>` back-reference). Protocol-version mismatch is already rejected by
  the upstream `Auth` handshake (`open_peer` surfaces it as a `PeerError`). E2E:
  `remote_daemon_death_is_isolated_from_local_daemon` SIGKILLs the remote and asserts
  the GUI gets `SessionClosed` while the local daemon keeps serving new sessions.
- **PR6 remaining** (further hardening): upstream reconnect/backoff (reuse
  `kmux-connect` `recovery`), idle peer drop (debounced teardown when no local viewer
  remains), and backpressure refinement on a lagging viewer.

## Resolved — federation addressing & testability

**Decision: option (A), the direct endpoint.** `PeerTarget` gained a
`Direct { host, port, token, accept_invalid_certs }` variant (`PROTOCOL_VERSION = 26`)
alongside the existing `Ssh { .. }`, so a peer can be reached over TCP+TLS without SSH —
for LAN / same-host setups and, critically, for CI. `crates/kmuxd/tests/federation_e2e.rs`
spawns two loopback `kmuxd`s at isolated `XDG_*` dirs and federates over `Direct`, giving
PR3 the end-to-end coverage the project expects (`mise run test` is a pre-push gate).

PR3's `open_peer` implements the `Direct` path only; `Ssh` targets return a "not supported
yet" `PeerError` (the SSH `probe-or-start` + `-L` tunnel path is future work — it reuses
`kmux-connect`'s `prepare_ssh`, which already underpins the GUI's remote connections).
