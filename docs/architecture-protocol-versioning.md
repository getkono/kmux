# Data-Plane Protocol Versioning

The kmux client↔daemon data plane is designed so ordinary feature additions do
not force every installed binary to upgrade in lockstep. It combines a named
MessagePack schema, a semantic supported-version range, and explicit named
capabilities. Authentication negotiates both before either side sends normal
session traffic.

This document is how the schema *changes*; what it *is* is specified in
[protocol.md](protocol.md). This contract applies only to `ClientMessage` and
`ServerMessage`. Persistence,
daemon handoff, the daemon↔VT-worker protocol, and C/Swift ABIs keep their own
formats and version gates.

## Wire format

Every transport carries the same frames — a length prefix, a codec tag and a
named-MessagePack payload — specified in [protocol.md](protocol.md#frames),
with the message catalogue, state machines and error model. Codec tags are
permanent assignments: the retired Postcard tags `0` and `1` are never reused,
so a new decoder cannot mistake an old positional payload for a named one.

## Version and capability negotiation

Each binary advertises an inclusive `ProtocolRange { min, max }`. Versions are
semantic triples:

- A major version changes only for an incompatible schema redesign.
- A minor version establishes a new compatible baseline.
- A patch version changes no schema semantics.

The negotiated version is the highest version in the overlap. Ranges from
different major versions never overlap. A build speaks `1.0.0..=1.1.0`; normal
additive features do not edit that range.

| Version | Baseline it establishes |
|---|---|
| `1.0.0` | The named-MessagePack schema. |
| `1.1.0` | Every [lenient enum](#unknown-variants) decodes a variant it does not know to `Unknown` instead of failing the frame, and a refused `AuthResult` carries a typed `failure` (`AuthFailure`) beside its text `reason`. |

A `1.1.0` build still accepts a `1.0.0` peer (`MIN_PROTOCOL_VERSION` is
`1.0.0`); the pair negotiates `1.0.0`, and the `1.1.0` side must not send such a
peer a variant it does not have.

Optional features use stable string capabilities instead. The client offers
capabilities in `Auth`, and the daemon returns the supported intersection in
`AuthResult`. A sender must not emit a capability-gated message or codec until
the peer accepted that capability. The initial capability is `frame.zstd`.
`session.closed.peer` (issue #228) is the client's promise that it reads
`ClosedSessionEntry.peer` and names it in `SessionRestore`: a hub sends a
federated peer's closed sessions only to a client that negotiated it, since
one that ignores `peer` would restore the peer's word from the hub's own
graveyard. A reply-shaping capability like this one is how a field whose
meaning an older reader would get wrong is added without a range bump.
Unknown capabilities are ignored, not treated as an authentication failure.

Authentication follows this order:

1. Negotiate the protocol range. Reject a disjoint or missing legacy range.
2. Intersect named capabilities.
3. Validate the shared token and cryptographic identity proof.
4. Return the negotiated version and capabilities in the successful
   `AuthResult`.

The SSH `probe-or-start` and local control status paths expose
`protocol_range` so incompatible peers fail before opening the data plane. They
also retain a frozen integer `protocol_version = 41` field for old JSON
consumers. Current code never uses that integer to claim compatibility; a peer
that reports only the integer is rejected as a legacy Postcard peer.

## Unknown variants

A nested enum a newer peer may extend is *lenient*: it has an `Unknown`
variant, and the `wire_enum!` decoder in `kmux-protocol`
(`messages/wire_enum.rs`) turns any variant name this build does not know —
unit or carrying data — into it, skipping the payload, so the frame around it
still decodes. A known name with a malformed payload is still an error.

| Enum | Where it travels | What a receiver does with `Unknown` |
|---|---|---|
| `ErrorCode` | `ServerMessage::Error` | Reads the error's `message`, as for any code |
| `AuthFailure` | `AuthResult.failure` | Treats it as a refusal without a hint; shows `reason` |
| `Compression` | `AuthResult.compression` | Nothing: the field is informational, frames self-describe |
| `SessionEventMsg` | `ServerMessage::Event` | Ignores the event; a hub does not relay it |
| `PaneProgressState` | `PaneInfo`, `PaneProgressChanged` | Treats it as `Remove`: no progress bar |
| `AttentionKind` | `Notify`, `PaneAttention` | Treats it as `TurnDone`, the plain request for attention |
| `FrontendKind` | `Auth.client_kind`, `ClientInfo.frontend` | Shows `unknown`; reports it as `Cli` |
| `PeerTarget` | `OpenPeer.target` | The daemon answers `PeerError` |
| `LayoutScheme` | `ApplyLayoutScheme.scheme` | The daemon ignores the request |

**A build never sends `Unknown`.** Where it passes on a value it received as
`Unknown`, it sends the known value that one is treated as (`sendable()` on
`AttentionKind`, `PaneProgressState` and `FrontendKind`): a daemon broadcasting
a newer client's `Notify`, reporting its `client_kind`, or a hub relaying a
peer's events, session list and client list. A hub drops an unknown event, and a
daemon neither applies nor forwards an unknown layout scheme. So a `1.1.0` build
never puts a value on the wire a `1.0.0` peer cannot decode.

The other nested enums stay strict, because an older build cannot act
correctly on a value it does not understand, or because the enum also crosses a
Postcard boundary, which has no self-describing form to skip: `DiffOp`,
`CursorShape` and `KeyCode`/`KeyAction` (the VT-worker contract), `LayoutNode`,
`SplitDir` and `SessionStatus` (persisted daemon state). A new variant of one of
those needs a capability, or a new major version.

`MessageCategory` has serde derives only for the client's local metrics log;
it is not on the wire.

## Schema evolution rules

Compatible changes:

- Add a named struct field with `#[serde(default)]` on receivers that may read
  messages from older senders.
- Add optional output metadata that older named-map readers can ignore.
  Example: `SessionEntry::peer_unreachable` (issue #208), a `#[serde(default)]`
  flag an older client ignores and an older daemon never sets; and an
  unsolicited `SessionListResult` with `request_id = RESYNC_REQUEST_ID`, an
  existing variant every client already accepts; and `AuthResult.daemon_instance`
  with `Auth.resume_instance` (issue #209), which a daemon that predates them
  never sends and never reads — it resumes on the `connection_id` alone, as
  before. None of them needed a range bump or a capability.
- Add a new message or behavior behind a named negotiated capability.
- Add a capability without changing `PROTOCOL_VERSION` or
  `MIN_PROTOCOL_VERSION`.
- Add a variant to a [lenient enum](#unknown-variants), sent only to a peer that
  negotiated `1.1.0` or later.
- Remove a variant no build has ever sent. `ErrorCode::AuthFailed` and
  `InputDisabled` and `SessionEventMsg::LayoutChanged` went this way in `1.1.0`:
  every receiver already handled them, and no sender produced them.

Incompatible changes:

- Rename or remove a field or message variant.
- Change a field's meaning or type incompatibly.
- Reuse a codec tag or capability name with different semantics.
- Send a new variant of a strict enum without first negotiating its
  capability, or of a lenient one to a peer below `1.1.0`.

An incompatible redesign requires a new major schema and an intentional range
policy. Do not bump the range merely to force two builds to match: build SHA and
profile diagnostics already report build skew separately from wire
compatibility.

## Failure and security behavior

Range rejection happens before token validation, so incompatible decoders do
not continue into privileged application traffic. Frame size and decompression
limits still apply before MessagePack decoding. Unknown frame tags, reserved
legacy tags, malformed MessagePack, and unnegotiated features fail closed.

Cryptographic identity and the shared token are authentication layers, not
protocol-version substitutes. A compatible schema does not make a peer trusted;
the normal nonce/signature proof and token checks remain mandatory.

## History: the retired integer scheme

Before this design the data plane used a single monotonically increasing
`PROTOCOL_VERSION: u32` over a positional Postcard codec, matched exactly on both
sides. Because Postcard is positional, *any* field addition was a wire break, so
every feature bumped the integer and every bump forced client and daemon to
upgrade together. That integer ran from `1` to `40`; feature documents written in
that era still cite it (for example "added in `PROTOCOL_VERSION` 28"), and those
references should be read as historical markers, not as anything a current build
negotiates.

`LEGACY_PROTOCOL_VERSION = 41` is the frozen successor value. It is never used
for compatibility decisions — it exists only so JSON status consumers written
against the old integer field keep parsing, and so a protocol-40 peer sees a
value it recognises as "newer than me" and refuses rather than misreading a
named-map frame as a positional one.

## Contributor checklist

When changing a data-plane message:

1. Decide whether the change is an optional feature, a defaulted field, or a
   genuinely incompatible redesign.
2. Use named fields and add `#[serde(default)]` where older senders may omit a
   field.
3. Add and negotiate a stable capability before sending new variants or using a
   new codec.
4. Add compatibility tests in `kmux-protocol`, including older/future schema
   fixtures where relevant. A new lenient enum gets `#[serde(remote = "Self")]`,
   a `#[serde(other)] Unknown` variant, `wire_enum!`, a fixture in
   `messages/wire_enum.rs` that decodes a newer peer's message carrying an
   unknown variant, and a row in the table above.
5. Update this document and any feature-specific architecture document.

Do not change the worker, persistence, handoff, or FFI contract version unless
that separate contract actually changed.
