# Terminal Backend Architecture

## Overview

kmux uses a **server-authoritative VT rendering model**: only the daemon
(`kmuxd`) runs a VT emulator. It parses PTY output, maintains a grid, and
ships pre-resolved `CellState` diffs to thin clients. Clients never touch raw
escape sequences — they render what the server tells them.

```
PTY ──bytes──► TerminalBackend (wezterm / future ghostty)
                     │
              DiffEngine<B>   ← computes frame-to-frame CellState diffs
                     │
         ServerMessage::TerminalUpdate  ──► (fan-out) ──► all attached clients
```

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

`NullEventSink` (no-op) is used in production today.  The ghostty backend will
install a real sink that forwards title/bell events to the host UI.

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

## Persistence decoupling

Disk format uses `PersistedTermSize { rows: u16, cols: u16 }` (no pixel fields).
This keeps the on-disk `STATE_VERSION = 2` unchanged — old checkpoints load
cleanly.  A translation shim pads `pixel_width = 0, pixel_height = 0` on read;
the reverse conversion drops them on write.

The first `Attach` from a live client after daemon restart will carry the real
terminal dimensions, trigger `reconcile_size`, and update both the emulator and
the PTY to match.

## Feature flags

```toml
[features]
default = ["backend-wezterm"]
backend-wezterm = ["dep:tattoy-wezterm-term", "dep:tattoy-wezterm-surface"]
backend-ghostty = []   # reserved; no implementation yet
```

`backend-ghostty` is declared but produces no code.  The `ActiveBackend` type
alias in `term_state.rs` is gated on `cfg(feature = "backend-wezterm")`:

```rust
#[cfg(feature = "backend-wezterm")]
pub type ActiveBackend = WezTermBackend;

pub type TermState = DiffEngine<ActiveBackend>;
```

## `BackendFactory` — reserved for runtime selection

```rust
#[allow(dead_code)]
pub trait BackendFactory: Send + Sync + 'static {
    fn name(&self) -> &'static str;
}
```

Not wired to anything today.  If runtime backend switching is ever needed (e.g.
a daemon that supports both wezterm and ghostty simultaneously), a factory
registry can use this trait to construct backends by name without changing the
`DiffEngine<B>` static-dispatch path.

## Adding a new backend (e.g. libghostty)

1. **Enable the feature.** In `kmuxd/Cargo.toml` add the libghostty dep under
   `backend-ghostty`:
   ```toml
   backend-ghostty = ["dep:ghostty-sys"]
   ```

2. **Create `backend/ghostty/mod.rs`** with a `GhosttyBackend` struct that
   implements `TerminalBackend`.  Wire `BackendConfig.events` to the libghostty
   surface callback so title/bell push to the `BackendEventSink` without
   blocking.

3. **Declare `ActiveBackend` for the feature** in `term_state.rs`:
   ```rust
   #[cfg(feature = "backend-ghostty")]
   pub use crate::backend::ghostty::GhosttyBackend;
   #[cfg(feature = "backend-ghostty")]
   pub type ActiveBackend = GhosttyBackend;
   ```
   Enforce mutual exclusivity with a `compile_error!` if both features are
   enabled simultaneously.

4. **Update `backend/mod.rs`** to declare the ghostty submodule under
   `#[cfg(feature = "backend-ghostty")]`.

5. **Tests.** Add a `MockBackend`-style test in `backend/ghostty/mod.rs` for
   the new trait implementation.  The existing `DiffEngine` tests in
   `diff_engine/compute.rs` stay backend-agnostic because they use `MockBackend`.

Files a ghostty PR touches (and nothing else):
- `kmuxd/Cargo.toml` — dep + feature activation
- `crates/kmuxd/src/backend/ghostty/mod.rs` — new file
- `crates/kmuxd/src/backend/mod.rs` — `pub mod ghostty` declaration
- `crates/kmuxd/src/term_state.rs` — `ActiveBackend` alias
