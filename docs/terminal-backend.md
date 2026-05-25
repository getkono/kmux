# Terminal Backend Architecture

## Overview

kmux uses a **server-authoritative VT rendering model**: only the daemon
(`kmuxd`) runs a VT emulator. It parses PTY output, maintains a grid, and
ships pre-resolved `CellState` diffs to thin clients. Clients never touch raw
escape sequences — they render what the server tells them.

```
PTY ──bytes──► TerminalBackend (libghostty-vt, via GhosttyBackend)
                     │
              DiffEngine<B>   ← computes frame-to-frame CellState diffs
                     │
         ServerMessage::TerminalUpdate  ──► (fan-out) ──► all attached clients
```

The VT emulator is **libghostty-vt v1.3.1**, vendored at `vendor/ghostty/`
as a git submodule.  Ghostty exposes the `Terminal` / `Stream(Handler)` /
`Screen` types only as an unstable Zig module, so kmux ships a small Zig
wrapper that pins a **kmux-owned, stable C ABI** over those types.  The
wrapper lives at `crates/kmux-ghostty-sys/zig/src/wrapper.zig`; Rust
consumes it through two crates:

- `kmux-ghostty-sys` — `#[repr(C)]` structs and `extern "C"` declarations;
  the crate's `build.rs` drives `zig build` and emits a single
  `libkmux_ghostty.so` that ships with the daemon.
- `kmux-ghostty` — safe Rust façade: `GhosttyTerm` owning the opaque
  handle, `EventSink` trampolines, `Send` assertion, typed errors.

`GhosttyBackend` in `crates/kmuxd/src/backend/ghostty/mod.rs` wraps
`GhosttyTerm` with the kmuxd `TerminalBackend` trait.

## `TerminalBackend` trait

Located in `crates/kmuxd/src/backend/mod.rs`.

### Key design properties

**Static dispatch.** `DiffEngine<B: TerminalBackend>` uses a generic parameter,
not a trait object.  The `new()` and `name()` methods have `where Self: Sized`
bounds, making the trait intentionally non-object-safe.  This avoids a vtable
hop on the hot path (every PTY byte triggers `feed()`).

**`BackendConfig` construction.** All backends are created with a single
`BackendConfig` value:

```rust
pub struct BackendConfig {
    pub size: BackendSize,
    pub capabilities: CapabilityHandles,
    pub events: Arc<dyn BackendEventSink>,
    pub scrollback: usize,
}
```

**Required methods.**

| Method | Purpose |
|---|---|
| `new(cfg) -> Self` | Construct backend; `where Self: Sized` |
| `name() -> &'static str` | Human-readable name; `where Self: Sized` |
| `feed(&mut self, data: &[u8])` | Push raw PTY bytes into the parser |
| `size(&self) -> BackendSize` | Current grid dimensions |
| `fill_cells(&self, out: &mut [CellState])` | Snapshot the grid |
| `cursor(&self) -> CursorState` | Cursor position and shape |
| `modes(&self) -> TermModes` | Terminal mode flags |
| `resize(&mut self, size: BackendSize)` | Resize the emulator |

Optional hooks with default no-op implementations: `fill_cells_and_cursor`,
`is_alt_screen`, `history_size`, `read_history_lines`.

## `BackendSize` — wire vs. emulator dimensions

```rust
pub struct BackendSize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,   // 0 = unknown
    pub pixel_height: u16,  // 0 = unknown
}
```

`pixel_*` fields are `0` when the platform does not expose them.  Backends that
support graphics protocols (sixel, kitty-image) use these for image scaling;
cell-only backends ignore them safely.  `From<TermSize>` / `From<BackendSize>`
conversions are provided so the wire type and the emulator type stay decoupled.

### Authority under mixed clients

When multiple clients attach at different sizes and one reports pixel dims of `0`
(unknown), only the pixel dims from the client that won the cell-dim minimum are
used; otherwise `0` is carried through.  This is safe: sixel/kitty scaling
degrades gracefully when `0` is passed to the emulator.

## `BackendEventSink` — non-blocking contract

```rust
pub trait BackendEventSink: Send + Sync + 'static {
    fn on_title(&self, _title: &str) {}
    fn on_bell(&self) {}
    fn on_osc52_copy(&self, _selection: &str, _base64_data: &str) {}
    fn on_hyperlink(&self, _id: Option<&str>, _uri: &str) {}
}
```

**All implementations MUST NOT block.** The sink is called from inside the VT
parser loop (`feed()`).  Any I/O must be pushed to an unbounded `mpsc` channel
and drained from a separate task.

`GhosttyBackend` installs a thin adapter that forwards libghostty-vt events to
whichever `Arc<dyn BackendEventSink>` the host passes in.  `NullEventSink`
(no-op) is used in code paths that do not need backend events.

## Key encoding (server-side)

The daemon encodes user keystrokes into terminal escape bytes via Ghostty's
`gvt.input.encodeKey`, fed with the live mode state of the per-pane Ghostty
terminal (DECCKM, kitty kbd flags, modifyOtherKeys, …).  The client sends
structured `KeyEvent`s on the wire; the daemon's `Backend::encode_key_event`
hands each one to the encoder and writes the bytes to the PTY.

This guarantees that modifier-encoded keys (Shift+Enter, Alt+Enter,
Shift+Tab, Ctrl+Arrow) match the protocol the inner program negotiated at
runtime.  See [`docs/keyboard.md`](keyboard.md) for the full architecture
and per-keystroke encoding examples.

## Multi-client size negotiation (smallest-wins)

**Policy.** Effective pane size = `min(rows) × min(cols)` across all currently
attached `ClientSender.size` values.  The minimum is computed independently for
rows and columns.

**Rationale.** This matches tmux: a client that cannot display beyond its
viewport should never see corrupted output.  The largest common visible area is
the intersection, not the union.

### How it works

1. On `Attach` or `Resize`, the daemon updates `ClientSender.size` for the
   calling client inside `PaneRelay.clients`.
2. `PaneRelay::apply_effective_size()` recomputes the minimum and, if it
   changed, calls `DiffEngine::resize(BackendSize)` on the emulator
   synchronously.
3. `PaneRelay::broadcast_resize()` fans out a `PaneResized` event to every
   client's control channel, then queues a forced `TerminalSnapshot` on every
   client's data channel so all grids are re-seeded at the new dims.
4. The kernel PTY is resized via `TIOCSWINSZ` after the emulator (and after the
   write lock is released) to avoid a lock-order deadlock.

**Detach keeps last effective size.** When `apply_effective_size()` finds no
remaining clients, it returns `None` immediately — `relay.size` is unchanged.
The pane holds the last negotiated size until the next attach.

**Race fix.** The original `resize()` in `app/io.rs` released the read lock and
then re-acquired a write lock, opening a window where a concurrent `Attach`
could insert a client in between, causing the PTY and emulator to diverge.
The current implementation acquires a single write lock for the entire
emulator-resize + broadcast sequence; only the async `TIOCSWINSZ` call happens
outside the lock.

## Protocol v14 contract

`TermSize` on the wire now carries pixel dimensions:

```rust
pub struct TermSize { pub rows: u16, pub cols: u16, pub pixel_width: u16, pub pixel_height: u16 }
```

`Default = { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }`.

`ClientMessage::Attach` carries `size: TermSize` so the daemon can apply
smallest-wins at attach time rather than waiting for the first `Resize`.

`SessionEventMsg::PaneResized` also carries `TermSize` (was `rows, cols`).

## Scrollback: absolute indices, daemon mirror, and rendering

### Problem it solves

Two scrollback regressions surfaced after the libghostty-vt port:

1. Resizes blew away the client's scrollback because `apply_snapshot` wiped it
   and every resize ships a snapshot.
2. libghostty-vt reflows on resize: lines it reports as visible history before
   resize can be evicted or restructured during the resize call itself. From
   the daemon's point of view history just "shrank", so no delta was ever
   streamed and the lines were silently lost.

The fix layers an authoritative, backend-independent history record inside
the daemon and addresses each line by a monotonic absolute index that both
sides share. Phase B (protocol v15) added the mirror and the indexing;
Phase C will drop inline `scrollback_lines` from `TerminalDiff` entirely
and move to lazy fetch.

### Invariants

1. **Snapshots never wipe scrollback.** `apply_snapshot` rewrites only the
   viewport; scrollback persists across resize and reattach. Explicit
   `SyncReset` or session restart are the only paths that clear history.
2. **Every scrollback line has an absolute `u64` index.** The index is
   monotonic for the life of the pane and shared end-to-end. An `append` at
   `first_index = N` asserts `N == current base + len()`; a mismatch is a
   gap, not a logic error.
3. **Daemon owns the source of truth.** `ScrollbackMirror` is independent of
   libghostty-vt's own scrollback ring. The mirror outlives the backend's
   resize reflow and alt-screen transitions.
4. **Lines are stored at capture width.** No truncation on insert; clients
   render them by wrapping, not clipping, when the viewport is narrower.
5. **`WIDE_CHAR_SPACER` slots are empty symbols.** The second half of a
   double-width glyph writes `set_symbol("")` — ratatui convention — not a
   trailing space.

### `ScrollbackMirror` (daemon)

Located at `crates/kmuxd/src/diff_engine/mirror.rs`.

```rust
pub struct ScrollbackMirror {
    base_index: u64,                  // oldest index still stored
    lines: VecDeque<Vec<CellState>>,  // bounded ring
    cap: usize,                       // MIRROR_CAPACITY = 10_000
}
```

Addressable API: `append`, `history_total`, `base_index`, `range(start, count)`,
`tail(n)`, `tail_first_index(n)`. When the ring is full, the oldest line is
evicted and `base_index` advances. Indices below `base_index` are
unrecoverable from the mirror; a client that asks for them gets a clamped
response starting at `base_index`.

### Wire changes (v15)

`PROTOCOL_VERSION` = **15**. Added fields/messages:

- `TerminalDiff` gains `history_total: u64` (monotonic count of lines ever
  scrolled off, as of this frame). `scrollback_lines` still present in v15
  for backwards compatibility with clients that don't track
  `ScrollbackAppend`; removed in v16.
- `GridSnapshot` gains `history_total: u64` and `scrollback_tail:
  Vec<Vec<CellState>>` (the last `SNAPSHOT_TAIL_LINES = 500` lines of the
  mirror). Reattaching clients render scrollback immediately without a
  round-trip.
- New `ServerMessage::ScrollbackAppend { pane_id, first_index, lines, seqno,
  sent_at_ms }`. Shares the `seqno` space with `TerminalUpdate` so the
  client applies them in order.
- New `ClientMessage::FetchHistory { request_id, pane_id, start_index: u64,
  count: u32 }` and reply `ServerMessage::HistoryLines { request_id,
  pane_id, first_index: u64, lines, history_total: u64 }`. Wired in v15;
  exercised on the client in Phase C.

Postcard is not self-describing — any struct field or enum variant change
is a wire break, which is why v14 → v15 → v16 each bump the version.

### Wire changes (v17)

`PROTOCOL_VERSION` = **17**. Added fields/messages:

- `PaneInfo` gains `title: String` (latest OSC 0/2 window title; empty until
  the program emits one). Populated by the daemon from `PaneRelay.title`.
- New `SessionEventMsg::PaneTitleChanged { pane_id, title }` broadcast by the
  daemon whenever the pane's VT emulator reports a new title. Production
  pane relays install a `PaneTitleSink` as the backend's event sink; the
  sink stores the title on the relay and pushes the event to every attached
  client on the unbounded control channel (non-blocking; VT-parser-safe).

### Daemon flow (per diff)

```
PTY bytes ──► backend.feed() ──► backend.history_size() vs prev
                                      │
                              new scrollback lines
                                      │
                           mirror.append(lines) ──► (first_index, count)
                                      │
                     TerminalDiff { ..., history_total }  ──► TerminalUpdate
                                      │
                     ScrollbackAppend { first_index, lines }  (separate msg)
```

`DiffEngine::resize` drains any backend-held history beyond our mirror's
head **before** calling `backend.resize()`; libghostty may reflow or evict
during the call. Post-resize, `prev_history_size` is re-synced to the
backend's new value so the next diff cycle only picks up genuinely new
lines — the mirror itself keeps everything ever seen.

### Client flow

Located at `crates/kmux-client/src/grid/scrollback.rs` and `grid/mod.rs`.

```rust
pub struct ScrollbackBuffer {
    lines: VecDeque<Vec<CellState>>,
    max_lines: usize,
    base_index: u64,
}
```

- `seed_tail(history_total, tail)` — called from `apply_snapshot`; sets
  `base_index = history_total - tail.len()` and replaces the ring with the
  tail slice.
- `append_with_index(first_index, lines) -> bool` — returns `false` on gap.
  On gap the buffer is cleared and the client will re-seed from the next
  snapshot (Phase B) or issue `FetchHistory` (Phase C).
- `get_absolute(idx)` — O(1) lookup by absolute index.

`apply_diff` derives `first_index = history_total - scrollback_lines.len()`
and calls `append_with_index`. `apply_scrollback_append` handles the
out-of-band variant. `apply_history_lines` fills gaps at the current head
(skips lines the buffer already has).

### Wrap-aware rendering

A scrollback line captured at 200 cols and rendered in an 80-col viewport
must span multiple viewport rows, not get clipped. `crates/kmux/src/ui/grid.rs`
and the helpers in `crates/kmux-client/src/grid/mod.rs` implement this:

- `effective_line_len(line)` — cells up to the last non-blank.
- `display_rows_for_line(line, cols)` — `max(1, ceil(effective / cols))`.
- `scrollback_display_row_at(scrollback, cols, rev_offset) -> (line_idx, col_start)`
  — walks from newest backwards to find the logical line and column window
  that owns a given display row.

`scroll_offset` is denominated in **display rows**, not logical lines.
`scroll_up(n)` caps at `total_scrollback_display_rows()` so users can scroll
through every captured row even if lines are wider than the viewport.

### What isn't in Phase B

- `scrollback_lines` is still on `TerminalDiff` (dropped in v16).
- The client does not yet issue `FetchHistory` on scroll-into-gap (Phase C).
- Daemon-restart persistence of the mirror is not implemented but the
  layout (bounded VecDeque + `base_index`) is chosen so a later change can
  flush it to `$XDG_STATE_HOME/kmux/sessions/<word_id>/pane-<n>.scrollback`
  without reshaping the API.

## Persistence decoupling

Disk format uses `PersistedTermSize { rows: u16, cols: u16 }` (no pixel fields).
This keeps the on-disk `STATE_VERSION = 2` unchanged — old checkpoints load
cleanly.  A translation shim pads `pixel_width = 0, pixel_height = 0` on read;
the reverse conversion drops them on write.

The first `Attach` from a live client after daemon restart will carry the real
terminal dimensions, trigger `reconcile_size`, and update both the emulator and
the PTY to match.

## FFI invariants (`kmux-ghostty-sys` ↔ Zig wrapper)

The Zig wrapper is single-threaded under the kmuxd `Arc<Mutex<DiffEngine<_>>>`
held at every `new_term_state` call site.  `GhosttyBackend` is asserted `Send`
and explicitly `!Sync` via `static_assertions`.  Safety rules exchanged across
the boundary:

- **No ownership transfer.** All `uint8_t*` / `kmux_cell*` buffers are borrowed;
  valid only for the duration of the call (callbacks) or written into
  caller-allocated memory (fill functions).
- **Event callbacks must not retain pointers.** The Rust trampoline copies title
  / hyperlink bytes to an owned `&str` via `str::from_utf8` (silently drops on
  invalid UTF-8) before handing them to the sink.
- **Kitty toggles are borrowed atomics.** Rust holds the `Arc<AtomicBool>`s;
  Zig stores `*const std.atomic.Value(bool)` and does an acquire load per hit.
  `GhosttyBackend` guarantees the `Arc`s outlive the opaque term handle.
- **ABI version check on construction.** `kmux_ghostty_abi_version()` is
  compared against a compile-time constant on both sides; mismatch panics.

## Reserved: runtime backend selection

```rust
#[allow(dead_code)]
pub trait BackendFactory: Send + Sync + 'static {
    fn name(&self) -> &'static str;
}
```

Not wired to anything today.  If runtime backend switching is ever needed, a
factory registry can use this trait to construct backends by name without
changing the `DiffEngine<B>` static-dispatch path.

## Adding a second backend

The public surface below is what any new backend has to satisfy.  Nothing on
the daemon or wire side assumes libghostty-vt specifically — adding a second
backend is a self-contained change to a new `backend/<name>/` submodule plus
a type swap in `term_state.rs`.

1. Implement `TerminalBackend` in `crates/kmuxd/src/backend/<name>/mod.rs`.
   Wire `BackendConfig.events` to the backend's title/bell/OSC callbacks
   without blocking.
2. Port the behavioural suite in `backend/ghostty/mod.rs` verbatim — those
   tests are the contract every backend must meet.
3. Repoint `ActiveBackend` in `term_state.rs` (or add a `BackendFactory`-based
   registry if you want both compiled in).
4. Update this document.
