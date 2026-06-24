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
    the upstream stream. (postcard is positional, so adding a field is breaking →
    the version bump, not just `#[serde(default)]`.)
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
- **PR6 — version guard — confirmed + tested.** A peer link is rejected on a
  `PROTOCOL_VERSION` mismatch by the standard upstream `Auth` handshake (the remote
  checks the version *before* the token), surfaced as `PeerError`. E2E
  `federation_surfaces_upstream_auth_rejection_as_peer_error` covers that branch via a
  wrong-token rejection (the same `AuthResult{success:false}` a version mismatch yields).
- **PR6 — idle peer drop: intentionally not added.** The daemon's existing
  idle-shutdown (`startup.rs`, debounced on client count → 0) already drops the whole
  daemon — peers and their links included — when no GUI is connected, which is the only
  case where dropping an upstream is unambiguously safe. Dropping a peer while a GUI is
  still connected would remove its sessions from the picker mid-use, so a separate
  debounced peer-idle-drop is both redundant (zero-GUI case) and wrong (GUI-connected
  case).
- **PR6 remaining** (future): **transparent upstream reconnect/backoff** — a *transient*
  upstream blip (remote alive, network hiccup) currently surfaces as `SessionClosed`,
  and recovery relies on the GUI re-issuing `OpenPeer` on its next local (re)connect
  (#121 GUI rewire). Reconnecting in-daemon (reuse `kmux-connect` `recovery`, preserve
  the local words, re-attach panes) is deferred: it needs remote-restart semantics
  (a restarted remote has *different* session words, so a silent re-map would be wrong)
  and a reachable flaky remote to verify, neither of which is in CI scope.

## Resolved — federation addressing & testability

**Decision: option (A), the direct endpoint.** `PeerTarget` gained a
`Direct { host, port, token, accept_invalid_certs }` variant (`PROTOCOL_VERSION = 26`)
alongside the existing `Ssh { .. }`, so a peer can be reached over TCP+TLS without SSH —
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
