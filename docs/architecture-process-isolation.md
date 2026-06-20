# Session process isolation (issue #126)

## Why

`kmuxd` runs a VT emulator per pane. The emulator is **libghostty-vt**, reached
through FFI (`kmux-ghostty` over `kmux-ghostty-sys`, a Zig/C library). That FFI
is the crash-prone surface: a memory fault inside libghostty-vt raises a
**SIGSEGV/SIGABRT against the whole daemon**, taking down every session and
client at once.

Issue #126 asks to stop one session from crashing the daemon. Its title says "by
OS threads," but the body hedges — and the hedge is right: `catch_unwind` and OS
threads share one address space and one process-wide signal disposition, so they
contain a Rust `panic!` but **cannot** contain a hardware trap or `abort()` from
C. Only a **separate OS process** is an independent failure domain the kernel
tears down in isolation. So the FFI surface must run in a subprocess.

## What runs where

```
┌─ kmuxd (daemon) ──────────────────────────────┐        ┌─ kmux-vt-worker ─────────┐
│ transports, session registry, client fan-out, │        │ PTY master fd (dup)      │
│ auth, layout, federation, handoff              │        │ GhosttyBackend (FFI)     │
│                                                │        │ DiffEngine + mirror      │
│  PaneEngine::Worker                            │        │                          │
│   ├─ req_tx  ───── Input/Keys/Resize/… ───────────────▶ │ request loop             │
│   ├─ supervisor ◀── Diff/Cursor/Title/Exit ──────────── │ PTY read loop            │
│   └─ CellGrid mirror (for sync snapshots)      │  socketpair (postcard frames)     │
│                                                │   + SCM_RIGHTS once (the PTY fd)   │
└────────────────────────────────────────────────┘        └──────────────────────────┘
```

The daemon keeps only **pure-safe-Rust orchestration**. Everything that touches
the Ghostty FFI (and the `kmux-pty` libc I/O) lives in the worker. A worker
SIGSEGV kills only that worker; the daemon and every other session keep running.

## The seam

Everything VT-related sits behind one trait, shared by both paths so they emit
**byte-identical diffs by construction**:

- **`kmux-vt-core`** (new crate) — the `TerminalBackend` trait + its
  `GhosttyBackend` impl, the `DiffEngine`, the `ScrollbackMirror`, and the
  `TermState` alias. Extracted out of `kmuxd` so the daemon's in-process path and
  the worker both depend on the same code.
- **`kmux-worker-protocol`** (new crate) — the versioned daemon↔worker wire
  contract (`WorkerRequest`, `WorkerEvent`, `WORKER_PROTOCOL_VERSION`) and its
  framing codec. Server-side only; GUI frontends never depend on it.
- **`kmux-vt-worker`** (new bin) — the worker: adopt the PTY fd, run the
  `kmux-vt-core` pipeline, speak `kmux-worker-protocol`. The FFI executes *only*
  in this binary.
- **`kmuxd::engine::PaneEngine`** — an enum (`InProcess` | `Worker`) that
  `PaneRelay` holds instead of touching `term_state`/`writer` directly. Every VT
  read (snapshot, history) and PTY write (input, keys, paste) routes through it.

`InProcessEngine` is the default and is byte-for-byte the daemon's original
behavior. `WorkerEngine` runs the pipeline out-of-process.

## Boundary choice

The worker owns the **whole terminal half** of a pane (PTY fd + backend + diff
engine + scrollback mirror) and emits the *compact diff messages* the client
wire protocol already carries. Moving only the `TerminalBackend` trait
out-of-process would be the wrong seam — `compute_diff()` would ship the full
`rows*cols` grid across the socket every frame just to diff it away, and would
split `DiffEngine`'s `prev_cells`/`mirror` from the backend.

## Transport & handshake

One `AF_UNIX`/`SOCK_STREAM` socketpair. The first frame is the daemon's
`Hello`, carrying the PTY master fd as `SCM_RIGHTS` ancillary data — the *only*
fd that ever crosses the link (the same mechanism the daemon handoff uses). The
worker adopts it (`PtyProcess::from_inherited`) and replies `Ready`; that
fd-carrying handshake is lock-step. After it, both ends split the stream and
exchange fd-less, length-prefixed postcard frames concurrently, reusing the
`kmux-protocol` codec.

**The daemon retains the authoritative master fd** (the worker holds a dup).
This is the load-bearing invariant: when the worker dies, the daemon's fd keeps
the PTY's file description open, so the shell receives no SIGHUP and survives.

## Diffs, seqnos, and the mirror

The worker emits **unsequenced** `Diff`/`CursorOnly` events. A daemon-side
**supervisor task** stamps the monotonic seqno, pushes to the pane's scrollback
replay buffer, and fans out through the *same* `dispatch_diff_result` the
in-process relay uses — so a worker pane is identical on the wire to an
in-process one, and seqnos survive a worker restart (they live in the daemon).

Several daemon call sites read the grid **synchronously** while holding the
`sessions` lock (attach replay, resize re-seed, checkpoint, force-full-snapshot).
To keep them synchronous, `WorkerEngine` maintains a daemon-side **`CellGrid`
mirror** (the same client grid model federation uses for proxied panes), fed
from the worker's event stream. `snapshot()` and history reads come from the
mirror — no IPC round-trip. The kernel PTY `TIOCSWINSZ` is issued by the daemon
via its retained fd; the worker only resizes its emulator.

## Versioning & fallback

`WORKER_PROTOCOL_VERSION` gates the link. The daemon and worker are normally the
same build (the daemon execs the worker next to itself), but a stale binary
after an in-place upgrade is possible; on a version mismatch — or any worker
spawn failure, or a missing worker binary — the daemon **falls back to the
in-process engine** for that pane, which is always safe.

## Opt-in rollout

Isolation is selected per pane by `KMUX_SESSION_ISOLATION=process`. The default
is in-process, mirroring how the GPU renderer shipped (`KMUX_RENDERER=wgpu`):
the worker path is compiled and tested everywhere, but the runtime default is
unchanged until it has soaked. Making isolation the default is a deliberate
post-soak follow-up.

The daemon finds the worker binary via `$KMUX_VT_WORKER_BIN`, else next to its
own executable, else on `PATH`.

## Crash handling

A worker death is just a task seeing its socket EOF — the daemon is structurally
unaffected. The supervisor reaps the child and inspects its exit status: a clean
exit (code 0) is the pane-close/handoff path; a signal death (SIGSEGV/SIGABRT)
or non-zero exit is a **crash**, on which it:

1. broadcasts `SessionEventMsg::PaneFaulted` to the pane's attached clients
   (PROTOCOL_VERSION 28), and
2. reports the pane id on a channel to a single daemon-level respawn task
   (`app::recover`) — never re-entrantly from the dying supervisor.

The respawn task re-adopts the daemon's retained master fd into a fresh worker
(the shell is still alive), swaps the engine, and resyncs attached clients with a
snapshot. A crash-loop guard bounds restarts to 3 per pane within 60s; past that
the pane is left faulted.

## Interaction with handoff & federation

- **Handoff** (`crate::handoff`, live daemon upgrade): the daemon still owns
  every PTY master fd, so the fd-migration path is unchanged. `quiesce_relays`
  calls `PaneEngine::abort_relay_task`, which for a worker sends `Shutdown`
  (releasing the worker's dup) before aborting. The successor daemon respawns
  fresh workers post-restore in whatever isolation mode it is configured for;
  live worker migration across a handoff is intentionally not attempted.
- **Federation** (`crate::federation`): a proxied remote pane has no local PTY or
  VT — the remote daemon runs the emulator and the local daemon mirrors it.
  `WorkerEngine` therefore applies only to locally-hosted panes; federated panes
  are untouched.

## Tested

- `kmux-vt-worker/tests/worker_smoke.rs` — drives a real worker subprocess
  through the whole boundary (fd passing + handshake + steady stream) and
  asserts PTY output becomes a cell diff.
- `kmuxd/tests/process_isolation_e2e.rs` — launches a real daemon with
  `KMUX_SESSION_ISOLATION=process`, attaches a client, kills the worker, and
  asserts the daemon survives, the client sees `PaneFaulted`, the pane respawns
  and resyncs, and a fresh isolated session still works.

## Follow-ups

These are designed but intentionally deferred to keep this change reviewable:

- **Content-preserving respawn.** Today a respawned worker starts blank (the live
  shell does not redraw), so the visible screen is lost across a crash though the
  pane stays usable. To preserve it, add a `WorkerRequest::SeedEmulator { data }`
  that feeds a reconstructed ANSI preamble (via the existing
  `app::restore::snapshot_to_ansi`) into the new worker's emulator — not the PTY
  — before resuming live reads.
- **Per-session worker grouping** (`IsolationUnit::{Pane, Session}`). The current
  model is one worker per pane, which is the *strongest* isolation (a crash takes
  down exactly one terminal). A per-session mode would multiplex a session's
  panes over one worker to cut process count at the `MAX_SESSIONS=1000` ceiling,
  trading isolation for fewer processes. The wire protocol is already
  pane-addressed (`Hello` carries `pane_id`), so this is a supervisor/worker
  lifecycle change, not a wire change.
- **Make isolation the default** after soak, then retire the in-process path or
  keep it as the `KMUX_SESSION_ISOLATION=thread`-style fallback.
