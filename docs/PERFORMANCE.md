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

## Shipped: the CPU/scheduling follow-ups (issue #182)

The remaining items were CPU/scheduling, not transport or format. All five
shipped, each gated on the oracle/conformance suite staying green.

### 1. Off-UI-thread client diff application

`CellGrid::apply_diff` no longer runs on the UI tick. A dedicated apply worker
thread (owned by the `SessionManager`) holds the authoritative `GridContent` per
pane; the UI enqueues content mutations and the worker republishes a whole,
immutable `Arc<GridContent>` per touched pane through an **`ArcSwap`
double-buffer** (`crates/kmux-client/src/grid/apply_worker.rs`). A large burst no
longer blocks the UI. The `CellGrid` facade splits into apply-mutated content
(off-thread) and UI-owned view state (scroll/selection), so the view stays
responsive. The daemon's pane mirror and tests use the synchronous `Local`
backing unchanged.

We chose the Arc double-buffer over a hand-rolled seqlock: it is tear-free with
no `unsafe`, the `(generation, snapshot)` pair lives in one allocation (nothing
to tear against), and renderers keep a cheap shared borrow. A content digest is
blind to read-during-apply tears, so the handoff carries its own invariant —
verified by a reader/writer property test (real worker vs. a concurrent reader,
no torn tuple) and the worker's per-pane order assertion. See
[architecture-verification.md](architecture-verification.md).

### 2. Diff-engine scrollback mirror clone

Scrollback lines are now `Arc<[CellState]>` (`ScrollbackLine`) end to end —
materialised once at the libghostty FFI boundary and shared by the daemon mirror,
the `DiffResult`, the wire messages, and the client buffer. The per-frame
`mirror.append(clone)` and the per-client fan-out are now `Arc` pointer bumps.
postcard serialises `Arc<[T]>` byte-identically to `[T]` (serde `rc`), so there
was **no PROTOCOL_VERSION / worker / state bump**.

### 3. Renderer dirty-row cache

`GridContent` stamps each row with the generation it last changed at
(`row_generation`); `geometry::build_scene_cached` reuses the cell-layer geometry
of unchanged rows and re-emits only dirty rows, rebuilding the cheap view
overlays every frame. A content digest can't cover render bugs, so a frozen
parity test asserts the cached scene is byte-identical to a full rebuild.

### 4. QUIC per-pane writer batching

The QUIC `pane_uni_writer` (`crates/kmuxd/src/connection.rs`) now drains a batch
and flushes once, matching the merged TCP/UDS writer. Byte-identical wire output;
QUIC's flush is cheap, so the win is modest (fewer await points, uniform shape).

### 5. Impairment-harness coverage

The deterministic shim (`crates/kmuxd/src/impair.rs`, `KMUX_NET_*`) delays
pane-data frames on **every** transport (QUIC `pane_uni_writer` + TCP/UDS
`TcpAttacher`). The slow-client overflow→`Lagged` path is exercised under the
oracle by `relay::tests::oracle_survives_data_channel_overflow_lagged`, which
forces the overflow and asserts the digest stays clean across the resync.
