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
- **Peer attribution + peer-routed create, `PROTOCOL_VERSION = 26 → 27`** (the
  launcher; see [architecture-frontend.md](architecture-frontend.md)):
  - `SessionEntry.peer: Option<PeerId>` — machine-readable "which machine is this
    session on", set by the daemon's `localize_entry` (it had only the decorated
    `name @ peer` before). The launcher/sidebar group by it; `kmux ls` adds a PEER
    column. It rides `SessionEntry`, **not** the persisted `SessionMeta`, so it
    needs no checkpoint migration.
  - `ClientMessage::SessionCreate.peer: Option<PeerId>` routes creation to a
    federated peer: the hub's `PeerManager::create_remote_session` forwards the
    `SessionCreate` upstream, draws a local `WordId` for the returned session, and
    registers the mapping (the create-time analog of `open_peer`'s adoption). The
    feed loop completes the request via a `pending_creates` oneshot, since it owns
    the upstream stream. (This landed under the retired positional Postcard
    codec, where adding a field was a wire break and needed a version bump.
    Under the named-map schema a `#[serde(default)]` field is additive — see
    [architecture-protocol-versioning.md](architecture-protocol-versioning.md).)
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
    upstream; the **last** to leave forwards `Detach`. That `Attach` always asks for a
    snapshot (`last_seqno: None`), whatever seqno the viewer resumes from (a GUI
    reconnecting re-attaches with its `last_seqno`, issue #208): the mirror is new, and
    a delta on top of a blank mirror would leave every later viewer a wrong grid;
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
- **PR4 facet — per-viewer pause (local) — landed.** A paused GUI (issue #68) viewing a
  proxied pane now stops receiving its output: `ServerApp::set_paused` also marks the
  client's federated viewers (`set_federated_paused` → `PeerManager::set_paused`), and
  `fan_out` skips paused viewers (never marking them lagged) — matching the local relay.
  A paused viewer still counts toward smallest-wins sizing and resyncs on resume via
  re-attach (which mints from the still-current mirror). The viewer pause is now
  **reason-aware** and honors per-pane auto-pause exemptions (issue #68 follow-up): a
  `Viewer` carries `pause_auto` + `no_auto_pause`, `fan_out` honors
  `Viewer::output_paused()`, and `PeerManager::{set_paused, set_pane_no_auto_pause}` mirror
  the local relay's `ClientSender` semantics. Units:
  `fan_out_skips_paused_viewer_without_dropping_it`,
  `fan_out_streams_auto_pause_exempt_viewer`.
- **PR4 remaining facets** (independent, lower-risk; a naive forward would break
  multi-viewer correctness, so each needs real arbitration/filtering state):
  - **pause-*union* upstream** — when **all** local viewers of a proxied pane are paused,
    stop the *upstream* stream too (the local-pause above already stops downstream
    delivery; this reclaims the federation link's bandwidth). The natural mechanism is
    `Detach` upstream on all-paused and re-`Attach` on first-resume — deferred because
    that resume cost (a fresh upstream snapshot) is a real trade-off vs. keeping the
    mirror warm, and the win only matters under sustained all-paused.
  - capability union upstream / filter downstream; and input-lock arbitration across
    local viewers.
- **PR5 prerequisite — SSH peer federation — landed.** `open_peer` now serves
  `PeerTarget::Ssh` as well as `Direct`: it negotiates the `-L` tunnel via
  `kmux-connect`'s `ssh::negotiate` and connects over TCP+TLS through it, sharing the
  Direct path's auth/list/register tail. The tunnel child is parked on the
  `PeerConnection` and torn down on close/reap (a `TunnelGuard` prevents leaks on the
  error paths). This unblocks the GUI sending `OpenPeer { Ssh }` for `--server user@host`.
- **PR5 — GUI connection model rewired (behavioral; runtime-pending).** The GUI now
  **always bootstraps the local daemon (UDS)** and federates a remote `--server` through it
  instead of dialling out itself:
  - `AppCore` gains `desired_peer: Option<PeerTarget>`. A remote `--server` (still parsed to
    a `ResolvedTarget::Ssh` for *identity*) is converted in `AppCore::new` into
    `desired_peer` + a **local** bootstrap target; `is_local` continues to reflect *server
    identity* (it drives auto-select), decoupled from the now-always-local transport.
  - `current_target()` always returns `LocalDaemon`, and after **every** successful local
    (re)connect the driver calls `federate_desired_peer()`, which issues
    `SessionManager::open_peer(PeerTarget)` → `ClientMessage::OpenPeer`. Re-federation after
    a reconnect is automatic and idempotent on the daemon.
  - The daemon's `PeerOpened`/`PeerError` replies become `SessionEvent::PeerOpened`/`PeerError`.
    `PeerOpened` re-arms the auto-select that was suppressed pre-federation and refreshes the
    session list (so the *remote's* sessions drive the picker); `PeerError` surfaces as a
    disconnect (reconnect retries the local link + `OpenPeer`). The launcher's expand /
    `disconnect_remote` actions drive peer setup and teardown through this same
    `open_peer`/`close_peer` path; `collapse_remote` keeps the link while an active session
    still belongs to the peer and sends `ClosePeer` once it does not (the old
    `ServerPicker`/`prepare_switch` switch model was retired).
  - **Known v1 limitation** (flagged for the runtime pass): a brief `Normal`-mode flash between
    local connect and federation. (Two earlier gaps are resolved: peer teardown now rides
    `collapse_remote`/`disconnect_remote`, and `--session NAME` matches the undecorated
    `SessionEntry::base_name()`, so it resolves a federated `NAME @ peer` session.) Covered by
    unit tests (`federate_desired_peer_*`, `peer_opened_*`, `peer_error_*`,
    `find_session_by_name_*`); the end-to-end UX needs a running GTK/Swift GUI + a reachable
    remote, so it is verified there, not in CI.
- **PR5b — lean GUI: the network stack is feature-gated out — landed.** A default-on
  `remote` feature on `kmux-connect` gates the direct-transport surface (QUIC via `quinn`,
  TCP+TLS via `rustls`/`tokio-rustls`, `ssh::negotiate`, and the `TransportSupervisor`).
  `kmux-client` and `kmux-app` forward it (`remote = ["<lower>/remote", …]`); the workspace
  sets `default-features = false` on `kmux-connect`/`kmux-client`/`kmux-app`, so the GUI
  frontends (`kmux`, `kmux-gtk`, `kmux-ffi`/`kmux-swift`) inherit the **lean** UDS-only stack
  and only `kmuxd` opts back in (its `federation` feature pulls `kmux-connect/remote`). What
  stays ungated is everything the lean GUI still needs: the UDS bootstrap + local-daemon
  lifecycle, `--server` string parsing → `RemoteTarget`/`PeerTarget` (identity, not transport,
  so the GUI can still build an `OpenPeer`), `ConnectResult`/`connect_uds`, and the transport
  scorer *types* (`RttSample`/`EndpointHealth`) woven into the always-compiled session manager.
  Verified by building each frontend in isolation and asserting `rustls`/`quinn`/`rcgen`/
  `tokio-rustls` are absent from its compiled deps (`cargo build -p kmux`/`kmux-gtk`/`kmux-ffi`
  in a clean target dir). **Consequence:** in a lean build the CLI `--server` paths (`kmux ls
  --server …`, `--dry-run --server …`) and `--test` transport probing are unavailable — they
  return a clear "not supported in this build" error; remote access is via the GUI's `OpenPeer`
  federation. The full client (with `--features remote`, e.g. for diagnostics) keeps them. The
  workspace `cargo build`/`clippy` still compiles every crate with `remote` on (each lib crate
  is a build root with its own default), so the gated paths are always linted; the leanness
  only materializes when a GUI binary is built in an invocation that excludes `kmuxd`.
- **PR6 — peer-down isolation + version guard — landed; superseded by the link
  supervisor below (issue #208).** A peer going down is isolated: locally-hosted PTY
  panes are untouched (separate relay), and the local daemon keeps serving. It no
  longer closes the peer's sessions: see **Peer unreachable and re-link**. Protocol-version
  mismatch is rejected by the upstream `Auth` handshake (`open_peer` surfaces it as a
  `PeerError`). E2E: `remote_daemon_death_is_isolated_from_local_daemon` SIGKILLs the
  remote and asserts the GUI's session is listed unreachable — never closed — while the
  local daemon keeps serving new sessions.
- **Peer unreachable and re-link (issue #208).** Each peer's link runs under one
  supervisor task (`federation/link.rs`, `spawn_link`), whose feed loop
  (`federation/feed.rs`) is the old one split into one handler per frame family:
  - **Liveness.** The hub pings the peer every `UPSTREAM_PING_INTERVAL` (5 s) and
    closes the link once the peer has been silent — not a frame, not a pong — past
    `UPSTREAM_DEADLINE` (15 s; `upstream_silent(last_inbound, now)`). Before, it only
    answered the peer's pings, so a black-holed link was never noticed.
  - **Unreachable.** When the link ends (closed, failed, or silent) the peer is marked
    down (`dead`): its sessions stay listed — under the same local words, flagged
    `SessionEntry::peer_unreachable` — its proxied panes and their viewers are kept,
    requests to it fail at once, and every client is sent the session list (an
    unsolicited `SessionListResult`, `RESYNC_REQUEST_ID`). No `SessionClosed` is sent.
  - **Re-link.** The supervisor re-opens the link with `kmux_client::backoff` (the
    GUI's policy), each attempt bounded by `CONNECT_ATTEMPT_TIMEOUT` (20 s); an SSH
    peer is re-negotiated each time, so a restarted remote (new token, new port) is
    found again. On success the peer's list is reconciled (`reconcile_sessions`): a
    session still listed keeps its local word, a new one draws one, one no longer
    listed is closed. Every proxied pane with viewers is re-attached. If the peer is
    the **same daemon run** as before (its `AuthResult.daemon_instance`, kept on the
    `PeerConnection`) and the pane's mirror is in step (seeded by a snapshot, none
    asked for since), it resumes from the seqno the mirror reached: the peer replays
    only what the link missed, or answers `SyncReset` and a snapshot when that is too
    much, and the frames continue the seqnos every viewer already has. Otherwise — a
    new run, whose seqnos start over, or a peer that sends no instance id — it is
    re-attached **for a snapshot**, which re-seeds the mirror and resyncs every
    streaming viewer (a paused one catches up when it resumes). Then every client is
    sent the list.
  - **Ordered lists.** Every add or remove of a peer's session (a reconcile, the
    peer's `SessionClosed`, `close_remote_session`, `close_peer`,
    `create_remote_session`'s registration)
    holds `PeerManager`'s membership gate together with the broadcast of that change,
    and every session list takes its federated entries (and is queued or broadcast)
    under the same gate, after the local session map's read lock. So a list taken
    before a federated session closed cannot reach a client after its `SessionClosed`
    and bring it back, and a re-list racing a create cannot register one session
    under two words. A peer closed while its link was being re-opened stays
    closed: `relink` and a reconcile check, under the gate, that the connection is
    still the peer's open one, so no word is drawn and no tunnel parked for it. A
    dropped link's receiver is dropped at once, so frames do not queue during the
    reopen (its socket ends on the next frame, or by TCP keepalive).
  - **When a session closes.** Only when the peer reports it closed (its
    `SessionClosed` event, or its session list no longer naming it) — then for every
    client, not just the session's viewers — or when the user closes it or its peer
    (`close_remote_session` for one session, once the peer acks; `close_peer` sends each
    client a `SessionClosed` for every one of its sessions). Whichever of the peer's
    ack and its own `SessionClosed` event lands first closes the session; the other
    finds it gone, so each client is told once.
  - **A current listing.** The hub keeps its cached entries in line with the peer: a
    `LayoutUpdate` patches the cached tab, and a session or tab created, closed or
    renamed on the peer makes the hub ask for the peer's list again; a
    `SessionListResult` from the peer — that answer, or the peer's own resync after
    the link lagged — is reconciled the same way.
  - **`open_peer`** reuses an open peer, reachable or not (an unreachable one is being
    re-opened already), so the lazy reaping of dead peers is gone. Reusing an
    unreachable peer re-targets it: the connection's connector, which the supervisor
    reads afresh on each attempt, is replaced by one for the new `PeerTarget` — a
    restarted Direct peer rotates its token, which is not part of its `PeerId`.
  Tests: the supervisor on the paused clock over a channel-played peer
  (`federation::link` — dropped link → unreachable → re-linked under the same word,
  panes re-attached for a snapshot; a re-linked pane resumes from its seqno only in
  the same daemon run (`a_relinked_pane_resumes_only_in_the_same_daemon_run`); a silent peer is pinged then declared unreachable;
  an answering one stays reachable; a closed peer is not re-opened), the frame handlers
  (`federation::feed`), and the E2E
  `a_frozen_peer_is_unreachable_then_restored_under_the_same_word` (SIGSTOP the remote,
  assert unreachable and no `SessionClosed`, SIGCONT, assert the same word back and the
  marker still on screen). GUIs show the state: the session's display name gains
  `· unreachable` (GTK sidebar and header), and the Swift sidebar row says
  "Unreachable — reconnecting" (`FfiSession::unreachable`, FFI ABI 28).
- **PR6 — viewer backpressure parity — landed.** A proxied pane's frames are fanned
  out by `ProxiedPane::fan_out`, which applies the **same** policy as the local PTY
  relay (`relay::broadcast_to_clients`): a viewer whose bounded data channel is full is
  sent a `Lagged` over its **unbounded ctrl channel** (out-of-band, so it lands despite
  the backed-up data channel) and dropped — it re-attaches and is served a fresh
  snapshot minted from the still-correct mirror (the mirror is fed *before* fan-out, so
  a slow viewer never desyncs it); a closed viewer is dropped silently. The downstream
  relay is now identical for PTY-backed and peer-backed panes. (Previously a full
  channel silently dropped the frame, diverging that viewer permanently.)
  **Session events and lifecycle** (titles, layout, tab/session events, and the
  death-path `SessionClosed`) instead travel over each viewer's **unbounded ctrl**
  channel (`viewers_under_word`), matching how the local daemon delivers events — so a
  backed-up pane content stream can never drop a title change, and a viewer that was
  full at the instant the peer died still receives its `SessionClosed` (otherwise the
  GUI would hang, as no further frames follow). Content keeps backpressure; events are
  guaranteed.
- **PR6 — concurrent-open race — fixed.** `open_peer`'s reuse check and its publish
  straddle the awaiting connect/auth/list, so two GUIs federating the same target at
  once could both connect and both publish, the second overwriting the first and leaking
  its link, feed task, and SSH tunnel. The winner is now chosen under a single `peers`
  lock (the loser tears its duplicate down and reuses the winner), so a race can never
  leak a connection or corrupt the word index. E2E:
  `concurrent_open_peer_to_same_target_converges_on_one_link`.
- **PR6 — SSH tunnel shutdown leak — fixed.** A federated `Ssh` peer parks its `ssh -L`
  child on the connection; `tokio::process::Child` is not kill-on-drop and runtime
  teardown (`shutdown_background`) races process exit, so daemon shutdown orphaned one
  `ssh` per SSH peer. `PeerManager::close_all` (via `ServerApp::close_all_peers`) now
  kills every tunnel and aborts every feed loop synchronously on the shutdown path
  (all paths, including a committed handoff — peer links are not migrated); a `Drop` on
  `PeerConnection` makes "never leaks its tunnel" structural. Unit:
  `close_all_kills_tunnels_and_clears_peers`.
- **PR6 — version guard — confirmed + tested.** A peer link is rejected when
  its semantic protocol range does not overlap in the standard upstream `Auth`
  handshake (the remote checks the range *before* the token), surfaced as `PeerError`. E2E
  `federation_surfaces_upstream_auth_rejection_as_peer_error` covers that branch via a
  wrong-token rejection (the same `AuthResult{success:false}` a version mismatch yields).
- **PR6 — idle peer drop: intentionally not added.** The daemon's opt-in
  idle-shutdown (`startup.rs`, debounced on client count → 0; off by default since
  issue #207) drops the whole
  daemon — peers and their links included — when no GUI is connected, which is the only
  case where dropping an upstream is unambiguously safe. Dropping a peer while a GUI is
  still connected would remove its sessions from the picker mid-use, so a separate
  debounced peer-idle-drop is both redundant (zero-GUI case) and wrong (GUI-connected
  case).
- **Transparent upstream reconnect — landed (issue #208).** See **Peer unreachable and
  re-link** above. A restarted remote restores its sessions under their own words
  (checkpoints, handoff), so the reconciled list maps them back to the same local
  words; a session the remote no longer has is closed rather than silently re-mapped.
  See **Re-discovering a restarted peer** below for what a `Direct` target can and
  cannot find again on its own.

### Re-discovering a restarted peer

What the link finds again by itself, and what it cannot:

| The peer … | `Ssh` target | `Direct` target |
|---|---|---|
| dropped off the network, came back | re-linked; same run → panes resume from their seqnos | same |
| restarted through a handoff (`kmux daemon restart`: the successor adopts the token, but listens on new ports unless they are pinned — `--tcp-port` or a `[[listen]]` block in `kmuxd.toml`) | re-linked (`probe-or-start` hands over the ports); new run → panes re-attached for a snapshot | re-linked if the peer's TCP port is pinned, as above; otherwise **not re-linked** — nothing listens on the old port |
| restarted outright (a new token, and new ports unless pinned) | re-linked: `probe-or-start` over SSH hands over the new token and ports | **not re-linked**: every attempt is refused (`AuthFailure::BadToken`), or finds nothing on the old port |

The `Direct` cells are the remaining limit. A `Direct` target carries the one
address and token the peer had when the user opened it; an unpinned port is
chosen afresh by each process, and a `kmuxd` started outright issues a new
random token (`startup.rs`; only a handoff successor adopts its predecessor's) —
by design, so a token from one run is worthless in the next. The hub has no channel
to learn the new one: learning it is exactly what SSH does for an `Ssh` target,
and a `Direct` target is, by definition, one without SSH. Making it automatic
needs a trust decision this change does not take, in one of two shapes: a token
that survives a restart (a persisted token, with a rotation policy of its own),
or a peer that re-admits a hub by its Ed25519 identity alone once it has proved
the token in an earlier run (a persisted allow-list of `machine_id`s). Either
widens what a stolen token or key is worth, so it is a design decision, not a
fix.

What the link does instead is say so. A refused re-open is typed
(`AuthResult.failure`), and the link's error — logged on every attempt, and the
`PeerError` of an `OpenPeer` — names it and its remedy:
`peer rejected authentication: invalid token (the peer issues a new token when it
restarts: open it again with the new one …)` (`federation::link::refused`). The
link keeps retrying at the capped backoff, and the peer's sessions stay listed
`peer_unreachable`, so opening the peer again with its new token
(`OpenPeer` re-targets an unreachable peer, see `open_peer` above) brings them
back under their own words.

## Resolved — federation addressing & testability

**Decision: option (A), the direct endpoint.** `PeerTarget` gained a
`Direct { host, port, token, accept_invalid_certs }` variant alongside the existing
`Ssh { .. }`, so a peer can be reached over TCP+TLS without SSH —
for LAN / same-host setups and, critically, for CI. `crates/kmuxd/tests/federation_e2e.rs`
spawns two loopback `kmuxd`s at isolated `XDG_*` dirs and federates over `Direct`, giving
PR3 the end-to-end coverage the project expects (`mise run test` is a pre-push gate).

`open_peer` now wires **both** paths. `Direct` is the endpoint verbatim; `Ssh` reuses
`kmux-connect`'s `ssh::negotiate` (the same `kmuxd probe-or-start` + `-L` tunnel that
underpins the GUI's remote connections) to bring up a loopback forward, then connects over
TCP+TLS through it — identical from there on (the TOFU pin is keyed to the *real* remote
`host:tcp_port`, not the ephemeral tunnel port). The `ssh -L -N` child is parked on the
`PeerConnection` (it is not kill-on-drop) and killed on `close_peer`/reap; a `TunnelGuard`
kills it on any error between `negotiate` and registration so a failed open can't leak it.

**Testability note:** the `Direct` endpoint remains the path exercised end-to-end in CI
(`federation_e2e.rs`, no sshd required). The SSH path's tunnel-lifecycle invariants are
unit-tested (`tunnel_guard_*`); its full `negotiate` handshake needs a reachable sshd and so
is verified against a real remote rather than in CI.
