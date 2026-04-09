# Performance Optimization Roadmap

Architectural optimizations to make kmux competitive with ssh+tmux in
efficiency, ordered from highest to lowest estimated impact.

---

## 1. Server-Side VT Parsing + Diff-Based Output

**Current behavior:** The server reads raw PTY bytes in
`relay.rs:session_read_loop` (line 30) and fans them out verbatim to every
attached client via `ServerMessage::PtyOutput`. Each client independently
parses the same VT stream through `alacritty_terminal` in
`terminal_view.rs:TerminalBuffer::push_bytes` (line 67-69).

**Why it's slow:** When multiple clients are attached to the same session,
every client receives and parses the full raw byte stream. A single `cat
largefile.txt` sends the entire file contents N times. The protocol carries
redundant data -- escape sequences that move the cursor, set colors, and
erase regions are retransmitted even though only a handful of cells
actually changed on screen.

**Proposed change:** Run `alacritty_terminal::Term` on the server per
session. After each PTY read, diff the grid against the previous state and
send only changed cells as a `CellDiff` message (row, col, char, fg, bg,
flags). For bulk output (>50% of cells changed), fall back to a
full-grid snapshot. Clients apply diffs directly to their local grid
without VT parsing.

**Estimated effort:** Large. Requires a new `ServerMessage::CellDiff`
variant in `kmux-protocol`, server-side `Term` instances in `app.rs` /
`relay.rs`, a diff algorithm, and client-side diff application logic. The
existing `TerminalBuffer` on the client would become a thin grid store
rather than a full VT parser.

**Relevant code:**
- `crates/kmuxd/src/relay.rs:29-45` -- raw byte read + fan-out
- `crates/kmux-client/src/terminal_view.rs:67-69` -- `push_bytes()` VT parsing
- `crates/kmux-protocol/src/messages.rs:203-207` -- `PtyOutput { data: Vec<u8> }`

---

## 2. Streaming Compression (zstd/LZ4)

**Current behavior:** PTY output bytes are sent uncompressed. Each
`ServerMessage::PtyOutput` is postcard-encoded (`frame.rs:23-24`) and sent
as a raw WebSocket binary frame. The `data: Vec<u8>` field in
`messages.rs:205` carries verbatim terminal output.

**Why it's slow:** Terminal output is highly compressible -- repeated
whitespace, ANSI escape sequences, and structured text (logs, code) see
3-10x compression ratios. SSH achieves this with optional zlib compression.
kmux sends every byte uncompressed, wasting bandwidth especially over
WAN links.

**Proposed change:** Add per-connection streaming compression negotiated
during auth. The `ClientMessage::Auth` message would include a
`compression: Option<CompressionAlgo>` field. When agreed, both sides wrap
their WebSocket streams in a `zstd::stream::Encoder` /
`zstd::stream::Decoder` (or LZ4 for lower latency). Compression applies
to the postcard-encoded bytes before WS framing.

**Estimated effort:** Medium. Requires a new dependency (`zstd` or `lz4`),
negotiation logic in `connection.rs:57-83` (auth handler), and wrapping the
sink/stream in `writer_loop` (line 354-368) and the client's `connect.rs`.

**Relevant code:**
- `crates/kmux-protocol/src/frame.rs:23-24` -- `encode_server` / `to_allocvec`
- `crates/kmuxd/src/connection.rs:354-368` -- `writer_loop` sends raw bytes
- `crates/kmux-protocol/src/messages.rs:106-110` -- `Auth` message (negotiation point)

---

## 3. Server-Side Output Coalescing

**Current behavior:** In `relay.rs:29-45`, each successful `reader.read()`
immediately iterates over all clients and sends the chunk. The 4 KiB read
buffer (line 27) means the relay loop wakes, allocates, clones, and sends
on every kernel-level PTY read completion -- potentially thousands of times
per second during high-throughput output (`cat`, `make`, etc.).

**Why it's slow:** Each iteration locks the `ClientMap` mutex (line 51),
clones the `ServerMessage` per client (line 53), and sends through an mpsc
channel. The per-message overhead (lock acquire, postcard encode, WS frame,
TLS record) dominates at high message rates. SSH coalesces internally
because its TCP send buffer naturally batches.

**Proposed change:** Buffer PTY reads for a configurable coalescing window
(2-16ms). Use `tokio::time::sleep` or `tokio::select!` with a deadline
to accumulate bytes into a larger buffer before fanning out. Reset the
timer on each read; flush when the buffer hits a size threshold (e.g.
32 KiB) or the timer expires.

**Estimated effort:** Small. Localized to `relay.rs:session_read_loop` --
add a `BytesMut` accumulator and a `tokio::time::Instant` deadline. No
protocol changes required.

**Relevant code:**
- `crates/kmuxd/src/relay.rs:27` -- `buf = vec![0u8; 4096]`
- `crates/kmuxd/src/relay.rs:29-45` -- immediate fan-out loop
- `crates/kmuxd/src/relay.rs:51-53` -- per-message mutex lock + clone

---

## 4. WebSocket Frame Batching

**Current behavior:** The `writer_loop` in `connection.rs:354-368` calls
`rx.recv().await` and sends each `ServerMessage` as an individual WebSocket
binary frame (line 361). Each frame requires a separate `sink.send()` call,
which flushes the underlying TLS stream.

**Why it's slow:** Each `sink.send()` triggers a TLS record write and
potentially a TCP `write()` syscall. During bursts (e.g. scrollback replay
at lines 162-191), hundreds of messages are queued in the unbounded channel
but drained one at a time. This creates excessive syscall and TLS record
overhead -- one TLS record per ~4 KiB message instead of batching multiple
messages into a single ~16 KiB record.

**Proposed change:** After `recv().await` returns, drain all remaining
messages from the channel with `try_recv()`, encode them all, and
concatenate into a single WebSocket frame (or use `sink.send_all` with a
`feed`/`flush` pattern). This batches multiple postcard-encoded messages
into fewer TLS records.

**Estimated effort:** Small. Only `connection.rs:writer_loop` changes.
Define a length-prefix framing within a single WS binary frame so the
client can split batched messages. Alternatively, use `sink.feed()` +
`sink.flush()` to let tungstenite batch at the WS layer.

**Relevant code:**
- `crates/kmuxd/src/connection.rs:354-368` -- `writer_loop`
- `crates/kmuxd/src/connection.rs:358-363` -- single-message recv + send
- `crates/kmuxd/src/connection.rs:160-191` -- scrollback replay burst

---

## 5. Zero-Copy with `bytes::Bytes`

**Current behavior:** PTY output flows through several clone points:
1. `relay.rs:33` -- `buf[..n].to_vec()` (first allocation)
2. `relay.rs:38` -- `chunk.clone()` for scrollback
3. `relay.rs:53` -- `msg.clone()` per client (clones the inner `Vec<u8>`)
4. `frame.rs:23-24` -- `postcard::to_allocvec()` serializes into a new `Vec`

Each 4 KiB PTY read results in at least 3 full copies of the data before
it reaches the WebSocket.

**Why it's slow:** Memory allocation and copying dominate at high
throughput. With 3 clients attached, a single read produces ~5 copies of
the data (1 original + 1 scrollback + 3 client clones), totaling ~20 KiB
of allocation for a 4 KiB read.

**Proposed change:** Replace `Vec<u8>` with `bytes::Bytes` throughout the
output pipeline. `Bytes` uses reference counting internally -- cloning is
O(1). Change `ServerMessage::PtyOutput::data` from `Vec<u8>` to `Bytes`.
Use `BytesMut` for the read buffer in `relay.rs` and `freeze()` before
fan-out. Serialization still requires a copy, but the fan-out clones become
free.

**Estimated effort:** Medium. Requires adding `bytes` as a dependency to
`kmux-protocol`, changing the `data` field type in `messages.rs:205`,
updating `relay.rs`, and ensuring `postcard` can serialize `Bytes` (may
need a `serde_bytes` attribute or custom serialization).

**Relevant code:**
- `crates/kmuxd/src/relay.rs:33` -- `to_vec()` allocation
- `crates/kmuxd/src/relay.rs:38` -- `chunk.clone()` for scrollback
- `crates/kmuxd/src/relay.rs:53` -- `msg.clone()` per client
- `crates/kmux-protocol/src/messages.rs:205` -- `data: Vec<u8>`

---

## 6. Background VT Parsing on Client

**Current behavior:** When `ServerMessage::PtyOutput` arrives, `app.rs`
handles it synchronously in `handle_server_message` (lines 409-413):

```rust
ServerMessage::PtyOutput { session, data, .. } => {
    if let Some(buf) = self.buffers.get_mut(&session) {
        buf.push_bytes(&data);
    }
    Task::none()
}
```

`push_bytes` calls `self.processor.advance(&mut self.term, data)` (
`terminal_view.rs:68`) which runs the full VTE state machine inline on the
iced event loop thread.

**Why it's slow:** VT parsing for large output bursts (e.g. `cat` a 1 MB
file) blocks the iced event loop, freezing the UI. The `advance()` call
processes every byte sequentially -- escape sequence parsing, grid cell
updates, scrolling -- all on the main thread.

**Proposed change:** Move `TerminalBuffer` into a background
`tokio::task::spawn_blocking` or a dedicated thread. The iced subscription
feeds raw bytes into a channel; the background thread runs `push_bytes()`
and signals the UI to re-render via generation counter changes. The
snapshot (`TerminalSnapshot::from_buffer`) would read from the `Term` under
a `Mutex` or use a double-buffer swap.

**Estimated effort:** Medium. Requires restructuring `TerminalBuffer` to be
`Send + Sync` (wrap `Term` in `Arc<Mutex<>>>`), a channel for incoming
bytes, and a mechanism to notify iced of updates (e.g. `iced::Command`
that polls the generation counter).

**Relevant code:**
- `crates/kmux-client/src/app.rs:409-413` -- synchronous `push_bytes` call
- `crates/kmux-client/src/terminal_view.rs:67-69` -- `push_bytes` + `advance`
- `crates/kmux-client/src/terminal_view.rs:130-181` -- `from_buffer` snapshot

---

## 7. Incremental Snapshots

**Current behavior:** Every frame, `TerminalSnapshot::from_buffer()`
(terminal_view.rs:130-181) copies the entire grid: it pre-allocates
`rows * cols` `SnapshotCell` structs (line 138-145), then iterates
`display_iter` to fill in each cell (line 147-171). For an 80x24 terminal,
this is 1,920 cells; for a fullscreen 200x50 terminal, 10,000 cells.

The canvas cache (`CanvasState::cache`) avoids re-drawing when the
generation hasn't changed (line 244-247), but the snapshot copy itself
happens unconditionally on every `view()` call (line 374).

**Why it's slow:** Most frames, only a few cells change (cursor blink,
single character typed). Copying the entire grid every frame wastes CPU and
creates GC pressure from the short-lived `Vec<SnapshotCell>`.

**Proposed change:** Track dirty rows in `TerminalBuffer` using a
`BitVec<u64>` or similar. `from_buffer()` reuses the previous snapshot and
only copies rows marked dirty. The generation counter (already in place at
`terminal_view.rs:48`) provides the invalidation signal; dirty-row tracking
provides the granularity.

**Estimated effort:** Small-Medium. Add a `dirty_rows: BitVec` to
`TerminalBuffer`, set bits in `push_bytes()` based on cursor position /
scroll regions, and update `from_buffer()` to do incremental copies. The
tricky part is hooking into alacritty_terminal's internal cursor tracking
to know which rows were touched.

**Relevant code:**
- `crates/kmux-client/src/terminal_view.rs:130-181` -- `from_buffer()` full copy
- `crates/kmux-client/src/terminal_view.rs:138-145` -- full grid pre-allocation
- `crates/kmux-client/src/terminal_view.rs:244-247` -- cache invalidation check
- `crates/kmux-client/src/terminal_view.rs:374` -- unconditional snapshot in `view()`

---

## 8. Raw TLS Transport (Optional)

**Current behavior:** All communication uses WebSocket over TLS
(`tokio-tungstenite` + `tokio-rustls`). Each message goes through: postcard
encode -> WS binary frame (2-14 byte header + masking on client->server) ->
TLS record (5 byte header + ~16 byte AEAD tag) -> TCP.

**Why it's slow:** WebSocket adds framing overhead: a 2-14 byte header per
frame, mandatory client->server masking (XOR of every byte), and
per-frame processing in tungstenite. For the high-frequency, small-message
pattern of terminal I/O (many 1-100 byte messages), the relative overhead
is significant. SSH uses raw TLS with a simple length-prefix and avoids
all of this.

**Proposed change:** Offer an alternative transport mode: raw TLS with
4-byte length-prefixed postcard frames. Negotiate during connection setup
(e.g. HTTP upgrade header or a TLS ALPN extension). Keep WebSocket as the
default for browser compatibility (future web client); use raw TLS for the
native client.

**Estimated effort:** Large. Requires a parallel listener in `main.rs`,
a new framed codec (e.g. `tokio_util::codec::LengthDelimitedCodec`), and
client-side transport selection. The payoff is modest compared to the
other optimizations -- most overhead comes from the items above, not WS
framing.

**Relevant code:**
- `crates/kmuxd/src/connection.rs:19` -- `WsStream` type alias
- `crates/kmuxd/src/connection.rs:288-289` -- WebSocket split
- `crates/kmuxd/src/connection.rs:354-368` -- `writer_loop` WS send
- `crates/kmuxd/src/tls.rs` -- TLS acceptor setup

---

## Summary Matrix

| #   | Optimization           | Bandwidth | Latency | CPU | Effort    | Multi-client |
| --- | ---------------------- | --------- | ------- | --- | --------- | ------------ |
| 1   | Server-side VT + diffs | +++       | +       | ++  | Large     | +++          |
| 2   | Streaming compression  | +++       | -       | -   | Medium    | ++           |
| 3   | Output coalescing      | +         | -       | ++  | Small     | ++           |
| 4   | WS frame batching      | +         | +       | ++  | Small     | +            |
| 5   | Zero-copy Bytes        | --        | +       | ++  | Medium    | ++           |
| 6   | Background VT parsing  | --        | ++      | +   | Medium    | --           |
| 7   | Incremental snapshots  | --        | ++      | ++  | Small-Med | --           |
| 8   | Raw TLS transport      | +         | +       | +   | Large     | +            |

Legend: `+++` major improvement, `++` moderate, `+` minor, `--` no change, `-` minor regression (e.g. compression adds CPU)
