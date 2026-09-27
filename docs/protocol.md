# The kmux data-plane protocol

This document is **normative**. It specifies the protocol a kmux client and a
`kmuxd` speak — and a `kmuxd` federation hub speaks to its peer — as it is
implemented today: how frames are laid out, what every message means, the
order and the lane it travels in, the states each side moves through, how
long each side waits, and how failures are reported. Where the code and this
document disagree, one of them is a bug; `kmux-protocol`'s `spec` tests hold
the catalogue, the timing table and the state tables below to the code, so
most disagreements fail the build.

Companions, which this document does not repeat:

- [architecture-protocol-versioning.md](architecture-protocol-versioning.md) —
  how the schema evolves: the version range, capabilities, the lenient-enum
  rule. Change that document when you change what is compatible.
- [connection.md](connection.md) — the transports under the frames (UDS,
  TCP+TLS, QUIC, SSH tunnels), client supervision, and the GUI → local daemon →
  federated peer architecture.
- [architecture-federation.md](architecture-federation.md) — what a hub does
  with a peer's messages.

It does not cover the daemon's other contracts, each versioned on its own: the
JSON control socket and handoff (`kmux_protocol::control_rpc`), the
daemon↔VT-worker contract (`kmux-worker-protocol`), persisted state, and the
`kmux-ffi` C ABI.

## Contents

1. [Frames](#frames)
2. [Handshake](#handshake)
3. [Lanes and ordering](#lanes-and-ordering)
4. [Message catalogue](#message-catalogue)
5. [State machines](#state-machines)
6. [Timing](#timing)
7. [Error model](#error-model)
8. [Known gaps](#known-gaps)

## Frames

Every transport carries the same frames, back to back:

```text
[u32 big-endian length][u8 codec tag][payload …]
```

- **Length** counts the tag byte and the payload. It is at least 1 and at most
  `MAX_FRAME_SIZE` (64 MiB, `kmux_protocol::codec`). A reader refuses a longer
  frame before allocating it (`ProtocolError::FrameTooLarge`) and a zero length
  as malformed (`MalformedFrame`); either ends the connection.
- **Codec tag** says how the payload is stored. Tags are permanent:

  | Tag | Payload | A reader … |
  |---:|---|---|
  | `0`, `1` | Retired positional Postcard (protocol ≤ 40) | refuses the connection: `LegacyPostcardCodec`, an upgrade error |
  | `2` | Named MessagePack, stored verbatim | decodes it |
  | `3` | Named MessagePack, zstd-compressed | inflates it — refusing more than `MAX_DECOMPRESSED_SIZE` (64 MiB) — then decodes it |
  | other | — | refuses the connection: `UnknownCodec` |

  Tag `3` is sent only on a connection whose `AuthResult.negotiated_capabilities`
  holds `frame.zstd`, and then only by the daemon's per-connection policy
  (`AuthResult.compression`; see [compression.md](compression.md)). Because
  every frame names its own codec, a reader needs no per-connection state to
  read it. The handshake itself always uses tag `2`.
- **Payload** is one `ClientMessage` (client → daemon) or `ServerMessage`
  (daemon → client), MessagePack with named fields (`rmp_serde::to_vec_named`,
  exact-pinned). A top-level message is adjacently tagged —
  `{"type": "<Variant>", "data": {…}}`, or `{"type": "<Variant>"}` for a unit
  variant. A field a receiver does not know is ignored; a field it expects and
  does not find takes its `#[serde(default)]`, or fails the decode.

A frame that is well-formed but does not decode as a message is answered with
`Error { request_id: None, code: InvalidMessage }` by the daemon, which keeps
the connection; a client logs and drops it. A nested enum value a newer peer
added decodes as `Unknown` instead of failing the frame, for the enums listed in
[architecture-protocol-versioning.md](architecture-protocol-versioning.md#unknown-variants).

## Handshake

A connection is unauthenticated until the four-message handshake completes:

```text
client                                   daemon
  │ Auth { token, protocol_range,          │
  │   protocol_capabilities, public_key,   │
  │   connection_id?, resume_instance?, …} │
  │ ─────────────────────────────────────▶ │  1. range overlap?   no → AuthResult{ProtocolMismatch}, close
  │                                        │  2. token valid?     no → AuthResult{BadToken}, close
  │                  AuthChallenge { nonce } │
  │ ◀───────────────────────────────────── │
  │ AuthProof { signature over nonce }     │
  │ ─────────────────────────────────────▶ │  3. signature valid? no → AuthResult{IdentityRejected}, close
  │ AuthResult { success, client_id,       │  4. register, or resume a registration
  │   connection_id, daemon_instance, … }  │
  │ ◀───────────────────────────────────── │
```

1. The protocol range is checked first, so a peer that cannot decode the rest
   is refused before anything privileged happens. The negotiated version is
   the highest both ranges hold ([versioning](architecture-protocol-versioning.md)).
2. The token is this daemon run's (`$XDG_RUNTIME_DIR/kmux/token`, or the one
   `probe-or-start` hands an SSH client). A handoff successor adopts its
   predecessor's; a daemon started outright draws a new one.
3. The signature proves the client holds the private key behind `public_key`;
   its SHA-256 fingerprint is the connection's `machine_id`
   ([architecture-identity.md](architecture-identity.md)).
4. The channel is registered afresh, or — when `Auth` carried a `connection_id`
   and the three conditions in
   [connection.md](connection.md#connectionid-and-session-resumption) hold
   (same run, live registration, same machine) — resumes that registration.
   `AuthResult.daemon_instance` names the daemon run; a client compares it
   across reconnects to know whether its seqnos and held input still apply.

A refused `AuthResult` carries `failure` (an `AuthFailure`) for the client to
decide on and `reason` (its text) for a person; the daemon flushes it, for up
to `AUTH_REFUSAL_FLUSH`, then closes. Before the handshake completes every
other message is answered with `Error { code: NotAuthenticated }` and the
connection is kept, so a client that spoke early can recover by sending
`Auth`; a connection that has not authenticated within `AUTH_DEADLINE` is
closed without a word.

## Lanes and ordering

The daemon queues every message for a connection on one of two lanes (see
`kmuxd::outbound` and [connection.md](connection.md#server-side-flow-control-and-deadlines)):

- **Control** — replies, errors, pings, session and layout events, session
  lists, input-lock answers, `Lagged`. Never dropped: if even the whole queue is
  full the client has stopped reading, and the connection is closed instead.
- **Pane data** — each attached pane's stream: `TerminalSnapshot`,
  `TerminalUpdate`, `CursorUpdate`, `ScrollbackAppend`, `SyncReset`,
  `GridDigest`, and the flood-prone events `PaneBell`, `PaneTitleChanged` and
  `PaneProgressChanged`. May be dropped under congestion; a pane whose frames
  were dropped is resynced (`SyncReset` then `TerminalSnapshot`) or told
  (`Lagged`), never silently left behind. On QUIC each pane has its own
  unidirectional stream; on TCP and UDS the lanes share one ordered queue.

What is guaranteed:

- **Within one pane's data**, frames arrive in the order the daemon produced
  them, and `TerminalUpdate`, `CursorUpdate` and `ScrollbackAppend` share one
  sequence-number space per pane, each frame one more than the last. A
  `TerminalSnapshot` carries the seqno it is current to; the next frame is one
  more. `SyncReset` is always followed at once by a `TerminalSnapshot` on the
  same path. A `GridDigest` follows the frames it certifies on the same path.
- **Within the control lane**, messages arrive in the order they were queued. A
  reply is queued before the broadcast its request caused, so the requester
  sees e.g. `SessionCreated` before `Event SessionCreated` (except
  `NotifyAccepted`, queued after its `PaneAttention` broadcast). A session list
  is queued while the daemon's session map is read-locked, so a concurrent
  change's event arrives after it.
- **Between the lanes**, nothing is guaranteed: `Lagged` on control may overtake
  pane frames still in flight, which the client then discards as out of sync.

A client treats a pane's seqno as a promise: a frame whose seqno is not the one
it expects means it missed something, and it re-attaches the pane (`Attach {
last_seqno: None }`) rather than apply it.

## Message catalogue

Columns: **Answer** is what the daemon sends the requester (and on failure);
**Also** is what everyone else receives; **Idem.** says whether sending the
message twice has the effect of once; **Cap.** is the capability a sender needs
before sending it — none today, the column exists so the first one is recorded
here. A `request_id` is chosen by the client, counts up from 0, and is echoed
in the reply; `RESYNC_REQUEST_ID` (`u64::MAX`) marks an unsolicited session
list. "Federated" describes a request for a session a hub proxies from a peer
([architecture-federation.md](architecture-federation.md)).

### Client → daemon

<!-- spec:client-messages -->
| Message | Answer | Also | Idem. | Cap. |
|---|---|---|---|---|
| `Auth` | `AuthChallenge`; a refusal is `AuthResult { failure }`, then close | — | resend before `AuthProof` replaces the challenge; ignored once authenticated | — |
| `AuthProof` | `AuthResult { success: true }`; a bad signature is `AuthResult { IdentityRejected }`, then close; with no challenge, `Error NotAuthenticated` | — | ignored once authenticated | — |
| `ChannelReady` | `ChannelSwitched { old_transport }` if this channel resumed a registration, else nothing | — | yes: the pending switch is consumed | — |
| `SessionCreate` | `SessionCreated`; `Error` | `Event SessionCreated`, `PaneSpawned` | no: creates another | — |
| `SessionClose` | `SessionClosed`; `Error SessionNotFound` | `Event SessionClosed` | effect yes; a second is `SessionNotFound` | — |
| `SessionList` | `SessionListResult` (local then federated sessions) | — | yes | — |
| `ProcessOverview` | `ProcessOverviewResult` (local and every peer's panes) | — | yes | — |
| `SessionRename` | `SessionRenamed` (no `request_id`); `Error SessionNotFound` | `Event SessionRenamed` | yes | — |
| `SessionListClosed` | `ClosedSessionListResult` | — | yes | — |
| `SessionRestore` | `SessionCreated`; `Error` | `Event SessionCreated`, `PaneSpawned` | no: a second is `SessionNotFound` | — |
| `PaneCreate` | `PaneCreated` (the pane opens in a new tab); `Error` | `Event TabCreated`, `PaneSpawned` | no | — |
| `PaneClose` | `PaneClosed`; `Error PaneNotFound` | `LayoutUpdate`, or `Event TabClosed` / `SessionClosed` when it was the last | no: a second is `PaneNotFound` | — |
| `TabCreate` | `TabCreated`; `Error` | `Event TabCreated`, `PaneSpawned` | no | — |
| `TabClose` | `TabClosed`; `Error SessionNotFound` | `Event TabClosed`, or `SessionClosed` for the last tab | effect yes | — |
| `TabRename` | nothing; `Error SessionNotFound` | `Event TabRenamed` (to the requester too) | yes | — |
| `TabReorder` | nothing; `Error SessionNotFound` (no `request_id`) | `Event TabsReordered` | yes | — |
| `PaneSplit` | `PaneSplit`; `Error` | `LayoutUpdate`, `PaneSpawned` | no | — |
| `PaneSwap` | nothing; `Error SessionNotFound` (no `request_id`) | `LayoutUpdate` | no: a second undoes the first | — |
| `SetLayoutRatios` | nothing; `Error SessionNotFound` | `LayoutUpdate` | yes | — |
| `ApplyLayoutScheme` | nothing; `Error SessionNotFound`; an `Unknown` scheme is ignored | `LayoutUpdate` | yes | — |
| `SetFocus` | nothing; `Error SessionNotFound` | `LayoutUpdate` | yes | — |
| `PtyInput` | nothing; `Error PaneNotFound` / `InputLocked` / `InternalError` (queue full) | — | no: bytes are written again | — |
| `PtyKeyBatch` | as `PtyInput` | — | no | — |
| `PtyPaste` | as `PtyInput` | — | no | — |
| `Resize` | nothing; `Error PaneNotFound` | if the pane's smallest-wins size changes: `Event PaneResized` and a `TerminalSnapshot` to each viewer | yes | — |
| `Attach` | the pane's replay on its data path: `TerminalSnapshot`, the missed `TerminalUpdate`s, or `SyncReset` + `TerminalSnapshot`; `Error PaneNotFound` | maybe a resize | yes: re-attaching replaces the attachment | — |
| `Detach` | nothing | maybe a resize | yes | — |
| `Signal` | nothing; `Error PaneNotFound` / `InternalError` | — | no | — |
| `RequestInputLock` | `InputLockGranted` or `InputLockDenied { holder }`; `Error PaneNotFound` | — | yes | — |
| `ReleaseInputLock` | `InputLockReleased` if this client held it, else nothing | — | effect yes | — |
| `SetSnapshotMode` | nothing | — | yes | — |
| `SetPaused` | nothing | — | yes | — |
| `SetPaneNoAutoPause` | nothing | — | yes | — |
| `FetchHistory` | `HistoryLines`; `Error PaneNotFound` | — | yes | — |
| `Ping` | `Pong { seq }` | — | yes | — |
| `Pong` | nothing (records a round trip if `seq` is the last ping's) | — | yes | — |
| `ListDirectory` | `DirectoryListing` (a failure is its `error`, never `Error`) | — | yes | — |
| `OpenPeer` | `PeerOpened`, or `PeerError` | — | yes: an open peer is reused | — |
| `ClosePeer` | `PeerClosed`, even for an unknown peer | `Event SessionClosed` per peer session | yes | — |
| `ClientList` | `ClientListResult`; `Error SessionNotFound` | — | yes | — |
| `KickClient` | `ClientKicked`; `Error SessionNotFound` / `ClientNotFound` | `SessionKicked` to the kicked connection | effect yes | — |
| `Notify` | `NotifyAccepted`; `Error PaneNotFound` | `Event PaneAttention` (a fresh `attention_id` each time) | no | — |
| `FetchLogs` | `LogChunk`s, then `LogEnd` unless `follow`; `Error InternalError` | — | yes (each `follow` is another tail) | — |

Notes that apply to rows above:

- **Federated sessions.** `SessionCreate { peer }`, `SessionClose`, `TabClose`,
  `PaneSwap`, `SetLayoutRatios`, `ApplyLayoutScheme`, `SetFocus`, the three
  input messages, `Resize`, `Attach`, `Detach`, `Signal`, `FetchHistory`,
  `ClientList` and `KickClient` are forwarded to the peer under the peer's own
  ids, and its answer comes back translated. A hub attaches a proxied pane
  upstream once, for its first viewer, and serves later viewers from its
  mirror. The rest are answered by the hub alone — see [Known gaps](#known-gaps).
- **`Attach` replay** is `compute_replay`: `last_seqno: None` is a snapshot;
  `Some(n)` within the pane's retained diffs is exactly the diffs after `n`,
  unless there are more than 256 of them or 256 KiB, then `SyncReset` + a
  snapshot; older than the retained diffs, or past the pane's current seqno
  (another daemon run's), `SyncReset` + a snapshot. `Attach` also clears the
  connection's pause and per-pane exemption for that pane; snapshot mode
  survives it.

### Daemon → client

<!-- spec:server-messages -->
| Message | Kind | Lane | Ordering, and what a client does | Cap. |
|---|---|---|---|---|
| `AuthChallenge` | reply to `Auth` | control | before `AuthResult` | — |
| `AuthResult` | reply to `AuthProof`, or to a refused `Auth` | control | last of the handshake; a refusal is flushed, then the connection closes | — |
| `ChannelSwitched` | reply to `ChannelReady` | control | the client closes the old channel | — |
| `SessionCreated` | reply to `SessionCreate` / `SessionRestore` | control | before its `Event SessionCreated` | — |
| `SessionClosed` | reply to `SessionClose` | control | before its `Event SessionClosed`; handled like the event | — |
| `SessionListResult` | reply to `SessionList`; unsolicited with `RESYNC_REQUEST_ID` after this connection lagged the event broadcast, or a peer changed | control | the client takes any list as the whole truth: an unlisted session is closed | — |
| `ClosedSessionListResult` | reply to `SessionListClosed` | control | — | — |
| `ProcessOverviewResult` | reply to `ProcessOverview` | control | — | — |
| `PaneCreated` | reply to `PaneCreate` | control | before its `Event TabCreated` | — |
| `PaneClosed` | reply to `PaneClose` | control | before its layout or close event; handled like `Event PaneClosed` | — |
| `TabCreated` | reply to `TabCreate` | control | before its event | — |
| `TabClosed` | reply to `TabClose` | control | before its event | — |
| `PaneSplit` | reply to `PaneSplit` | control | before its `LayoutUpdate` | — |
| `LayoutUpdate` | broadcast after every layout change | control | the client replaces the tab's tree (last writer wins) | — |
| `Lagged` | push: this client's pane stream overflowed | control | may overtake pane frames; the client clears the pane and re-attaches | — |
| `SyncReset` | push, inside an attach replay or a congestion resync | pane data | immediately followed by `TerminalSnapshot` | — |
| `GridDigest` | push, every 32nd seqno by default | pane data (lossy) | after the frames it covers; checked only when the client is at exactly that seqno; a mismatch re-attaches | — |
| `Event` | broadcast of a `SessionEventMsg` | control, but `PaneBell` / `PaneTitleChanged` / `PaneProgressChanged` on pane data (lossy) | after the reply that caused it; `Unknown` is ignored | — |
| `Error` | reply | control | see [Error model](#error-model) | — |
| `Ping` | push every `PING_INTERVAL` | control | answered with `Pong` | — |
| `Pong` | reply to `Ping` | control | — | — |
| `TerminalUpdate` | push: a pane's diff | pane data | next seqno; a gap re-attaches | — |
| `TerminalSnapshot` | push: attach, resize, resync, worker respawn, snapshot mode | pane data | resets the pane's expected seqno | — |
| `CursorUpdate` | push: cursor or modes only | pane data | next seqno | — |
| `ScrollbackAppend` | push: lines appended to the pane's history | pane data | next seqno; before the `TerminalUpdate` of the same output, except on a scrollback reset | — |
| `HistoryLines` | reply to `FetchHistory` | control (a federated pane's: pane data) | — | — |
| `InputLockGranted` | reply to `RequestInputLock` | control | — | — |
| `InputLockDenied` | reply to `RequestInputLock` | control | names the holder | — |
| `InputLockReleased` | reply to `ReleaseInputLock` | control | — | — |
| `SessionRenamed` | reply to `SessionRename` | control | before its event; handled like it | — |
| `DirectoryListing` | reply to `ListDirectory` | control | — | — |
| `PeerOpened` | reply to `OpenPeer` | control | the peer's sessions arrive in the next session list | — |
| `PeerClosed` | reply to `ClosePeer` | control | — | — |
| `PeerError` | reply to `OpenPeer` | control | `reason` says why, and for a refused peer what to do | — |
| `ClientListResult` | reply to `ClientList` | control | — | — |
| `ClientKicked` | reply to `KickClient` | control | — | — |
| `SessionKicked` | push, to the kicked connection only | control | the client leaves the session | — |
| `NotifyAccepted` | reply to `Notify` | control | may follow its `PaneAttention` | — |
| `LogChunk` | reply stream to `FetchLogs` | control | in order | — |
| `LogEnd` | end of a non-`follow` `FetchLogs` | control | after every `LogChunk` | — |

## State machines

Each row is a transition and the test that pins it. The `spec` tests fail if a
named test no longer exists; a transition with no test is not allowed in these
tables.

### Connection (client, `kmux_client::connection_state::ConnectionState`)

<!-- spec:states-connection -->
| From | Event | To | Pinned by |
|---|---|---|---|
| `Idle`, `Disconnected` | a bootstrap starts (`prepare_reconnect`) | `Handshaking`, keeping `connection_id` | `reconnect_preserves_connection_id_for_handoff` |
| `Handshaking`, `Reconnecting` | the bootstrap's `AuthResult { success: true }` | `Connected { transport }` | `a_new_link_resumes_every_visible_pane_from_its_last_seqno` |
| `Handshaking` | the bootstrap fails | `Disconnected { BootstrapFailed }` | `failed_bootstrap_stashes_error_and_disconnects` |
| any | `AuthResult { success: false }` | `Disconnected { AuthFailed }`, with the hint for a protocol mismatch | `auth_failed_emits_event_and_clears_connection`, `a_protocol_mismatch_disconnects_with_the_upgrade_hint` |
| `Connected` | the channel closes | `Reconnecting { attempt: 1 }` | `a_dropped_link_keeps_the_ui_and_schedules_a_reconnect` |
| `Connected` | no frame for `SILENCE_TIMEOUT` | `Reconnecting { attempt: 1 }` | `a_silent_link_is_lost_at_the_liveness_timeout`, `timeout_triggers_when_no_inbound` |
| `Connected` | the SSH tunnel of a tunnelled link dies | `Reconnecting { attempt: 1 }` | `a_dead_ssh_tunnel_loses_only_a_tunnelled_link` |
| `Connected` | the supervisor promotes a transport | `Connected { new transport }` | `a_transport_swap_makes_the_new_channel_the_live_one` |
| `Reconnecting { n }` | an attempt fails | `Reconnecting { n + 1 }` after the backoff | `a_failed_reconnect_attempt_schedules_the_next`, `reconnect_retries_on_the_backoff_schedule_until_connected` |
| `Reconnecting` | an attempt is refused (range or token) | `Disconnected` | `a_refused_reconnect_attempt_stops_retrying` |
| `Reconnecting` | an attempt succeeds, same daemon run | `Connected`; held input delivered in order | `keys_typed_during_an_outage_are_delivered_in_order_on_reconnect` |
| `Reconnecting` | an attempt succeeds, another run | `Connected`; held input dropped | `outage_input_is_dropped_for_another_daemon_run_or_when_too_old` |

### Authentication (daemon, per connection)

<!-- spec:states-auth -->
| From | Event | To | Pinned by |
|---|---|---|---|
| unauthenticated | `Auth`, range and token good | challenged (`AuthChallenge` sent) | `auth_rejects_invalid_signature` |
| unauthenticated | `Auth`, ranges disjoint | closed (`ProtocolMismatch`) | `auth_rejects_disjoint_protocol_range_before_token_validation` |
| unauthenticated | `Auth`, bad token | closed (`BadToken`), the refusal flushed first | `invalid_token_result_is_flushed_before_close` |
| unauthenticated | any other message | unauthenticated (`Error NotAuthenticated`) | `an_unauthenticated_client_is_told_to_send_auth_first`, `an_auth_proof_without_a_challenge_is_refused_without_closing` |
| unauthenticated | `AUTH_DEADLINE` passes | closed, nothing sent | `a_silent_unauthenticated_connection_is_closed_at_the_auth_deadline`, `auth_verdict_closes_only_an_unauthenticated_connection_past_the_deadline` |
| challenged | `AuthProof`, bad signature | closed (`IdentityRejected`) | `auth_rejects_invalid_signature` |
| challenged | `AuthProof`, good, no `connection_id` | authenticated, a fresh registration | `register_client_assigns_fresh_ids_and_stores_metrics` |
| challenged | `AuthProof`, good, resumable `connection_id` | authenticated, the registration resumed (generation + 1) | `a_resuming_channel_holds_the_bumped_generation` |
| challenged | `AuthProof`, `connection_id` of another run | authenticated, a fresh registration (`OtherRun`) | `a_channel_resuming_another_run_is_registered_afresh`, `resume_from_another_daemon_run_registers_a_fresh_connection` |
| challenged | `AuthProof`, `connection_id` of another machine | authenticated, a fresh registration (`OtherMachine`) | `resume_with_another_machines_identity_registers_a_fresh_connection` |
| authenticated | `Auth` or `AuthProof` | authenticated, ignored | `a_second_auth_after_authentication_is_ignored_silently`, `a_stray_auth_proof_after_authentication_is_ignored_silently` |
| authenticated | no frame for `PONG_DEADLINE` after a ping | closed | `an_authenticated_client_that_never_answers_a_ping_is_closed`, `pong_verdict_closes_only_past_the_deadline_of_an_unanswered_ping` |
| authenticated | a write takes `FRAME_WRITE_TIMEOUT` | closed | `a_peer_that_stops_reading_is_closed_after_the_write_timeout` |
| authenticated | a control message finds the queue full | closed | `a_control_message_on_a_full_queue_closes_the_connection` |
| authenticated | the channel ends while a newer one resumed its registration | the registration stays with the newer channel | `a_superseded_channel_ending_leaves_the_resumed_connection_intact`, `release_connection_honours_only_the_current_generation` |

### Attach (a pane on the client, `PaneSync`, and its stream on the daemon)

<!-- spec:states-attach -->
| From | Event | To | Pinned by |
|---|---|---|---|
| detached | `Attach { last_seqno: None }` | awaiting sync; the daemon sends a snapshot | `attach_sends_current_size`, `compute_replay_fresh_attach_returns_full_snapshot` |
| detached | `Attach` for an unknown pane | detached (`Error PaneNotFound`) | `attach_to_an_unknown_pane_errors_and_starts_no_stream` |
| awaiting sync | `TerminalSnapshot` at seqno `n` | synced, expecting `n + 1` | `terminal_snapshot_transitions_to_synced` |
| awaiting sync | a diff | awaiting sync, the diff discarded | `terminal_update_discarded_when_awaiting_sync`, `cursor_update_for_a_pane_awaiting_sync_is_discarded_and_counted` |
| synced (`n`) | a frame at seqno `n` | synced (`n + 1`) | `cursor_update_applies_the_cursor_and_advances_the_expected_seqno`, `scrollback_append_appends_the_lines_and_advances_the_expected_seqno` |
| synced (`n`) | a frame at another seqno | awaiting sync; re-attached | `cursor_update_with_a_seqno_gap_resyncs_the_pane` |
| synced | `SyncReset` | awaiting sync | `sync_reset_clears_the_grid_and_parks_the_pane_awaiting_sync` |
| synced | `Lagged` | awaiting sync; re-attached | `lagged_clears_the_grid_counts_the_lag_and_reattaches_the_pane` |
| synced (`n`) | `GridDigest` at `n - 1` that does not match | awaiting sync; re-attached | `grid_digest_mismatch_triggers_one_resync`, `grid_digest_match_does_not_resync` |
| synced (`n`) | a new link to the same daemon run | `Attach { last_seqno: Some(n - 1) }`; the daemon replays the rest | `a_new_link_resumes_every_visible_pane_from_its_last_seqno`, `compute_replay_delta_under_threshold_returns_delta`, `a_resumed_client_is_replayed_from_its_last_seqno` |
| synced | a new link to another daemon run | `Attach { last_seqno: None }` | `a_new_link_to_another_daemon_run_attaches_every_pane_afresh` |
| (daemon) | `Attach { Some(n) }` past the retained diffs or the threshold | `SyncReset` + snapshot | `compute_replay_delta_over_threshold_coalesces_to_syncreset` |
| (daemon) | `Attach { Some(n) }` past the pane's current seqno | `SyncReset` + snapshot | `compute_replay_from_a_seqno_this_pane_never_reached_resets` |
| (daemon) | a viewer's pane stream is full | `Lagged` on control; the viewer is dropped from the pane | `broadcast_sends_lagged_via_ctrl_when_data_full`, `broadcast_removes_client_after_full` |
| (daemon) | the shared TCP/UDS queue is congested | pane frames dropped, then `SyncReset` + snapshot | `a_congested_pane_stream_stays_bounded_and_resyncs_once_the_client_reads` |
| attached | `Detach` | detached | `detach_from_a_pane_this_client_never_attached_answers_nothing` |

### Pause (a connection, [connection-pause.md](connection-pause.md))

<!-- spec:states-pause -->
| From | Event | To | Pinned by |
|---|---|---|---|
| streaming | `SetPaused { paused: true, auto: false }` | manually paused: no pane output, no `Lagged`, still counted for size | `set_paused_marks_client_across_panes`, `broadcast_skips_paused_client`, `paused_client_not_marked_lagged_when_data_full`, `paused_client_still_counts_toward_effective_size` |
| streaming | `SetPaused { paused: true, auto: true }` | auto-paused: exempt panes (`SetPaneNoAutoPause`) keep streaming | `auto_pause_exempt_pane_keeps_streaming_until_manual`, `fan_out_streams_auto_pause_exempt_viewer` |
| auto-paused | `SetPaused { auto: false }` | manually paused, exemptions included | `manual_pause_overrides_auto_pause_exemption` |
| paused | `SetPaused { paused: false }`, then `Attach { Some(last) }` per visible pane | streaming, caught up to the final state | `reconcile_pause_sends_setpaused_and_resume_reattaches_visible_panes`, `reattach_preserves_snapshot_mode_and_clears_pause` |
| manually paused (client) | a keystroke | dropped | `manual_pause_drops_user_input` |
| auto-paused (client) | a keystroke | resumes, and the key is sent | `keystroke_resumes_auto_pause_but_not_manual_pause`, `auto_pause_does_not_drop_input` |

### Peer (a hub's link to a federated `kmuxd`)

<!-- spec:states-peer -->
| From | Event | To | Pinned by |
|---|---|---|---|
| absent | `OpenPeer`, handshake and list succeed | linked; its sessions listed under local words | `gui_attaches_to_remote_session_through_local_daemon`, `a_handshake_answers_the_challenge_then_fetches_the_list` |
| absent | `OpenPeer` fails to connect or is refused | absent (`PeerError`) | `open_peer_that_cannot_be_reached_answers_peer_error_naming_the_peer`, `federation_surfaces_upstream_auth_rejection_as_peer_error`, `a_handshake_fails_on_an_unanswered_challenge_a_refusal_or_silence` |
| absent | `OpenPeer` with an `Unknown` target | absent (`PeerError`) | `open_peer_with_a_target_kind_this_daemon_does_not_know_answers_peer_error` |
| absent | two `OpenPeer`s for one target race | one link; the loser torn down | `the_loser_of_a_concurrent_open_is_torn_down`, `concurrent_open_peer_to_same_target_converges_on_one_link` |
| linked | the peer answers pings | linked | `a_peer_that_answers_stays_reachable` |
| linked | the link closes | unreachable: sessions kept, flagged `peer_unreachable` | `a_dropped_link_leaves_the_peer_unreachable_then_relinks_under_the_same_word`, `remote_daemon_death_is_isolated_from_local_daemon` |
| linked | the peer is silent for `SILENCE_TIMEOUT` | unreachable | `a_silent_peer_is_pinged_then_declared_unreachable`, `upstream_silent_only_past_the_deadline` |
| unreachable | a re-open succeeds | linked; sessions reconciled under their words; every proxied pane re-attached for a snapshot | `a_dropped_link_leaves_the_peer_unreachable_then_relinks_under_the_same_word`, `a_frozen_peer_is_unreachable_then_restored_under_the_same_word` |
| unreachable | a re-open is refused | unreachable; the error names the remedy | `a_refusal_names_its_remedy` |
| unreachable | `OpenPeer` for it again | unreachable, re-targeted | `reopening_an_unreachable_peer_retargets_its_link` |
| linked, unreachable | `ClosePeer` | absent; `SessionClosed` for each of its sessions | `closing_a_peer_closes_its_sessions_for_every_client`, `a_peer_closed_during_its_reopen_is_left_closed`, `a_closed_peer_is_not_reopened` |

### Session (daemon)

<!-- spec:states-session -->
| From | Event | To | Pinned by |
|---|---|---|---|
| absent | `SessionCreate` | live; everyone told | `creating_a_session_tells_every_client_not_only_the_creator` |
| absent | `SessionCreate` on a peer | live on the peer, listed under a local word | `gui_creates_a_session_on_a_federated_peer` |
| live | `SessionClose` | inactive (in the graveyard); everyone told | `closing_a_session_tells_every_client_not_only_the_requester`, `close_persists_graveyard_file_for_crash_recovery` |
| live | `SessionRename` | live, renamed; everyone told | `renaming_a_session_tells_every_client_not_only_the_renamer` |
| inactive | `SessionRestore` | live | `close_then_restore_roundtrip` |
| inactive | the graveyard's count or age cap | gone | `count_cap_evicts_oldest`, `ttl_prunes_stale_entries` |
| live (federated) | its peer becomes unreachable, then returns | live, flagged `peer_unreachable` meanwhile | `a_frozen_peer_is_unreachable_then_restored_under_the_same_word` |
| live (federated) | the peer closes it, or stops listing it | gone for every client | `a_session_the_peer_closes_is_closed_for_every_client`, `a_session_its_peer_closes_leaves_the_hubs_list`, `a_peer_session_list_reconciles_the_hub_listing` |
| (client) listed | a session list without it | closed on the client | `a_resync_list_closes_sessions_it_no_longer_lists` |

## Timing

Every interval and deadline the two ends must agree on lives in
`kmux_protocol::timing`; the client, the daemon and a hub import it, and the
module checks the relations between them at compile time (a peer is called
silent only after two pings could have been answered, the daemon waits longer
for a pong than a client does for any frame, a pane stream is reset only after
QUIC itself would have called the connection idle). A timing that is one
process's own business — a redraw tick, the daemon's worker restart backoff —
stays beside its code.

<!-- spec:timing -->
| Constant | Value | What it bounds |
|---|---|---|
| `PING_INTERVAL` | 5 s | how often each end pings the other (daemon → client, client → daemon, hub → peer) |
| `SILENCE_TIMEOUT` | 15 s | how long a client or hub lets its daemon be silent before calling the link dead |
| `PONG_DEADLINE` | 30 s | how long the daemon waits for any frame after a ping it wrote, then closes |
| `AUTH_DEADLINE` | 30 s | how long the daemon gives a connection to authenticate, then closes |
| `AUTH_REPLY_TIMEOUT` | 10 s | how long a client or hub waits for `AuthResult` |
| `AUTH_REFUSAL_FLUSH` | 1 s | how long the daemon lets a refused `AuthResult` flush before closing |
| `TRANSPORT_HANDSHAKE_TIMEOUT` | 10 s | the longest a TLS or QUIC handshake may take on the daemon's listener |
| `FRAME_WRITE_TIMEOUT` | 30 s | the longest one frame write or flush may take before the daemon closes |
| `QUIC_IDLE_TIMEOUT` | 300 s | QUIC's idle timeout, on both ends |
| `QUIC_KEEP_ALIVE` | 15 s | QUIC keep-alive interval, on both ends |
| `PANE_STREAM_STALL_TIMEOUT` | 330 s | how long one QUIC pane stream may block before it alone is reset (and `Lagged` sent) |
| `PEER_CONNECT_TIMEOUT` | 20 s | one attempt to open or re-open a federation link |
| `PEER_LIST_TIMEOUT` | 10 s | a hub waiting for its peer's session list or client list |
| `PEER_CREATE_TIMEOUT` | 10 s | a hub waiting for its peer to create a session |
| `PEER_OVERVIEW_TIMEOUT` | 2 s | a hub waiting for its peer's process overview |
| `BACKOFF_MIN` | 250 ms | the first delay before re-opening a dropped link (a GUI's or a hub's); doubled per attempt |
| `BACKOFF_MAX` | 15 s | the longest delay between re-open attempts |
| `BACKOFF_JITTER_CAP_PERMILLE` | 200 ‰ | the most jitter takes off a delay |

## Error model

A request fails in one of four ways, each on the control lane:

- **`AuthResult { success: false, failure, reason }`** — the handshake was
  refused; the connection closes. `failure` is an `AuthFailure`:

  | `AuthFailure` | Meaning | Retry? |
  |---|---|---|
  | `ProtocolMismatch { client, daemon }` | the ranges do not overlap | no — upgrade one side (the client shows the hint) |
  | `BadToken` | not this daemon run's token | no — fetch the current token (a local client re-reads it each bootstrap; an SSH one re-negotiates) |
  | `IdentityRejected` | the signature does not verify | no |
  | `Unknown` | a newer daemon's reason | no; show `reason` |

  A client that reconnects on its own stops on any refusal
  (`BootstrapTaskResult::Refused`) rather than retry it.
- **`Error { request_id, code, message }`** — a request failed; the connection
  stays. `request_id` is the request's when it had one, else `None`. A client
  shows `message`; `code` says what kind of failure it was:

  | `ErrorCode` | Sent for | Retry? |
  |---|---|---|
  | `SessionNotFound` | a request naming a session (or a tab of one) that does not exist: `SessionClose`, `SessionRestore`, `SessionRename`, `PaneCreate`, `TabCreate`, `TabClose`, `TabRename`, `TabReorder`, `PaneSplit`, `PaneSwap`, `SetLayoutRatios`, `ApplyLayoutScheme`, `SetFocus`, `ClientList`, `KickClient` | no; for a federated `ClientList` it can also mean the peer did not answer, which may pass |
  | `SessionAlreadyExists` | a session name clash in the PTY registry (not reachable through today's requests) | no |
  | `NotAuthenticated` | any request before the handshake; `AuthProof` with no challenge | after authenticating |
  | `InvalidMessage` | a frame that does not decode (`request_id: None`) | no |
  | `InternalError` | the daemon could not carry the request out | sometimes: a restart in progress (`HandoffInProgress`: create, split, restore), a full input queue, or an unreachable peer pass; a spawn failure, an invalid signal or an unreadable log do not |
  | `InputLocked` | input to a pane another client holds the lock of | once the lock is released |
  | `SessionLimitReached` | `SessionCreate` past the session limit, or with every session word in use | once sessions close |
  | `PaneNotFound` | a request naming a pane that does not exist: the input messages, `Resize`, `Signal`, `Attach`, the lock messages, `PaneClose`, `FetchHistory`, `Notify` | no |
  | `ClientNotFound` | `KickClient` naming a connection not attached to the session | no |
  | `Unknown` | a newer daemon's code; never sent | show `message` |

- **`PeerError { request_id, peer, reason }`** — `OpenPeer` failed. `reason`
  names the cause and, for a refused peer, what to do
  ([architecture-federation.md](architecture-federation.md#re-discovering-a-restarted-peer)).
- **`DirectoryListing { error: Some(..) }`** — `ListDirectory` failed; the
  listing echoes the path it tried.

A daemon that closes a connection (a deadline, a write timeout, an overflowed
control queue) sends nothing first; the client sees the channel end and
reconnects ([connection.md](connection.md#automatic-reconnect-issue-208)).

## Known gaps

What the protocol does today that it should not, recorded so it is not
mistaken for intent:

- **Some requests for a federated session are not forwarded.** `SessionRename`,
  `SessionRestore`, `PaneCreate`, `TabCreate`, `PaneSplit`, `PaneClose`,
  `TabRename`, `TabReorder`, `RequestInputLock` and `ReleaseInputLock` for a
  proxied session are answered by the hub, which does not host it, with
  `SessionNotFound` or `PaneNotFound`.
- **A hub does not relay `GridDigest`**, so a proxied pane's viewers are not
  verified end to end; the hub's own mirror is.
- **A peer's error for a forwarded request with no reply** (`PtyInput`,
  `Signal`, `FetchHistory`) is dropped by the hub, so the client never hears
  of it.
- **`LayoutUpdate` is not sent on attach**: a client learns a tab's layout from
  the session list and the `LayoutUpdate`s that follow it.
