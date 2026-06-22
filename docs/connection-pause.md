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

## Wire protocol

`ClientMessage::SetPaused { paused, auto }` — connection-level (all of the
client's panes), mirroring `SetSnapshotMode`. The original pause shipped in
protocol v24 (`SetPaused { paused }`, bumped 23 → 24).

**Reason-aware pause + per-pane exemption (v32).** `SetPaused` gained an `auto`
flag and a companion `ClientMessage::SetPaneNoAutoPause { pane_id, exempt }`
arrived, so a backgrounded client can keep streaming chosen panes while pausing
the rest (`PROTOCOL_VERSION` 31 → 32):

- `auto = true` is the debounced background auto-pause; `auto = false` is an
  explicit manual pause. When both sources are active the client sends
  `auto: false` (a manual pause wins). Ignored when `paused = false`.
- `SetPaneNoAutoPause` marks one pane **exempt from auto-pause** for this
  client. An exempt pane keeps streaming through an auto-pause; a *manual* pause
  still stops it. The exemption is a per-client preference and is **not**
  persisted across a re-attach — the client re-asserts it after each `Attach`.
  Session-level exemption is a client-side grouping that expands to one
  `SetPaneNoAutoPause` per pane (no separate wire message).

The daemon's per-pane skip rule is therefore:

```text
withhold output = paused && !(auto && pane.no_auto_pause)
```

There is no dedicated resume message: on resume the client re-issues the
existing `Attach { pane_id, last_seqno: None, size }` per visible pane, which
flows through the daemon's normal snapshot-attach path.

## Daemon (`kmuxd`)

- `ClientSender.paused: bool` + `pause_auto: bool` + per-pane `no_auto_pause: bool`.
  `ServerApp::set_paused(client_id, paused, auto)` sets the first two across all
  of the client's panes (mirrors `set_snapshot_mode`);
  `ServerApp::set_pane_no_auto_pause(client_id, pane_id, exempt)` sets the third
  on one pane. `ClientSender::output_paused()` evaluates the skip rule above.
- `broadcast_to_clients`, `broadcast_resize`, and the post-respawn recovery
  snapshot skip a client whose `output_paused()` is true: no frames are sent,
  and a paused client is **never** marked `Lagged` or dropped even if its bounded
  data channel fills. An auto-pause-exempt pane keeps streaming through a
  background pause. The client catches up on resume.
- A paused client **still counts toward the effective (smallest-wins) pane size**,
  so pausing never reflows the PTY for other attached clients.
- `attach()` **preserves connection-level flags** (`force_full_snapshot`) across a
  re-attach but **resets `paused` and `no_auto_pause`** (a re-attach means the
  client is live again; it re-asserts any exemption right after attaching).
- `compute_replay` adds a **coalescing threshold**: a delta attach whose buffered
  diffs exceed `MAX_RESUME_DELTA_{DIFFS,BYTES}` returns a single snapshot
  (`SyncReset`) rather than replaying — a defensive bound for the delta path.
- **Federation** mirrors all of this: a proxied-pane `Viewer` carries `paused`,
  `pause_auto`, and `no_auto_pause`; `fan_out` honors `Viewer::output_paused()`,
  and the same reason/exemption setters apply (so pausing a GUI viewing a proxied
  remote session works pane-by-pane too).

## Client / app

- `SessionManager::reconcile_pause(paused, auto)` is the single sync point: it
  sends `SetPaused { paused, auto }` only when the connection state changes, then
  re-attaches via `attach_fresh` exactly the visible panes that transition from
  *withheld* to *streaming* (the old "resume re-attaches all visible" is the
  special case). Pause sends **no `Detach`** — panes stay attached server-side;
  pause is purely a broadcast skip. (A delta-replay resume is intentionally *not*
  used: the client's sync model only establishes sync from a snapshot, and a
  snapshot is both robust and the minimal "final state only" payload.)
- The auto-pause exemption lives on the `SessionManager` (`auto_pause_exempt_panes`
  / `auto_pause_exempt_sessions`), so `attach_fresh` re-asserts it after each
  `Attach`. `pause_applied` mirrors the last `(paused, auto)` pushed, and is reset
  on disconnect so a reconnect re-sends.
- `AppCore` holds two independent pause sources — `manual_pause` (the
  `TogglePause` action) and `auto_pause` (window backgrounded) — whose **OR** is
  the effective state (`is_paused`). A manual pause **persists across focus
  changes**; auto clears on its own. `pause_reason()` (`PauseReason`) drives the
  global status indicator; `is_pane_paused(pane_id)` / `pane_pause_reason(pane_id)`
  drive the per-pane + per-tab markers (honoring exemptions). The toggles are
  `toggle_{focused_pane,active_session}_no_auto_pause` (and explicit-target
  `toggle_{pane,session}_no_auto_pause` for context menus).

## Per-pane / per-tab indication + force-disable (issue #68 follow-up)

- **Indicators.** A paused tab is prefixed with a pause glyph (GTK `tab_label`,
  Swift `FfiTab.paused`), and each paused pane shows a small ⏸ badge in its
  top-right corner on the rendered terminal (GTK `render::paint_pause_badge`
  on the Cairo path; Swift `drawPauseBadge` in `TerminalView`). A tab is "paused"
  if any of its panes is.
- **Force-disable = "keep streaming in background".** A pane or session can be
  marked exempt from auto-pause via a context-menu toggle (GTK pane menu +
  primary-menu session item; Swift pane and session context menus, with a
  checkmark from `FfiPaneRect.no_auto_pause` / `KmuxDriver::session_no_auto_pause`).
  The semantics are **auto-pause only**: an exempt pane keeps streaming when the
  window is backgrounded, but a manual `Ctrl+Shift+B` / `⌘⇧B` still pauses
  everything.

## Auto-pause on background

The `FrontendDriver` exposes `set_window_background(bool)`; the frontend reports
window lifecycle and the driver applies a **debounce** (`AUTO_PAUSE_DEBOUNCE`,
1 s) before auto-pausing, resuming **immediately** on foreground. Focus loss
alone does **not** pause — a visible-but-unfocused window keeps streaming so the
user can still watch; only a hidden/minimized/backgrounded window pauses.

- **GTK** (`kmux-gtk`): watches the toplevel `GdkSurface` state for `MINIMIZED`.
  Manual toggle: `Ctrl+Shift+B` / the "Pause Connection" menu item. The
  "Keep Streaming in Background" pane (context menu) + session (primary menu)
  toggles set the exemption. Pause shows in the header subtitle plus the per-tab
  and on-screen markers.
- **Swift** (`kmux-swift`): drives `set_window_background` from SwiftUI
  `scenePhase` (`.background` pauses; `.inactive` keeps streaming). Manual toggle:
  `⌘⇧B` / the "Pause Connection" menu item. "Keep Streaming in Background" lives
  in the pane and session context menus. Pause shows as a pill in the connection
  badge plus the per-tab and on-screen markers.
- FFI: `FfiAction::{TogglePause, ToggleFocusedPaneNoAutoPause,
  ToggleActiveSessionNoAutoPause}`, `KmuxDriver::{set_window_background,
  toggle_pane_no_auto_pause, toggle_session_no_auto_pause, session_no_auto_pause,
  pause_state}`, and per-pane `FfiPaneRect.{paused, no_auto_pause}` /
  `FfiTab.paused`. `KMUX_FFI_ABI_VERSION` is 18.

## Verify

1. Open a session; toggle pause (`Ctrl+Shift+B` on GTK, `⌘⇧B` on macOS). The
   indicator shows "Paused", **every** tab gets a ⏸ marker, and **every** pane
   draws an on-screen ⏸ badge.
2. Generate heavy output in the paused pane (`yes`, `seq 1 1000000`); confirm no
   terminal traffic (HUD/metrics flat).
3. Toggle again — the markers clear, the screen is correct immediately, and
   scrollback is complete (scroll up; `FetchHistory` fills paused-era lines).
4. Minimize the window → auto-pause after ~1 s; restore → auto-resume + instant
   catch-up. A manual pause is **not** cleared by focus changes.
5. Mark a pane "Keep Streaming in Background" (pane context menu), then minimize:
   that pane keeps updating while the rest auto-pause; restore → only the
   non-exempt panes catch up. A manual pause still pauses the exempt pane too.
