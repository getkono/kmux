# Graceful daemon handoff (live PTY migration)

`kmuxd` can restart **without killing the shells it hosts**. On a planned restart
the outgoing daemon streams each pane's live PTY master file descriptor to a
freshly-spawned successor over a Unix socket using `SCM_RIGHTS`; the running
processes (editors, REPLs, `tail -f`, build jobs, ssh sessions) keep running and
are simply reparented to init. This implements
[issue #35](https://github.com/getkono/kmux/issues/35).

It complements — and falls back to — the pre-existing **snapshot restore**
(`docs/daemon-lifecycle.md` §11), where a fresh shell is respawned and the old
grid/scrollback is replayed as ANSI. Snapshot restore preserves the *picture*;
handoff preserves the *process*.

## Why fd passing (and not the old approaches)

The unit of migration is the PTY **master fd**. A `dup` of it shares the same
open file description, so the child keeps its controlling terminal as long as
*any* dup stays open. Passing a dup to the successor (which gets its own dup from
the kernel) keeps the child alive across the predecessor's exit — no `SIGHUP`.

Two earlier primitives were inadequate and have been removed/repurposed:

- **dup-and-leak on drop** (`PtyProcess` keep-alive) only kept a child alive
  *within the same process*; when the daemon exits the kernel closes every fd,
  so the child got `SIGHUP` anyway. Keep-alive now serves only to suppress
  `SIGKILL` during the brief handoff overlap, and no longer leaks a dup at all
  (issue #205): the successor's `SCM_RIGHTS` copy is what keeps the terminal
  up.
- **`/proc/<pid>/fd` reattach** was Linux-only (broken on macOS). `SCM_RIGHTS`
  is POSIX and works identically on Linux and macOS.

## Sequence (O = outgoing, N = incoming)

```
client: kmux daemon restart ──restart──▶ O (control socket) ◀──{attempt}──
O: spawn the handoff task (the main loop keeps servicing SIGINT/SIGTERM)
O: close pane creation ; bind handoff.sock
O: spawn  N = current_exe + DAEMON_BOOT_ARGS + --handoff   (O's direct child,
   own process group, cwd /; N does not daemonize and takes no pid file)
N: connect handoff.sock          (none within 10 s: ask the control socket —
                                  a daemon serving there → N stands down)
O: accept within 15 s (else kill N, roll back) ; N's pid ← peer credentials
O ──Hello{version, token, panes, pid}──▶ N
N: pid == peer credentials' pid?  (else stand down)
N: version ok?  ──Accept──▶ O
     (else ──Decline──▶ O ; O writes its final checkpoint ; ──Released──▶ N ;
      N snapshot-restores)
loop over live panes (lock-step):
  O ──PaneFd{pane_id} + master fd (SCM_RIGHTS)──▶ N
  N ──PaneFdAck──▶ O
O: hold the PTY readers (≤ 5 s) ; write + fsync the final checkpoint (sealed)
O ──Complete──▶ N                       (Complete + Ack share one 10 s bound:
N ──Ack──▶ O                             the panes are frozen meanwhile)
                                        ◀── COMMIT POINT: O reads the Ack
O: set_all_keep_alive ; quiesce relays    (nothing here can fail the handoff)
O ──Released──▶ N ; O exits (releases listeners, control/data sockets, pid file)
N: restore_with_handoff(checkpoint, inherited fds) ; bind sockets ;
   claim pid file once O (by its verified pid) has exited ; serve
client: reconnect (new ports, adopted token) ; re-attach with last_seqno

any failure before the commit point — including SIGINT/SIGTERM to O:
O: release the readers ; unseal the checkpoint ; reopen pane creation ;
   ──Abort{reason}──▶ N ; wait ≤ 5 s for N to exit, else kill it ;
   record "stood down: <reason>" for `kmux daemon restart` ;
   keep serving (or, for a signal, shut down as usual)
N: exit without serving, with HANDOFF_STOOD_DOWN_EXIT_CODE (75)
```

Every frame read and write on both sides is bounded (`STEP`, 30 s; issue #207).
A peer that stalls mid-handoff used to block the other forever, and because O
ran the handoff inline in its main `select!`, a stalled successor also kept O
from servicing SIGTERM.

Every bound lives in one place, `kmux_protocol::control_rpc::handoff_timeouts`,
and none is configurable: both daemons (and `kmux daemon restart`) must agree
on them. `SUCCESSOR_CONNECT` (15 s) > `PREDECESSOR_CONNECT` (10 s), so a
successor gives up on the handoff socket while its predecessor still waits for
it; `HOLD` (5 s) and `FROZEN` (10 s) bound how long a pane can be frozen before a
rollback lets it go (plus the final checkpoint's own disk write, which is never
abandoned half-way: the write owns the checkpointer while it runs);
`PREDECESSOR_EXIT_GRACE` (15 s), `SUCCESSOR_STAND_DOWN_GRACE` (5 s) and
`PID_FILE_CLAIM` (10 s) bound the waits for the other daemon to exit;
`RESTART_WAIT` (120 s) is how long `kmux daemon restart` waits for either
outcome. Compile-time assertions keep the ones that must nest nested.

Key files: `crates/kmuxd/src/handoff/{mod,sender,receiver}.rs` (transport +
orchestration), `crates/kmuxd/src/app/migrate.rs` (`collect_handoff_panes`,
`quiesce_relays`), `crates/kmuxd/src/app/restore.rs` (`restore_with_handoff`,
`build_pane_relay`), `crates/kmux-pty/src/pty.rs` (`from_inherited`,
`dup_owned`), `crates/kmux-protocol/src/control_rpc.rs` (`HandoffMessage`,
`HANDOFF_PROTOCOL_VERSION`).

## Versioning

The handoff is a cross-component boundary — during an upgrade O and N may be
different builds — so it is versioned by `HANDOFF_PROTOCOL_VERSION`
(`kmux-protocol::control_rpc`). On a mismatch the successor sends `Decline` and
falls back to snapshot restore (always safe; the on-disk checkpoint is itself
versioned by `STATE_VERSION`). **Bump `HANDOFF_PROTOCOL_VERSION` on any change to
the `HandoffMessage` wire format.** Version 3 (issue #207) added O's `pid` to
`Hello` and made N O's direct child.

### Mixed versions

A successor of another version declines, and O then writes its final
checkpoint and answers `Released` (N restores it) or, if that write fails,
`Abort`. Two older successor builds do not fit that exactly, and O covers both
by stopping the successor itself rather than trusting it to stand down:

- A **version-1** N (the released protocol) restores and serves right after its
  `Decline`, without waiting for O's answer — so even when O commits, N may read
  the checkpoint before O's final write and briefly serve beside O until O
  exits. If O instead rolls back, it kills that N (by the pid its connection's
  peer credentials name) after the `Abort`, so two daemons serve only for the
  moment between N binding its sockets and O's kill.
- A **version-1 or -2** N daemonizes itself, so O's child is only its launcher
  and is gone at once. O knows the real N by its peer credentials once it
  connects, and kills it by that pid on a rollback. One that never connects at
  all cannot be found and stopped; a version-1 or -2 N in that state restores
  and serves as the pre-#207 code did. Only a downgrade to such a build is
  exposed; a version-3 N stands down whenever a daemon still answers the
  control socket.
- A **version-1 O** (the released daemon, upgrading to this build) spawns N
  without keeping its handle: a version-3 N that stands down stays a zombie of
  that O until O exits. Harmless, and nothing on the old side can change it.

**Upgrading from a released daemon does not keep shells.** Released daemons
speak handoff version 1, so the first restart onto this build is declined and
the successor snapshot-restores: running programs are restarted once, with
their screens replayed. Later restarts between version-3 builds keep them.

## Correctness invariants

- **No split reads.** O streams the fds while it is still the sole reader, then
  parks every pane's reader between two reads (`hold_relays`, a hold that can be
  released) and only then snapshots: an in-process pane's relay loop, and a
  worker pane's `kmux-vt-worker`, which parks on `WorkerRequest::Hold` and
  answers `Held` after every event for what it read, so the daemon-side mirror
  the checkpoint is taken from is complete. After the commit point it stops the
  parked readers (`quiesce_relays`: the relay tasks are aborted, the workers
  shut down). N starts reading strictly later (after `Released`). Output
  produced in the gap stays buffered in the kernel PTY and is drained by N. So
  no two readers ever race on a master, and no bytes are lost. A worker respawn
  waits for the handoff to end, so no fresh, unheld reader appears meanwhile.
- **Checkpoint before the commit point.** The final checkpoint is written,
  `fsync`ed and sealed (`Checkpointer::write_final_from`) before `Complete` is sent,
  so nothing fallible remains after N's `Ack` (issue #207). It used to be written
  after the `Ack`, and a failed write there was treated as a rollback although N
  already held every fd. The snapshot N seeds from reflects exactly the bytes O
  consumed; everything after sits unread in the kernel buffer for N.
- **Worker panes are held too** (issue #207). They used not to be: a
  `kmux-vt-worker` read its PTY on until the commit point, so what it consumed
  after the checkpoint was in neither N's seed nor N's kernel buffer. A worker
  blocked writing input to a child that does not read stdin cannot read the
  `Hold` until that write is done, so it fails the hold (`HOLD`, 5 s) and the
  handoff rolls back rather than splitting a read. Input for a held worker
  waits on the daemon's side until the hold is released, so a held worker is
  never blocked writing input when the `Release` comes.
- **Foreign-child exit.** N's inherited children are reparented to init and
  cannot be `waitpid`-ed, so exit is surfaced by the relay loop's PTY-EOF break
  (`session_diff_loop` → `SessionManager::notify_exited` → `PaneExited`), backed
  by a `kill(pid, 0)` liveness poll (`spawn_kill_poll_task`).
- **Closing an inherited pane.** The same poll is what close waits on:
  `SIGHUP`+`SIGTERM` to the process group, then after the grace period
  `SIGKILL` to the group, confirmed by the poll. `ECHILD` from a `waitpid` on a
  foreign child is never taken for an exit, so a shell that ignores `SIGTERM`
  cannot survive the close (issue #205; see `docs/daemon-lifecycle.md` §9.4).
- **Only the intended fds cross.** Every PTY master is close-on-exec, so the
  successor `N` inherits nothing by `exec`; it receives exactly the masters sent
  as `PaneFd`, over `SCM_RIGHTS`.
- **Seamless seed.** Inherited panes seed their emulator from the snapshot
  **without** the "[kmux: session restored]" separator (`SeedMode::Inherited`);
  respawned panes keep it (`SeedMode::Respawned`).

## Fault tolerance & idempotency

- **Commit point = N's `Ack`.** Before it, nothing irreversible has happened: O
  only sent `dup`s (it keeps its originals), its readers were *held*, not
  stopped, and its checkpoint was sealed, not relied on. Any failure — a frame
  that times out, a bad frame, readers that do not park within 5 s, a
  checkpoint that cannot be written — rolls back: O releases the readers,
  unseals the checkpoint, reopens pane creation, sends `Abort`, stops N (it
  waits `SUCCESSOR_STAND_DOWN_GRACE` for N to exit, then kills it), records why
  for `kmux daemon restart`, and resumes serving. The sender sets a shared
  `committed` flag the moment the `Ack` arrives.
- **After the commit** nothing can fail the handoff: keep-alive and stopping the
  readers are in memory, and a `Released` that cannot be sent is logged. N holds
  every live fd plus the checkpoint, so it completes the takeover even if O dies
  without sending `Released`.
- **Two daemons never serve at once.** N takes over only on O's word
  (`Released`) or once O is gone. `Abort` makes N exit without serving — even
  when O has exited by the time N reads it, since `Abort` says O did not commit.
  When N loses O mid-handoff instead (a timeout, EOF, a bad frame), it closes
  the socket — so an O still waiting on N fails its step and rolls back — and
  gives O `PREDECESSOR_EXIT_GRACE` (15 s) to exit: if O is still running, it is
  serving, and N exits; if O is gone, N restores the snapshot (lost before its
  `Ack`) or adopts the fds it holds (lost after). An N that never reaches the
  handoff socket (or has no runtime dir to find it in) has no O to ask, so it
  asks the control socket instead: a daemon still answering there is serving,
  and N stands down; nothing answering means O is gone, and N restores. O, for
  its part, kills an N that has not connected within `SUCCESSOR_CONNECT` before
  it rolls back, so a slow N cannot connect later and find nobody waiting.
- **Identity, not files.** N knows O by the pid in O's `Hello`, accepted only
  when it matches the socket's peer credentials (`SO_PEERCRED` on Linux,
  `LOCAL_PEEREPID` on macOS, via tokio's `peer_cred`; the peer must also run as
  the same user). A mismatch is refused: N stands down. N used to read O's pid
  from the pid file, which may be missing, stale, or name a reused pid. O knows N
  both as its child and by the same peer credentials.
- **Signals mid-handoff.** The handoff runs as a task of its own, so O's main
  loop still services SIGINT and SIGTERM. A shutdown mid-handoff does not abort
  the task: it fires the handoff's `Cancel` signal, and every step not yet past
  the commit point gives up at once — a shutdown wins a tie with an `Ack`
  already in the socket, so an `Ack` still unread does not commit. The handoff
  rolls back as above — `Abort`, then N stops — and O shuts down normally,
  writing its own final checkpoint with the checkpointer the handoff handed
  back. Both daemons stop. It used to abort the task, sending nothing: when the
  signal landed between N's `Ack` and O reading it, O killed the shells on its
  way out while N, finding O gone after its `Ack`, adopted their dead PTYs. A
  handoff that has already committed runs to its end (it cannot fail from
  there), and O exits as after any committed handoff; N serves.
- **Settling a finished handoff.** The main loop takes a handoff out of flight
  in the same poll that sees its task end, then makes the daemon consistent
  with how it ended inside that `select!` branch, so a signal arriving
  meanwhile finds no handoff in flight rather than awaiting a finished task
  again (which panicked, and skipped the shutdown checkpoint).
- **No pane is created mid-handoff.** Creating a session, tab, split or
  restored session holds a read guard on a gate from before its PTY spawns
  until the pane is in `sessions`; O takes the gate for writing before it
  advertises the panes (waiting for creations already under way) and holds it
  until the handoff ends. A creation meanwhile is refused with
  `KmuxError::HandoffInProgress` rather than waited on, since a pane made then
  would be neither handed over nor frozen for the final checkpoint. A rollback
  reopens the gate; a commit never does.
- **Concurrent restarts** are refused (`HandoffStatus` → `busy`).
- **Reporting a stand-down.** `restart` answers with the handoff's number, and
  the `handoff` control command reports whether it still runs and, once it has
  rolled back, why. `kmux daemon restart` polls it beside the pid and prints
  `handoff stood down: <why>` as soon as it knows, rather than timing out.
  When the old daemon is gone and nothing answers for 10 s, it says the daemon
  stopped during the handoff (a shutdown signal stops both daemons) rather
  than claiming the old one kept serving. Against a daemon that predates the
  report (it answers `restart` without a number) it asks nothing more and
  waits the old 15 s.
- **A pane whose child exits mid-handoff** is sent with `has_live_fd = false`
  (respawned from the snapshot) or, if it exits after N inherits it, is marked
  `Exited` via the EOF path.
- **An old daemon** that predates `restart` closes the control connection without
  replying; the client detects this and falls back to a hard stop-then-respawn
  (running shells do not survive that one-time fallback).

## Upgrading a running daemon (`mise run upgrade-daemon`, issue #36)

The handoff above is what makes a *live upgrade* possible: ship a new `kmuxd`
build and restart onto it without dropping the shells it hosts. The
`mise run upgrade-daemon` task does exactly that:

1. `cargo build --release -p kmuxd` — also refreshes the build-tree
   `libkmux_ghostty` the installed binary's rpath points at, keeping the new
   daemon ABI-matched (`kmux-ghostty-sys` `EXPECTED_ABI_VERSION`).
2. `cargo install --path crates/kmuxd` — **atomically replaces**
   `~/.cargo/bin/kmuxd` in place.
3. `kmux daemon restart` — drives the handoff above; the successor is the new
   binary.

Two mechanics are load-bearing:

- **The successor runs the *new* code only if the running daemon's own binary
  path was replaced.** `spawn_successor` re-execs the running daemon's binary
  (`handoff::sender::resolve_successor_exe`), not the install target. So an
  in-place upgrade takes effect when the running daemon *is* the installed
  `~/.cargo/bin/kmuxd`; a dev daemon launched from `target/debug/kmuxd` would
  re-exec the debug build. After the atomic replace, `resolve_successor_exe`
  handles the platform split: macOS keeps the path (now the new inode); Linux's
  `/proc/self/exe` reads back as `"<path> (deleted)"`, which it strips and
  resolves to the replacement so the new code runs rather than `ENOENT`-ing.
- **The outgoing daemon must fully exit.** Its migrated PTY children are kept
  alive. They used to park a `waitpid` thread each in the runtime's blocking
  pool, which dropping the runtime would *join* and hang on forever. Since
  issue #205 one reaper thread outside the runtime watches every child, so
  nothing in the pool waits on them; `main` still shuts the runtime down with
  `Runtime::shutdown_background()` so that no other stuck blocking task can
  defeat issue #36's "old daemon completely shut-off".

Across a version bump the handoff degrades safely: a `HANDOFF_PROTOCOL_VERSION`
mismatch → `Decline` → snapshot restore (version 2, issue #207, added `Abort` and
has N wait for `Released` after a `Decline`; a version-1 O answers a `Decline`
with `Released` too, and a version-1 N restores without waiting; version 3 adds
O's pid to `Hello`, so the upgrade from a released, version-1 daemon — like one
from a version-2 build — snapshot-restores once: shells do not survive that
restart; see §Mixed versions); a `PROTOCOL_VERSION` mismatch is caught
by the client on reconnect (it surfaces the documented "run `kmux daemon
restart`" guidance); the on-disk checkpoint is versioned by `STATE_VERSION`.

QA for the full upgrade surface — real workloads, the version-bump matrix,
failure injection, and resource hygiene — is tracked in
[`qa-daemon-upgrade.md`](qa-daemon-upgrade.md), backed by the automated
cross-process tests in `crates/kmuxd/tests/handoff_e2e.rs`.

## Out of scope

- **Listening sockets / the QUIC endpoint are not migrated.** Ephemeral ports and
  the auth token rotate; connected clients reconnect via the existing logic
  (re-auth with the adopted token, re-attach with `last_seqno`). The successor
  adopts the predecessor's token so re-auth is seamless. True zero-downtime
  *client* connections (passing listener fds) is a possible future follow-up.
- `relay.rs::foreground_process_name` still reads `/proc/<pgid>/comm` (Linux-only
  title polling) — a pre-existing limitation, unrelated to the handoff.
