# Protocol Compression

Implements [issue #59](https://github.com/getkono/kmux/issues/59). The daemon
compresses high-volume server→client traffic — shell output, terminal
snapshots, scrollback hydration — which is mostly UTF-8 text plus ANSI escapes
and compresses 3–8×. Compression is **negotiated on the handshake** (default on
for non-local clients, off for local) and works identically across every
transport (QUIC, TCP+TLS, UDS), because it lives in the one shared wire codec.

## Where it lives

`crates/kmux-protocol/src/codec.rs` — the length-prefixed framing shared by all
transports. A single change there covers QUIC, TCP+TLS, and UDS. The separate
`kmuxd::handoff` codec (daemon-to-daemon `SCM_RIGHTS` fd passing) is a different
protocol and is **not** compressed.

## Frame format

Every frame is self-describing:

```text
[u32 big-endian length][u8 codec tag][payload…]
```

- `length` counts the codec tag byte plus the payload.
- `codec tag`: `2` = raw named MessagePack, `3` = zstd-compressed named
  MessagePack. Tags `0` and `1` are permanently reserved for the retired
  Postcard codec and produce an explicit upgrade error.
- Bounds: on-wire `length` ≤ `MAX_FRAME_SIZE` (64 MiB); a zstd frame may not
  inflate past `MAX_DECOMPRESSED_SIZE` (64 MiB) — a decompression-bomb guard.

Because the tag is per-frame (the HTTP `Content-Encoding`-per-message analogue),
`read_frame` decompresses purely from the tag with **no per-connection state**.
That has two nice consequences:

1. The negotiation switch-over after auth needs no coordination between the
   reader and writer tasks — even a compressed `AuthResult` decodes fine.
2. It is robust across QUIC↔TCP transport swaps and `Lagged`/`SyncReset` gap
   recovery: every frame stands alone.

The writer only compresses a frame when the connection has compression enabled,
the payload is ≥ `min_size`, **and** the result is actually smaller — otherwise
it emits a raw frame, so a frame never grows beyond `payload + 1` byte.

### Codec API

| Function | Role |
|---|---|
| `write_frame(w, data)` | Always-raw frame. Handshake, `ls`, and any unnegotiated path. |
| `write_frame_compressed(w, data, Compressor)` | Compress-if-beneficial; returns wire bytes written. Used by the daemon's writer + QUIC pane writer. |
| `read_frame(r)` | Reads any frame, decompressing transparently. Signature unchanged, so all readers are codec-agnostic. |

## Negotiation

The client offers the `frame.zstd` named capability and the daemon returns the
supported intersection. The daemon then chooses the per-connection *policy*.
Mapped to HTTP:

| HTTP | kmux |
|---|---|
| `Accept-Encoding` (client offers) | `Auth.protocol_capabilities` contains `frame.zstd` |
| `Content-Encoding` (server decides) | `ServerMessage::AuthResult.compression: Option<Compression>` |
| per-message `Content-Encoding` | the per-frame codec tag |

The daemon is authoritative. On successful auth (`kmuxd`'s
`client_handler/dispatch.rs`) it computes `compression.enabled_for(transport)`,
flips the connection's shared `OutboundCompression` toggle that the writer and
pane-attacher tasks read, and echoes the choice in `AuthResult.compression`. The
client uses the response for observability; its `read_frame` handles either
current codec tag on every frame. See
[Data-Plane Protocol Versioning](architecture-protocol-versioning.md).

**Direction (v1): downlink only** (server→client). That is the shell output #59
targets, and self-describing frames mean the client needs zero writer changes.
Client→server (mainly large pastes) is a deferred follow-up.

## Configuration (`[compression]` in `kmuxd.toml`)

```toml
[compression]
mode = "auto"    # auto | always | never   (default: auto)
level = 3        # zstd level, sender-side only (default: 3)
min_size = 256   # don't compress frames smaller than this (default: 256)
```

- **`auto`** (default): compress every networked transport; leave local **UDS**
  clients uncompressed (bandwidth is free same-host, so compressing wastes CPU).
  This is the issue's "default on if the client is not local".
- **`always`** / **`never`**: force the decision regardless of locality.

`level` and `min_size` are sender-side only — the decompressor reconstructs the
level from the zstd frame, so neither appears on the wire.

> Locality is currently `transport == UDS`. SSH-tunnelled and same-host
> non-UDS clients (loopback QUIC/TCP) are treated as remote; refining locality
> by peer address is a noted follow-up.

## Algorithm choice

**zstd, level 3.** On modern x86-64 / Apple Silicon zstd gives the best
ratio/latency balance for terminal output: 3–8× on text at sub-millisecond
per-frame compression and >1 GB/s decompression. It beats lz4 on ratio with a
negligible speed difference at these small frame sizes, and beats zlib on both.
Adding lz4 or a dictionary variant later requires a distinct named capability
and permanent codec tag before either side may send it.

## Strategy — to finalize (empirical)

v1 ships **strategy A**; the optimum is best chosen from real traces. The
candidates:

- **A. Stateless per-frame zstd (shipped).** Simplest; swap/gap-safe. Compresses
  bulk output well. Small single-cell diffs (~tens of bytes, below `min_size`)
  don't compress, but they're a negligible share of bytes.
- **B. Stateless per-frame + embedded static dictionary.** A `zstd --train`ed
  dictionary of common ANSI/prompt/cell patterns, shipped on both sides and
  version-locked, primes each frame so small diffs also compress. Best stateless
  ratio; adds a training/embedding/versioning pipeline.
- **C. Streaming zstd context.** Best raw ratio (cross-frame redundancy) but the
  context cannot survive a transport swap and desyncs on any dropped/lagged
  frame — it fights kmux's resumption + gap-recovery design. Not recommended.

### How to finalize

1. Capture a long real session's server→client frames:
   ```sh
   KMUX_CAPTURE_FRAMES=/tmp/frames.bin cargo run -p kmuxd
   # attach a client; run `cat biglog`, a build, vim, tmux-in-tmux, etc.; quit.
   ```
   The env-gated tap (`kmuxd::capture`) appends each frame's pre-compression
   payload as `[u8 category][u32 len][payload]`. It is a no-op when unset.
2. Replay it offline:
   ```sh
   cargo run -p kmux-protocol --example compression_bench -- /tmp/frames.bin
   ```
   This reports, per zstd level, the resulting wire size, compression ratio, and
   throughput, plus a per-category byte breakdown (so it is obvious that `Shell`
   dominates). Use the numbers to revisit A vs B and the level.

## Tests

- `codec.rs`: raw/zstd roundtrip, shrink-and-roundtrip, below-`min_size` stays
  raw, incompressible falls back to raw, unknown-tag rejection, and a concurrent
  duplex-pipe roundtrip of a mixed message batch.
- `kmuxd/config.rs`: `enabled_for` matrix (auto/always/never × transports).
- `kmuxd/client_handler/dispatch.rs`: auth negotiates zstd on a networked
  transport and leaves UDS uncompressed under `auto`.
