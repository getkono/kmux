# Data-Plane Protocol Versioning

The kmux client↔daemon data plane is designed so ordinary feature additions do
not force every installed binary to upgrade in lockstep. It combines a named
MessagePack schema, a semantic supported-version range, and explicit named
capabilities. Authentication negotiates both before either side sends normal
session traffic.

This contract applies only to `ClientMessage` and `ServerMessage`. Persistence,
daemon handoff, the daemon↔VT-worker protocol, and C/Swift ABIs keep their own
formats and version gates.

## Wire format

Every transport uses the same frame envelope:

```text
[u32 big-endian length][u8 codec tag][payload…]
```

The payload is MessagePack encoded with named struct fields. The top-level
message enums use Serde's adjacent representation: a `type` field names the
variant and a `data` field contains its payload. This avoids positional coupling
between struct fields and makes additive evolution possible.

Codec tags are permanent assignments:

| Tag | Meaning |
|---:|---|
| `0` | Legacy raw Postcard; rejected with an upgrade error |
| `1` | Legacy zstd-compressed Postcard; rejected with an upgrade error |
| `2` | Raw named MessagePack |
| `3` | zstd-compressed named MessagePack |

Tags `0` and `1` are never reused. A new decoder therefore cannot mistake an
old positional payload for a current named payload.

## Version and capability negotiation

Each binary advertises an inclusive `ProtocolRange { min, max }`. Versions are
semantic triples:

- A major version changes only for an incompatible schema redesign.
- A minor version establishes a new compatible baseline.
- A patch version changes no schema semantics.

The negotiated version is the highest version in the overlap. Ranges from
different major versions never overlap. The current baseline is
`1.0.0..=1.0.0`; normal additive features do not edit it.

Optional features use stable string capabilities instead. The client offers
capabilities in `Auth`, and the daemon returns the supported intersection in
`AuthResult`. A sender must not emit a capability-gated message or codec until
the peer accepted that capability. The initial capability is `frame.zstd`.
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

## Schema evolution rules

Compatible changes:

- Add a named struct field with `#[serde(default)]` on receivers that may read
  messages from older senders.
- Add optional output metadata that older named-map readers can ignore.
- Add a new message or behavior behind a named negotiated capability.
- Add a capability without changing `PROTOCOL_VERSION` or
  `MIN_PROTOCOL_VERSION`.

Incompatible changes:

- Rename or remove a field or message variant.
- Change a field's meaning or type incompatibly.
- Reuse a codec tag or capability name with different semantics.
- Send a new variant without first negotiating its capability.

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
   fixtures where relevant.
5. Update this document and any feature-specific architecture document.

Do not change the worker, persistence, handoff, or FFI contract version unless
that separate contract actually changed.
