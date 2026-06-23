# Cryptographic identity & client management (issue #146)

kmux gives every participant — each client process and the daemon itself — a
stable, **cryptographically verifiable identity**, and exposes endpoints to list
the clients attached to a session and to **kick** one out of it. This lets a user
see *who else* is connected to a shared session and remove a connection that is,
say, shrinking everyone's view with a small window.

## Two identity levels

A connection carries two distinct identifiers:

1. **Machine id** — an **Ed25519 keypair**, one per `user@machine`, persisted as
   PKCS#8 (mode 0600) at `$XDG_CONFIG_HOME/kmux/identity.key` and generated lazily
   on first use (`kmux_protocol::identity::Identity::load_or_create`). The id is
   the hex **SHA-256 fingerprint of the public key**. It is shared by the daemon
   and all of that user's client processes on the machine; multiple connections
   from one machine share it. Collision resistance of the hash plus
   proof-of-possession (below) make it unforgeable — no two entities can claim the
   same id. The key lives in the *config* dir (like the TOFU store and TLS certs),
   so it is shared across the debug/release profiles.

2. **Per-connection label** — a user-readable name assigned by the daemon hosting
   the session: `username@hostname`, with a `#N` suffix to disambiguate when the
   same `user@host` has multiple live connections (e.g. `alice@macbook`,
   `alice@macbook#2`). This is the unit listed and kicked.

The shared **auth token still gates access** to the daemon. Identity is layered on
top purely for attribution and management.

## Handshake (challenge–response)

Presenting a public key proves nothing — anyone could paste another party's key.
So the daemon verifies **proof-of-possession**:

```
client                                  daemon
  │  Auth { token, protocol_version,       │
  │         public_key, hostname,          │
  │         username }                      │
  │ ─────────────────────────────────────► │  validate token + protocol
  │                                         │  (reject → AuthResult{success:false})
  │  AuthChallenge { nonce }                │  random 32-byte nonce
  │ ◄───────────────────────────────────── │
  │  AuthProof { signature }                │  sign(nonce) with identity key
  │ ─────────────────────────────────────► │  verify(public_key, nonce, signature)
  │  AuthResult { machine_id, label,        │  register connection, assign label
  │               server_machine_id, … }    │
  │ ◄───────────────────────────────────── │
```

The fresh per-connection nonce defeats replay. On the client side the handshake is
centralized: `kmux-connect`'s `send_auth_frame` builds the `Auth` frame and
`answer_auth_challenge` signs the nonce; the bootstrap intercept (and the
headless `kmux ls` / `ps` / `clients` / `kick` subcommands, via a shared
`authenticate` helper) drive it. The daemon, when it federates to a peer, presents
**its own** identity over the same path, so a peer's client list shows the hub as
one distinct entity.

## Listing & kicking

New wire messages (`PROTOCOL_VERSION` 32):

- `ClientList { word_id }` → `ClientListResult { clients: Vec<ClientInfo> }` —
  the connections attached to a session, each with `machine_id`, `label`,
  `hostname`, `transport`, and the panes it is viewing. `is_self` flags the
  requester.
- `KickClient { word_id, client_id }` → `ClientKicked` — detaches that one
  connection from every pane of the session (clearing any input lock and
  reconciling the smallest-wins size) and pushes `SessionKicked { by_label }` to
  the target so its UI leaves the session. The target's connection stays alive;
  other connections — including others from the same machine — are untouched.
- Errors use `ErrorCode::SessionNotFound` / `ClientNotFound`.

**Authorization:** any token-authenticated client may kick any other; identity is
attribution/display only.

## Federation

For a session proxied from a federated peer (issue #121), the local hub forwards
`ClientList` / `KickClient` to the **owning peer** and relays the reply, reusing
the same request/await-reply plumbing as `create_remote_session`
(`PeerConnection::pending_client_lists` / `pending_kicks`, completed by the feed
loop). The peer's labels/ids/machine ids are relayed verbatim — they are
meaningful on that host, and `machine_id` is globally unique. The dispatch layer
chooses local vs. forwarded via `ServerApp::is_federated_session`.

## Surfaces

| Surface | List | Kick |
| --- | --- | --- |
| CLI | `kmux clients [<session>]` (`--format json`) | `kmux kick <session> <label-or-id>` |
| Control socket | `kmux daemon sessions` shows label/machine id/hostname | — |
| GTK | "Connected Clients" main-area view (`Ctrl+Shift+K`, menu, `/clients`) | per-row **Kick** button |
| Swift | "Connected Clients" main-area view (`⌘⇧K`, menu) | per-row **Kick** button |

All three GUI/CLI surfaces render the same data: the daemon's `ClientInfo`,
surfaced to the GUIs through `AppCore::client_rows` (and `kmux-ffi`'s
`FfiClientRow`) and to the CLI by the headless `subcommands::clients` path.

## Key files

- Identity primitives: `crates/kmux-protocol/src/identity.rs` (feature `identity`),
  `dirs::identity_key_path`.
- Wire: `crates/kmux-protocol/src/messages/{client,server,session,types}.rs`.
- Daemon: `crates/kmuxd/src/client_handler/dispatch.rs` (handshake + dispatch),
  `crates/kmuxd/src/app/{mod.rs,clients.rs,peer_api.rs}`,
  `crates/kmuxd/src/federation/mod.rs`.
- Client: `crates/kmux-connect/src/{tcp_connect,pipeline,supervisor}.rs`,
  `crates/kmux-client/src/session_manager/{connection,server_handler}.rs`.
- App + GUI: `crates/kmux-app/src/{mode,core,driver}`,
  `crates/kmux-gtk/src/imp/clients.rs`, `crates/kmux-ffi/src/lib.rs`,
  `kmux-swift/Sources/KmuxApp/ConnectedClientsView.swift`.
