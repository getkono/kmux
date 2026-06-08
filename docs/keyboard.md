# Keyboard Input

kmux's input path forwards modifier-aware key events from the client to
the daemon, which encodes them using the live state of the in-pane Ghostty
terminal emulator.  This means modified keys (Shift+Enter, Alt+Enter,
Shift+Tab, Ctrl+Arrow, …) are encoded with whatever protocol the inner
program negotiated — kitty keyboard, xterm modifyOtherKeys, or legacy.

## Why server-side encoding

Each running shell or TUI inside a kmux pane can negotiate one of several
keyboard protocols:

- **Kitty keyboard protocol** (`CSI > N u` / `CSI < N u`) — apps like
  Claude Code and helix enable this for unambiguous modifier reporting.
- **xterm modifyOtherKeys** (`CSI > 4 ; N m`) — older convention used by
  many TUI editors when Kitty isn't available.
- **DECCKM** (DEC private mode 1) — controls whether arrow keys send
  `CSI O <A-D>` or `CSI [ <A-D>`.
- **DECKPAM** — controls keypad-application mode.

The right byte sequence for, say, Shift+Enter depends on which of these
the inner program enabled.  Encoding client-side would mean duplicating
Ghostty's `key_encode` logic and tracking remote pane state across the
wire.  Encoding server-side, using `ghostty_key_encoder_*` against the
live pane terminal, eliminates that drift entirely.

## Wire protocol

```
ClientMessage::PtyKey      { pane_id, event: KeyEvent }
ClientMessage::PtyKeyBatch { pane_id, events: Vec<KeyEvent> }
```

A `KeyEvent` carries:

- `code: KeyCode` — physical key (kmux-stable enum, mirrors the Zig
  `KmuxKey` enum and Ghostty's `gvt.input.Key`).
- `mods: KeyMods` — Shift / Ctrl / Alt / Super bitmask.
- `action: KeyAction` — Press or Repeat (Release events are dropped on
  the client side; kmux does not enable `REPORT_EVENT_TYPES`).
- `text: String` — UTF-8 the key would produce in a plain text field.
- `unshifted_codepoint: u32` — codepoint without shift, for kitty
  alternates (0 = unknown).

Raw byte writes still go through `PtyInput` for paste, mouse-report wheel
events, and signal-injection paths where the client already has bytes.

## Client side

The GUI frontend receives key events from its toolkit (GDK on GTK, AppKit on
macOS) and translates them into the wire `KeyEvent`. In `kmux-gtk` this is
`crates/kmux-gtk/src/imp/convert.rs::convert_to_protocol_key`; the Swift app
goes through the FFI `send_char` / `send_named_key`. Letters and digits are
mapped to their dedicated physical-key variants so kitty's "report alternates"
works correctly; other printables fall through as `KeyCode::Unidentified` plus
the text, letting the daemon's encoder write the UTF-8 directly. The toolkit
reports modified keys (Shift+Enter, Shift+Tab) as distinguishable events, so the
daemon can encode them under whatever protocol the inner program negotiated.

## Daemon side

`crates/kmuxd/src/backend/ghostty/mod.rs::GhosttyBackend::encode_key_event`
queries the live `gvt.input.KeyEncodeOptions` from the pane's Ghostty
terminal (kitty flags via the new `kmux_ghostty_kitty_flags` FFI getter,
DECCKM via `kmux_ghostty_modes`, etc.) and calls
`kmux_ghostty::encode_key`.  The result is written to the PTY along with
all other batched bytes.

For `PtyKeyBatch`, encoding happens under the term-state lock so a mode-
mutating sequence emitted by an earlier event in the batch is visible to
later ones.

## Examples

| Inner program emitted | User keystroke | Wire event | Encoded bytes |
|---|---|---|---|
| (nothing) | Enter | `KeyCode::Enter, mods: 0` | `\r` |
| (nothing) | Shift+Tab | `KeyCode::Tab, mods: SHIFT` | `\x1b[Z` (CBT) |
| (nothing) | Shift+Enter | `KeyCode::Enter, mods: SHIFT` | `\x1b[27;2;13~` (modifyOtherKeys) |
| `\x1b[>1u` (kitty kbd) | Shift+Enter | `KeyCode::Enter, mods: SHIFT` | `\x1b[13;2u` (CSI u) |
| `\x1b[>1u` (kitty kbd) | Alt+Enter | `KeyCode::Enter, mods: ALT` | `\x1b[13;3u` |
| (nothing) | Ctrl+Up | `KeyCode::ArrowUp, mods: CTRL` | `\x1b[1;5A` |

Test coverage in `crates/kmux-ghostty/src/lib.rs::tests::encode_*` and
`crates/kmuxd/src/backend/ghostty/mod.rs::tests::encode_key_event_*`.

## kmux-internal keys

`Ctrl+G` (and other mode-trigger keybindings; see
`crates/kmux/src/mode/resolve.rs`) are intercepted *before* the encoder is
called — they never reach the daemon and never become `PtyKey` events.
Mode-internal Tab/Enter (command palette completion, connect-form field
navigation) are similarly consumed locally.

## Backwards compatibility

The protocol bumped from `PROTOCOL_VERSION = 17` to `18` when `PtyKey` /
`PtyKeyBatch` were added.  Old clients refuse to connect to new daemons
and vice versa — users update both binaries together.  See
`version_mismatch_hint` in `kmux-protocol::messages::types`.
