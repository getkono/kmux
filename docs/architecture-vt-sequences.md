# Control-sequence handling and unknown-sequence logging

This document describes how kmux handles terminal control sequences: which ones
it gives special treatment, where that handling lives, and how sequences the
emulator does not implement are surfaced to users (issue #187). It complements
[terminal-backend.md](terminal-backend.md), which covers the VT backend overall.

## kmux does not parse VT — libghostty-vt does

kmux feeds raw PTY bytes to **libghostty-vt** (via `kmux-ghostty`), which owns
the parser and the grid model. The vast majority of sequences — cursor moves,
SGR colours/attributes, DEC modes, scrolling, OSC palette changes — are handled
entirely inside libghostty's terminal model and never surface to kmux. kmux reads
the resulting grid each frame and diffs it.

A small, deliberate set of sequences is **intercepted** because kmux must do
something with them beyond updating the grid (forward to clients, mirror into
per-pane state, or both). Everything about that special handling is funneled
through one type and one method, so it can be audited in one place.

## The catalog: `ControlEvent`

`kmux_vt_core::backend::ControlEvent` is the single enum every interception is
funneled through. Each variant documents the originating sequence, where it is
intercepted, and what kmux does with it:

| Variant      | Sequence  | What kmux does                                              |
|--------------|-----------|------------------------------------------------------------|
| `Title`      | OSC 0 / 2 | store per-pane, broadcast `PaneTitleChanged`               |
| `Bell`       | BEL       | broadcast `PaneBell`; mark the owning tab for attention    |
| `Osc52Copy`  | OSC 52    | broadcast `PaneClipboardCopy` (payload decoded client-side)|
| `Progress`   | OSC 9;4   | store per-pane, broadcast `PaneProgressChanged`            |
| `Hyperlink`  | OSC 8     | surfaced; no client-facing wire event yet (dropped)        |

The interception itself happens in **one Zig switch** — `Handler.vt` in
`crates/kmux-ghostty-sys/zig/src/wrapper.zig` — which pulls these actions out of
libghostty's stream and delegates everything else to the read-only handler. On
the Rust side, `EventSinkAdapter` (in `kmux-vt-core/src/backend/ghostty/mod.rs`)
turns each into a `ControlEvent`.

## The dispatch: `BackendEventSink::on_control_event`

Every consumer implements one method, `BackendEventSink::on_control_event`, and
`match`es on `ControlEvent`. There are exactly two production consumers, and they
are the two places to read to know "what does kmux do specially with VT
sequences?":

- **Daemon** — `PaneEventSink` (`crates/kmuxd/src/app/mod.rs`): broadcasts the
  wire events above to attached clients.
- **Isolated VT worker** — `WorkerEventSink` (`crates/kmux-vt-worker/src/main.rs`):
  re-emits title/bell/OSC 52 over the worker protocol; the daemon then runs them
  back through the *same* dispatch (`crates/kmuxd/src/engine/worker.rs`). The
  worker protocol has no frame for progress/hyperlinks, so the process-isolation
  path does not forward those (see [architecture-process-isolation.md](architecture-process-isolation.md)).

Adding a new interception is a three-step, compiler-guided change: add a
`ControlEvent` variant, map it in the Zig handler + `EventSinkAdapter`, and handle
it in the two `match`es.

## Unknown / unimplemented sequences

Sequences libghostty-vt does **not** implement never reach `ControlEvent`: the
parser drops them. libghostty already detects them and logs `unimplemented
CSI/ESC/OSC action` (and similar) through Zig's `std.log`, but that output went
to the default stderr sink that nothing captures.

kmux captures it at the source. `wrapper.zig` is the **root module** of the
`libkmux_ghostty` shared library, so its `pub const std_options.logFn` governs
every `std.log` call inside the linked libghostty-vt. That `logFn` forwards
warn-and-above lines to a process-global C callback the Rust side installs via
`kmux_ghostty_set_log_callback` (wrapper ABI 5).

The Rust side:

1. `kmux_ghostty::set_log_handler` installs the C trampoline (keeping the unsafe
   FFI + lossy-UTF-8 conversion inside `kmux-ghostty`) and hands callers a safe
   `fn(VtLogLevel, &str, &str)`.
2. `kmux_vt_core::backend::install_vt_log_forwarding` (called at daemon and worker
   startup) re-emits each line via `tracing` under the **`kmux::vt`** target.
   Identical messages are de-duplicated — the first is logged in full, repeats are
   suppressed and only periodically summarised — with a bounded tracked set so a
   misbehaving program cannot grow memory.

The daemon's `EnvFilter` carries a `kmux::vt=warn` directive so these lines reach
`daemon.log`; the worker's default `warn` filter already passes them to its
stderr (which the daemon captures).

### Reviewing them

```
kmux daemon logs                 # local daemon log (grep for kmux::vt)
kmux daemon logs -n 200          # last 200 lines
kmux daemon logs --server host   # a remote daemon's log, over the data plane
kmux daemon logs --follow        # tail -f
```

Under `[daemon] session_isolation = "process"`, the VT pipeline (and thus these
warnings) runs in the per-pane `kmux-vt-worker`; its stderr is captured with the
worker's output, so attribution to a pane is automatic in that mode. In the
default in-daemon mode the lines are not yet attributed to a specific pane — a
possible future enhancement.

## Verification

- `kmux-ghostty-sys` has a Rust test
  (`unimplemented_sequence_invokes_log_callback`) that links the *real* library —
  where `wrapper.zig` is the root module and `std_options` is active — installs a
  callback, feeds `CSI <space> A` (cursor-up with an intermediate, which
  libghostty logs as unimplemented), and asserts the callback fires. Note: `zig
  build test` uses the test runner as root, which bypasses `std_options`, so this
  check must live on the Rust side.
- `kmux_vt_core::backend::vt_log` unit-tests the de-dup/summary/bounding logic.
