# Connection pausing (bandwidth saving)

Issue #68. A client can **pause** receiving terminal output to save bandwidth —
useful on metered/slow links or when the window is in the background. While
paused the pane keeps running on the daemon; on resume the client catches up
**instantly** to the *final* state instead of replaying a backlog.

## Design principle

The daemon's Ghostty-backed VT (`term_state`) is the source of truth for the
screen, and the per-pane scrollback mirror/`DiffBuffer` is the source of truth
for history. Both keep advancing while a client is paused (the per-pane
`session_diff_loop` runs independently of clients — sessions persist while fully
detached). So "buffering frames" already exists; pausing just stops *pushing* to
one client, and resume reconciles from the live daemon state.

**Resume catch-up is O(screen size), not O(time paused):** the client re-attaches
its visible panes and the daemon replies with one snapshot of the final state —
never a frame-by-frame replay. This is also the truest realization of the
issue's "reconcile on the daemon so only the final state is sent over."

## Wire protocol (v24)

`ClientMessage::SetPaused { paused }` — connection-level (all of the client's
panes), mirroring `SetSnapshotMode`. `PROTOCOL_VERSION` was bumped 23 → 24.

There is no dedicated resume message: on resume the client re-issues the
existing `Attach { pane_id, last_seqno: None, size }` per visible pane, which
flows through the daemon's normal snapshot-attach path.

## Daemon (`kmuxd`)

- `ClientSender.paused: bool`. `ServerApp::set_paused(client_id, paused)` sets it
  across all of the client's panes (mirrors `set_snapshot_mode`).
- `broadcast_to_clients` **and** `broadcast_resize` skip paused clients: no frames
  are sent, and a paused client is **never** marked `Lagged` or dropped even if
  its bounded data channel fills. It catches up on resume.
- A paused client **still counts toward the effective (smallest-wins) pane size**,
  so pausing never reflows the PTY for other attached clients.
- `attach()` now **preserves connection-level flags** (`force_full_snapshot`)
  across a re-attach instead of resetting them (this also fixed a latent bug for
  snapshot-mode clients), and clears `paused` (a re-attach means the client is
  live again).
- `compute_replay` adds a **coalescing threshold**: a delta attach whose buffered
  diffs exceed `MAX_RESUME_DELTA_{DIFFS,BYTES}` returns a single snapshot
  (`SyncReset`) rather than replaying — a defensive bound for the delta path.

## Client / app

- `SessionManager::set_paused(paused)` sends `SetPaused`; on resume it re-attaches
  every visible pane via `attach_fresh` (full snapshot of the final state). Pause
  sends **no `Detach`** — panes stay attached server-side; pause is purely a
  broadcast skip. (A delta-replay resume is intentionally *not* used: the client's
  sync model only establishes sync from a snapshot, and a snapshot is both robust
  and the minimal "final state only" payload.)
- `AppCore` holds two independent pause sources — `manual_pause` (the
  `TogglePause` action) and `auto_pause` (window backgrounded) — whose **OR** is
  the effective state (`is_paused`). A manual pause **persists across focus
  changes**; auto clears on its own. `pause_reason()` (`PauseReason`) drives the
  status indicator.

## Auto-pause on background

The `FrontendDriver` exposes `set_window_background(bool)`; the frontend reports
window lifecycle and the driver applies a **debounce** (`AUTO_PAUSE_DEBOUNCE`,
1 s) before auto-pausing, resuming **immediately** on foreground. Focus loss
alone does **not** pause — a visible-but-unfocused window keeps streaming so the
user can still watch; only a hidden/minimized/backgrounded window pauses.

- **GTK** (`kmux-gtk`): watches the toplevel `GdkSurface` state for `MINIMIZED`.
  Manual toggle: `Ctrl+Shift+B` / the "Pause Connection" menu item. Pause shows in
  the header subtitle.
- **Swift** (`kmux-swift`): drives `set_window_background` from SwiftUI
  `scenePhase` (`.background` pauses; `.inactive` keeps streaming). Manual toggle:
  `⌘⇧B` / the "Pause Connection" menu item. Pause shows as a pill in the
  connection badge.
- FFI: `FfiAction::TogglePause`, `KmuxDriver::set_window_background`, and
  `pause_state() -> FfiPauseState`. `KMUX_FFI_ABI_VERSION` bumped 9 → 10.

## Verify

1. Open a session; toggle pause (`Ctrl+Shift+B` on GTK, `⌘⇧B` on macOS). The
   indicator shows "Paused".
2. Generate heavy output in the paused pane (`yes`, `seq 1 1000000`); confirm no
   terminal traffic (HUD/metrics flat).
3. Toggle again — the screen is correct immediately and scrollback is complete
   (scroll up; `FetchHistory` fills paused-era lines).
4. Minimize the window → auto-pause after ~1 s; restore → auto-resume + instant
   catch-up. A manual pause is **not** cleared by focus changes.
