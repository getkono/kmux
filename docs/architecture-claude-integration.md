# Claude Code integration — focus-on-turn-done notifications (issue #169)

When a long-running agent such as **Claude Code** runs inside a kmux pane, the
user often switches away while it works. kmux can raise a **native desktop
notification** when the agent finishes a turn or becomes blocked on input, and
**clicking it refocuses the right kmux window** for that session and selects the
pane — even when several windows of one GUI process show the same session.

The trigger is generic: any program in a pane can run `kmux notify`. Claude Code
is the motivating caller, wired through its `Stop` / `Notification` hooks.

## End-to-end flow

```
Claude Code hook ──run──▶ kmux notify ──ClientMessage::Notify──▶ daemon
   (Stop/Notification)      (in the pane)                          │
                                                                   │ broadcast
                                          SessionEventMsg::PaneAttention
                                                                   │ (to all clients)
                                                                   ▼
                                   GUI client(s): dedup on attention_id,
                                   post ONE native notification ──click──▶
                                   focus best window + select session/pane
```

1. **Identity in the pane.** The daemon exports `KMUX_PANE=<word>/<idx>` and
   `KMUX_SESSION=<word>` into every pane (`crates/kmuxd/src/capability.rs`,
   `pane_spawn_env`), so a program inside it knows which pane it occupies.
2. **`kmux notify`** (`crates/kmux-app/src/subcommands/notify.rs`) reads those
   vars, connects to the daemon hosting the pane (the local one — the hook runs
   co-located with its pane), runs the identity handshake, and sends
   `ClientMessage::Notify { pane_id, kind, title, body }`.
3. **The daemon** validates the pane, allocates a monotonic `attention_id`, and
   broadcasts `SessionEventMsg::PaneAttention` to **all** connected clients
   (`ServerApp::notify_pane_attention`), then replies `NotifyAccepted`.
4. **Each GUI client** turns it into a `FrontendEffect::Attention` /
   `FfiEffect::Attention` and the frontend posts a notification.

## Why `attention_id` (dedup)

The daemon broadcasts session events to every connected client with no
filtering. A single GUI process hosts multiple windows, each with its **own
daemon connection**, so all of them receive the same `PaneAttention`. Each
frontend keeps a small bounded set of seen `attention_id`s and posts **exactly
one** notification per id. The id is assigned server-side so it is identical
across those connections.

## Window selection (click → focus)

A click must pick one window. Both frontends apply the same policy:

1. a **visible** window already showing the session (the active/key one if
   several do), else
2. the active/key window (it switches to the session on click), else
3. any visible window.

The chosen window is raised, switched to the session
(`JumpToSession`/`select_session`), and the pane is selected
(`TopBarAction::SelectPane`). Selection happens at **click** time, so it
reflects the current window layout, not the layout when the notification was
posted.

## Per-platform notification backends

- **Linux / GNOME** (`crates/kmux-gtk/src/imp/attention.rs`): a `gio::Notification`
  with an app-scoped action `app.kmux-attention-focus` carrying `(word_id,
  pane_id)`. `NeedsInput` maps to `Urgent` priority. Windows register in a
  `thread_local` table (GTK is single-threaded on the main loop) and deregister
  on close.
- **macOS** (`kmux-swift/Sources/KmuxApp/AttentionCoordinator.swift`): a
  `UNUserNotificationCenter` request routed through a singleton coordinator that
  also dedups and owns the click delegate. `NeedsInput` uses a time-sensitive
  interruption level. Notifications are skipped for the bare-executable dev path
  (no bundle id, where `UNUserNotificationCenter` would trap); they work from the
  installed `kmux.app`.

## Wiring Claude Code

Add to `~/.claude/settings.json` (or a project `.claude/settings.json`). A single
`kmux notify` handles both hooks — the hook payload piped on stdin selects the
kind (`Stop`/`SubagentStop` → turn-done, `Notification` → needs-input) and fills
the body:

```json
{
  "hooks": {
    "Stop": [
      { "hooks": [{ "type": "command", "command": "kmux notify" }] }
    ],
    "Notification": [
      { "hooks": [{ "type": "command", "command": "kmux notify" }] }
    ]
  }
}
```

Flags override the inferred values: `kmux notify --kind needs-input --title …
--body …`. Outside a kmux pane (`KMUX_PANE` unset) it errors unless `--pane
<word/idx>` is given.

## Versioning

The wire and FFI boundaries bump together (see the repo's "Correctness"
invariants): `PROTOCOL_VERSION` for `ClientMessage::Notify` +
`SessionEventMsg::PaneAttention` + `AttentionKind`, and `KMUX_FFI_ABI_VERSION`
for `FfiEffect::Attention` + `FfiAttentionKind`.

## Follow-ups

- **`kmux://focus` URL scheme.** A click is handled in-process today (the GUI
  client that posted the notification owns the window and its daemon
  connection), so the URL scheme is not required. A `kmux://focus?session=…&pane=…`
  handler — registered on macOS via the existing `CFBundleURLTypes` and on Linux
  via an `x-scheme-handler` in the `.desktop` file — would let *external*
  triggers refocus a session; it is deferred.
