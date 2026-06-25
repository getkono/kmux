# Performance of the GUI ↔ daemon pipeline

How kmux keeps the client↔daemon link competitive with ssh+tmux, what has
shipped, and what remains. For transport selection and latency scoring see
[connection.md](connection.md); for the correctness machinery that lets these
optimizations be made safely see [architecture-verification.md](architecture-verification.md).

The local GUI↔daemon path already uses the lowest-overhead options available: a
**Unix domain socket** (`TransportKind::Uds`, no TLS, OS-enforced 0600), a compact
**postcard** binary wire format framed as `[u32 len][u8 codec tag][payload]`, and
**server-side VT parsing** so only changed cells travel. The remaining work is
CPU/scheduling, not transport or format.

## Shipped

- **Server-side VT parsing + cell diffs.** The daemon runs the terminal emulator
  per pane (`kmux-vt-core`), diffs the grid each frame, and broadcasts
  `TerminalUpdate` (changed cells) / `TerminalSnapshot` / `ScrollbackAppend` —
  never raw PTY bytes. Clients apply diffs to a thin `CellGrid` with no VT parser.
  `TerminalUpdate.diff` is `Arc`-shared so per-client fan-out is O(1)
  (`crates/kmuxd/src/relay.rs`).
- **Output coalescing.** The relay drains all immediately-available PTY output
  before computing one diff, so a burst (vim exit, `cat`) yields a single diff
  (`relay.rs:session_diff_loop`).
- **Per-frame zstd compression** (issue #59), negotiated on the handshake, self-
  describing via the codec tag (`crates/kmux-protocol/src/codec.rs`,
  [compression.md](compression.md)).
- **Flush-coalescing batching.** The per-connection writer (TCP/TLS + UDS) drains
  all queued messages, writes them as whole frames, and flushes once instead of
  per message — far fewer TLS records / syscalls, byte-identical wire output
  (`write_frame_compressed_into` + `flush`, `crates/kmuxd/src/client_handler/session.rs`).
- **Arc-shared snapshots.** `TerminalSnapshot` carries `Arc<GridSnapshot>`, so
  fanning one snapshot to multiple recipients (multi-GUI, federation,
  force-full-snapshot) is O(1) rather than a deep grid copy.
- **Grid-digest desync oracle** (PROTOCOL_VERSION 36). Continuous, self-healing
  verification that the client's reconstructed grid matches the daemon's
  authoritative grid; see [architecture-verification.md](architecture-verification.md).
- **Connection pausing** (issue #68) and **transport hot-swap + scoring**
  (issue #69) — see [connection.md](connection.md), [connection-pause.md](connection-pause.md).

## Open

Ordered by estimated impact for the typical (one local GUI) case.

### 1. Off-UI-thread client diff application

**Now:** `CellGrid::apply_diff` runs on the iced/GTK UI tick
(`crates/kmux-app/src/driver/mod.rs`), which also paints from the grid every
frame. A large burst can momentarily block the UI.

**Change:** apply diffs on a worker thread and publish to the UI via a generation
**seqlock** / double-buffer handoff, keeping `apply_diff` pure. **Highest risk:** a
content digest is blind to read-during-apply tears, so this must carry its own
invariant — verify with a `loom` model of the publish handoff, a reader/writer
property test asserting no torn `(generation, snapshot)` pair, and a worker-side
seqno-order assertion. Medium-large effort.

### 2. Diff-engine scrollback mirror clone

**Now:** `diff_engine` clones each frame's new scrollback lines into its mirror
while also returning them for the `ScrollbackAppend` (`compute.rs`,
`self.mirror.append(scrollback_lines.clone())`) — one full clone per scrolling
frame.

**Change:** share the lines (e.g. `Arc` per line) between the mirror and the
outgoing message. Touches scrollback correctness, so gate on the conformance
suite. Medium effort.

### 3. Renderer dirty-row cache

**Now:** the renderer rebuilds its draw cache when `cells_generation` changes,
even if one cell moved.

**Change:** track dirty rows and re-render only those. A content digest cannot
cover render bugs — verify against the deterministic `kmux diagnostic` patterns,
frozen once. Small-medium effort.

### 4. QUIC per-pane writer batching

**Now:** flush-coalescing batching (Shipped) covers the merged TCP/UDS writer but
not the QUIC `pane_uni_writer` (`crates/kmuxd/src/connection.rs`), which still
flushes per message. QUIC's per-message flush is cheap, so this is low priority.

### 5. Impairment-harness extension

The deterministic shim (`crates/kmuxd/src/impair.rs`, `KMUX_NET_DELAY_MS` etc.)
covers QUIC latency only. Extend it to TCP/UDS and to forcing overflow→`Lagged`,
and run the oracle under each variant, before relying on items 1–2 in production.
Testing infrastructure, not a runtime change.
