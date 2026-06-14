# QA matrix — live daemon upgrade (issue #36)

Manual validation plan for upgrading a running `kmuxd` in place
(`mise run upgrade-daemon`) without dropping the shells it hosts. It complements the
automated coverage and covers the surfaces that are impractical to assert in a
hermetic test (real interactive workloads, both GUI clients, deliberate version
bumps, failure injection).

Run the whole matrix on **Linux (GTK client, `kmux-gtk`)** and **macOS (Swift app
`kmux-swift` + GTK)**. Record pass/fail per row. See
[`daemon-handoff.md`](daemon-handoff.md) for the mechanism.

## Automated coverage (run first)

| Test | Location | Asserts |
| --- | --- | --- |
| `live_restart_preserves_running_shell_across_processes` | `crates/kmuxd/tests/handoff_e2e.rs` | A real cross-process `restart` migrates a live shell (same PID), session persists, old daemon exits. |
| `in_place_binary_swap_still_hands_off` | `crates/kmuxd/tests/handoff_e2e.rs` | After an atomic in-place binary replace, a successor still takes over (the `(deleted)`-inode regression). |
| `handoff::sender::tests::*` | `crates/kmuxd/src/handoff/sender.rs` | `resolve_successor_exe` strips the `(deleted)` marker, prefers the replacement, errors when nothing exists. |
| `restart_daemon_maps_accepted_busy_and_unsupported` | `crates/kmux-client/src/daemon/mod.rs` | The `restart` RPC contract: accepted / busy / old-daemon-fallback. |
| `reconnect_preserves_connection_id_for_handoff` | `crates/kmux-client/src/session_manager/mod.rs` | The client keeps its `connection_id` across the drop so the successor can transfer pane streams. |
| `live_pty_migrates_with_same_pid` (pre-existing) | `crates/kmuxd/src/app/migrate.rs` | In-process fd transfer keeps the same child PID. |
| `pane_fd_round_trips_and_keeps_child_alive`, `hello_version_mismatch_round_trips_for_decline` (pre-existing) | `crates/kmuxd/src/handoff/mod.rs` | SCM_RIGHTS fd passing; version-mismatch → decline. |

Run: `mise run test` (or `cargo test -p kmuxd -p kmux-pty -p kmux-client`).

> **Note on what is *not* automated:** a successor built with a *different*
> `HANDOFF_PROTOCOL_VERSION`/`STATE_VERSION` (needs a second build) and the real
> GUI-client reconnect UX are covered manually below, not in CI.

## A. Real workloads survive the upgrade

Setup: `mise run install`, start the GUI, then in the steps below run
`mise run upgrade-daemon` from a separate terminal and observe.

| # | Workload in a live pane | Steps | Expected |
| --- | --- | --- | --- |
| A1 | Interactive `vim` (or `nvim`) with a file open, in insert mode | upgrade | Editor still running (same process), screen redraws intact, no lost keystrokes, no `[kmux: session restored]` banner (live migration, not respawn). |
| A2 | `tail -f` on a file being appended to by a second process | upgrade | Output stream continues; no gap, no duplicated lines, no dropped lines across the quiesce window. |
| A3 | A long build / `cargo build` / `make` running | upgrade | Build keeps running to completion; same PID; output continues. |
| A4 | An `ssh user@host` session with a remote shell | upgrade | SSH session stays connected; remote shell responsive afterward. |
| A5 | A REPL (`python3`, `node`) with in-memory state | upgrade | REPL process survives; previously-defined variables still present. |
| A6 | A process emitting output *during* the quiesce window (e.g. `yes` \| `head -n 1000000`) | upgrade | All bytes accounted for — the gap is drained from the kernel PTY buffer, nothing lost or doubled. |

## B. Layout & state fidelity

| # | Surface | Expected after upgrade |
| --- | --- | --- |
| B1 | Multiple sessions | All sessions still listed (`kmux daemon status` session count unchanged). |
| B2 | Multiple tabs + split panes in one session | Tab set and tiling layout preserved exactly. |
| B3 | Scrollback | Scrollback contents identical before/after (spot-check a known line near the top). |
| B4 | Active session / pane / titles | Focus and pane/tab titles preserved. |
| B5 | `kmux daemon status` | Reports a **new PID**, the **new `kmuxd_version`**, same session count, matching protocol + profile. |

## C. Client reconnect UX (per client)

| # | Surface | Expected |
| --- | --- | --- |
| C1 | GTK client (`kmux-gtk`) during the brief outage | Freezes gracefully then reconnects (seamless, or via the `Mode::Disconnected` confirm path); no crash. |
| C2 | Swift app (`kmux-swift`) during the brief outage | Same — reconnects, no crash. |
| C3 | After reconnect | Cursor shape/blink, clipboard, and input all behave; panes resume live output. |

## D. The crux: in-place upgrade actually swaps code

| # | Steps | Expected |
| --- | --- | --- |
| D1 (Linux & macOS) | Run the installed `~/.cargo/bin/kmuxd` (via the GUI). Bump the workspace version (or make a visible change), `mise run upgrade-daemon`. | `kmux daemon status` shows the **new** `kmuxd_version`; sessions survived. On Linux this is the `/proc/self/exe (deleted)` path — confirm it does **not** silently keep the old version. |
| D2 | Confirm the old daemon is gone | After the upgrade, the previous PID is no longer alive (`ps -p <old_pid>`); exactly one `kmuxd` runs. |

## E. Version-bump matrix (deliberately break compatibility, one at a time)

Build a successor with a changed constant, install it, then upgrade onto it.

| # | Bump | Constant | Expected |
| --- | --- | --- | --- |
| E1 | Handoff protocol | `HANDOFF_PROTOCOL_VERSION` (`kmux-protocol/src/control_rpc.rs`) | Successor `Decline`s the live fd transfer → snapshot restore. Picture preserved; shells **respawned** (the `[kmux: session restored]` banner appears, per `SeedMode::Respawned`). |
| E2 | Wire protocol | `PROTOCOL_VERSION` (`kmux-protocol/src/messages/types.rs`) | Client reconnect surfaces the documented mismatch + "run `kmux daemon restart`" guidance rather than corrupting the session. |
| E3 | Checkpoint schema | `STATE_VERSION` (`kmuxd/src/persist/mod.rs`) | A stale checkpoint is rejected cleanly (no panic); fresh sessions start. |
| E4 | Library ABI | `kmux-ghostty-sys` `EXPECTED_ABI_VERSION` | A mismatched `libkmux_ghostty` is detected/refused at load rather than crashing the daemon. |

## F. Failure injection & fault tolerance

Cross-check against the invariants in [`daemon-handoff.md`](daemon-handoff.md).

| # | Inject | Expected |
| --- | --- | --- |
| F1 | Successor fails to connect within 15s (e.g. make it exit immediately) | Predecessor rolls back, clears `handoff_in_progress`, keeps serving — **no session loss**. |
| F2 | Successor crashes *after* the commit (`Ack`) | Takeover still completes (it holds every fd + the checkpoint). |
| F3 | Two `kmux daemon restart` concurrently | Second is refused with `busy` (`restart_daemon` → `Ok(false)`). |
| F4 | A daemon predating `restart` (old build) | Client falls back to hard stop-then-respawn; shells do **not** survive (documented one-time cost). |
| F5 | A pane whose child exits mid-handoff | Sent with `has_live_fd = false` (respawned), or marked `Exited` if it dies after the successor inherits it. |

## G. Resource hygiene

| # | Surface | Expected |
| --- | --- | --- |
| G1 | Many panes (e.g. 20+) | All migrate; no fd-exhaustion errors. |
| G2 | Repeated upgrades (loop `mise run upgrade-daemon` 5–10×) | Sessions persist each time; daemon open-fd count is stable across iterations (`lsof -p <pid> \| wc -l`) — no leak. |
| G3 | Idempotency | Back-to-back upgrades with no intervening activity converge (no duplicate daemons, no orphaned `kmuxd`/shell processes). |
