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
  `SIGKILL` during the brief handoff overlap.
- **`/proc/<pid>/fd` reattach** was Linux-only (broken on macOS). `SCM_RIGHTS`
  is POSIX and works identically on Linux and macOS.

## Sequence (O = outgoing, N = incoming)

```
client: kmux daemon restart ──restart──▶ O (control socket)
O: bind handoff.sock ; spawn  N = current_exe + DAEMON_BOOT_ARGS + --handoff
N: daemonize (no pid file) ; connect handoff.sock
O ──Hello{version, token, panes}──▶ N
N: version ok?  ──Accept──▶ O          (else ──Decline──▶ O, N snapshot-restores)
loop over live panes (lock-step):
  O ──PaneFd{pane_id} + master fd (SCM_RIGHTS)──▶ N
  N ──PaneFdAck──▶ O
O ──Complete──▶ N
N ──Ack──▶ O                            ◀── COMMIT POINT
O: set_all_keep_alive ; quiesce relays ; write checkpoint
O ──Released──▶ N ; O exits (releases listeners, control/data sockets, pid file)
N: restore_with_handoff(checkpoint, inherited fds) ; bind sockets ; claim pid file ; serve
client: reconnect (new ports, adopted token) ; re-attach with last_seqno
```

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
the `HandoffMessage` wire format.**

## Correctness invariants

- **No split reads.** O streams the fds while it is still the sole reader, then —
  *after* the commit point — calls `quiesce_relays()` (abort + await every relay
  read task) and only then snapshots. N starts reading strictly later (after
  `Released`). Output produced in the gap stays buffered in the kernel PTY and is
  drained by N. So no two readers ever race on a master, and no bytes are lost.
- **Checkpoint after quiesce.** The snapshot N seeds from reflects exactly the
  bytes O consumed; everything after sits unread in the kernel buffer for N.
- **Foreign-child exit.** N's inherited children are reparented to init and
  cannot be `waitpid`-ed, so exit is surfaced by the relay loop's PTY-EOF break
  (`session_diff_loop` → `SessionManager::notify_exited` → `PaneExited`), backed
  by a `kill(pid, 0)` liveness poll (`spawn_kill_poll_task`).
- **Seamless seed.** Inherited panes seed their emulator from the snapshot
  **without** the "[kmux: session restored]" separator (`SeedMode::Inherited`);
  respawned panes keep it (`SeedMode::Respawned`).

## Fault tolerance & idempotency

- **Commit point = N's `Ack`.** Before it, nothing destructive has happened (O
  never stopped reading and only sent `dup`s — it keeps its originals), so any
  failure rolls back: O clears its in-progress flag and resumes serving.
- **After the commit** N holds every live fd plus the checkpoint, so it completes
  the takeover even if O dies without sending `Released` (detected via socket EOF
  / pid death).
- **Concurrent restarts** are refused (`handoff_in_progress` flag → `busy`).
- **A pane whose child exits mid-handoff** is sent with `has_live_fd = false`
  (respawned from the snapshot) or, if it exits after N inherits it, is marked
  `Exited` via the EOF path.
- **An old daemon** that predates `restart` closes the control connection without
  replying; the client detects this and falls back to a hard stop-then-respawn
  (running shells do not survive that one-time fallback).

## Out of scope

- **Listening sockets / the QUIC endpoint are not migrated.** Ephemeral ports and
  the auth token rotate; connected clients reconnect via the existing logic
  (re-auth with the adopted token, re-attach with `last_seqno`). The successor
  adopts the predecessor's token so re-auth is seamless. True zero-downtime
  *client* connections (passing listener fds) is a possible future follow-up.
- `relay.rs::foreground_process_name` still reads `/proc/<pgid>/comm` (Linux-only
  title polling) — a pre-existing limitation, unrelated to the handoff.
