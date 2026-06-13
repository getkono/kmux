# kmux Terminal Multiplexer — VT Sequence & Feature Reference

This document catalogues every VT sequence and terminal feature relevant to a
modern, feature-complete multiplexer, and records kmux's current status for
each.

---

## Architecture overview

kmux uses a **server-authoritative VT rendering model**.  Only the daemon
(`kmuxd`) runs a VT emulator; clients receive pre-resolved cell data.

```
PTY stdout ──bytes──▶ GhosttyBackend (libghostty-vt)
                              │
                              ▼
                       DiffEngine<B>   ← frame-to-frame diffing
                              │
                        DiffOp stream  (Cell / Row / Clear)
                              │
                    ServerMessage::TerminalUpdate ──wire──▶ thin clients
```

The backend (`crates/kmuxd/src/backend/ghostty/`) parses raw PTY bytes through
`libghostty-vt` (accessed via the `kmux-ghostty` safe façade over a kmux-owned
C ABI in `crates/kmux-ghostty-sys/zig/src/wrapper.zig`), resolves all
named/indexed colours to RGB, and emits `CellState` structs containing
`(char, fg_rgb, bg_rgb, CellAttrs)`.  Clients never touch raw escape
sequences.  This means:

- **Colour handling is always authoritative** — the xterm-256 palette is
  resolved on the server; clients always receive 24-bit RGB.
- **Feature flags are advisory today** — `kitty_graphics` / `kitty_keyboard`
  atomics are recomputed on every client attach/detach, but libghostty-vt
  parses every supported sequence unconditionally, so no parse-time gating
  is applied.
- **The wire protocol is the limiting factor** — if a feature is parsed by
  `libghostty-vt` but has no field in `kmux-protocol::messages`, clients
  cannot act on it.

> **Multi-pane tiling.** Simultaneously-visible tiled panes (splits, focus,
> resize, swap, preset layouts, zoom) under the **Session → Tab → Pane** model
> are documented separately in [layout.md](layout.md): the shared layout tree,
> the deterministic resolver, the server-authoritative mutation/broadcast flow,
> and the keymap.

### Status legend

| Symbol | Meaning |
|--------|---------|
| **Stable** | Implemented, tested, working end-to-end |
| **Partial** | Works with documented gaps or approximations |
| **Unimplemented** | Not yet implemented; planned for a future phase |
| **Not planned** | Explicitly out of scope |

---

## Environment variables set in spawned shells

Shells inside kmux panes always receive:

| Variable | Value | Rationale |
|----------|-------|-----------|
| `TERM` | `xterm-256color` | `libghostty-vt` is an xterm-family parser; advertising a non-xterm TERM risks sequences the parser cannot handle |
| `COLORTERM` | `truecolor` | The emulator parses 24-bit SGR unconditionally; RGB is always transmitted on the wire |
| `TERM_PROGRAM` | `kmux` | Prevents the launching terminal's `$TERM_PROGRAM` from leaking into panes |
| `TERM_PROGRAM_VERSION` | `<cargo version>` | Stable identity for feature-sniffers (Starship, bat, etc.) |
| `KMUX` | `<cargo version>` | Marks the shell as kmux-managed (like tmux's `$TMUX`); the `kmux` entrypoint reads it to warn before nesting a GUI — see below (issue #73) |

These are set in `kmuxd`'s `capability::pane_spawn_env` and applied at PTY spawn.

### Nested-launch warning (issue #73)

Because every pane exports `KMUX`, running `kmux` *inside* a kmux pane is detectable. The `kmux` entrypoint (`crates/kmux/src/main.rs`) checks for `KMUX` before exec-ing the desktop GUI and, when set, warns at the **terminal** (not in the GUI — it would be invisible on a headless host) with three choices:

- **don't start** (the default, and what EOF / a non-interactive stdin pick) — opening a multiplexer inside itself is usually a mistake;
- **start anyway** — proceed this once;
- **always start from now on** — proceed and persist `warn_nested = false` to `config.toml` (`config::set_warn_when_nested`) so the prompt is never shown again.

The check lives only in the entrypoint, so the frontend it exec's never re-prompts.

---

## C0 control characters

These are single-byte sequences handled directly by the VT parser.

| Byte | Name | Seq | Status | Notes |
|------|------|-----|--------|-------|
| `0x00` | NUL | — | **Stable** | Ignored by emulator |
| `0x07` | BEL | `^G` | **Stable** | Forwarded via `BackendEventSink::on_bell()` |
| `0x08` | BS  | `^H` | **Stable** | Moves cursor left |
| `0x09` | HT  | `^I` | **Stable** | Horizontal tab stop |
| `0x0A` | LF  | `^J` | **Stable** | Line feed / newline |
| `0x0B` | VT  | `^K` | **Stable** | Treated as LF |
| `0x0C` | FF  | `^L` | **Stable** | Treated as LF |
| `0x0D` | CR  | `^M` | **Stable** | Carriage return |
| `0x0E` | SO  | `^N` | **Stable** | Shift Out — activate G1 charset |
| `0x0F` | SI  | `^O` | **Stable** | Shift In — activate G0 charset |
| `0x1B` | ESC | — | **Stable** | Introduces all escape sequences |

---

## Escape (two-character) sequences

| Sequence | Name | Status | Notes |
|----------|------|--------|-------|
| `ESC c` | RIS — Reset to Initial State | **Stable** | Full terminal reset via libghostty-vt |
| `ESC 7` | DECSC — Save cursor | **Stable** | Saves position, attributes, charset |
| `ESC 8` | DECRC — Restore cursor | **Stable** | |
| `ESC M` | RI — Reverse Index (scroll down) | **Stable** | |
| `ESC =` | DECKPAM — Keypad application mode | **Stable** | Parsed by emulator |
| `ESC >` | DECKPNM — Keypad numeric mode | **Stable** | Parsed by emulator |
| `ESC D` | IND — Index (scroll up) | **Stable** | |
| `ESC E` | NEL — Next Line | **Stable** | |
| `ESC H` | HTS — Horizontal Tab Set | **Stable** | |
| `ESC ( C` | G0 charset designation | **Stable** | e.g. line-drawing (`ESC ( 0`) |
| `ESC ) C` | G1 charset designation | **Stable** | |
| `ESC * C` | G2 charset designation | **Stable** | |
| `ESC + C` | G3 charset designation | **Stable** | |
| `ESC n` | LS2 — Locking Shift G2 | **Stable** | |
| `ESC o` | LS3 — Locking Shift G3 | **Stable** | |

---

## CSI sequences

### Cursor movement

| Sequence | Name | Status | Notes |
|----------|------|--------|-------|
| `CSI A` | CUU — Cursor Up | **Stable** | |
| `CSI B` | CUD — Cursor Down | **Stable** | |
| `CSI C` | CUF — Cursor Forward | **Stable** | |
| `CSI D` | CUB — Cursor Backward | **Stable** | |
| `CSI E` | CNL — Cursor Next Line | **Stable** | |
| `CSI F` | CPL — Cursor Preceding Line | **Stable** | |
| `CSI G` | CHA — Cursor Horizontal Absolute | **Stable** | |
| `CSI H` | CUP — Cursor Position | **Stable** | Tested via `\x1b[3;1H` |
| `CSI f` | HVP — Horizontal/Vertical Position | **Stable** | Synonym for CUP |
| `CSI I` | CHT — Cursor Horizontal Tab | **Stable** | |
| `CSI Z` | CBT — Cursor Backward Tab | **Stable** | |
| `CSI d` | VPA — Vertical Line Position Absolute | **Stable** | |
| `CSI \`` | HPA — Horizontal Position Absolute | **Stable** | |

### Erase

| Sequence | Name | Status | Notes |
|----------|------|--------|-------|
| `CSI J` | ED — Erase in Display (0/1/2/3) | **Stable** | Full-screen clear (>50 % changed to default) optimised to `DiffOp::Clear` |
| `CSI K` | EL — Erase in Line (0/1/2) | **Stable** | |
| `CSI X` | ECH — Erase Character | **Stable** | |
| `CSI P` | DCH — Delete Character | **Stable** | |
| `CSI @` | ICH — Insert Character | **Stable** | |

### Scrolling and line editing

| Sequence | Name | Status | Notes |
|----------|------|--------|-------|
| `CSI S` | SU — Scroll Up | **Stable** | |
| `CSI T` | SD — Scroll Down | **Stable** | |
| `CSI L` | IL — Insert Line | **Stable** | |
| `CSI M` | DL — Delete Line | **Stable** | |
| `CSI r` | DECSTBM — Set Top/Bottom Margins | **Stable** | |
| `CSI s` | SCOSC — Save Cursor (SCO variant) | **Stable** | |
| `CSI u` | SCORC / Kitty query | **Partial** | Restore cursor (SCO) parsed; kitty query path only active when `kitty_keyboard` is enabled (currently declared false by the GUI clients) |

### SGR — Select Graphic Rendition

All SGR attributes are parsed by `libghostty-vt`, resolved to the
`CellAttrs` bitfield, and forwarded over the wire.  The table below captures
what the wire protocol (`kmux-protocol::messages::CellAttrs`) can represent.

#### Intensity and style

| Sequence | Attribute | Wire bit | Status | Notes |
|----------|-----------|----------|--------|-------|
| `SGR 0` | Reset all | — | **Stable** | Clears all attribute bits; resets colour to default |
| `SGR 1` | Bold | `BOLD` | **Stable** | |
| `SGR 2` | Dim / Faint | `DIM` | **Stable** | |
| `SGR 3` | Italic | `ITALIC` | **Stable** | |
| `SGR 4` | Underline (single) | `UNDERLINE` | **Stable** | |
| `SGR 4:2` | Double underline | `UNDERLINE` | **Partial** | Parsed; wire collapses to single `UNDERLINE` bit — style is lost |
| `SGR 4:3` | Curly underline | `UNDERLINE` | **Partial** | Parsed; wire collapses |
| `SGR 4:4` | Dotted underline | `UNDERLINE` | **Partial** | Parsed; wire collapses |
| `SGR 4:5` | Dashed underline | `UNDERLINE` | **Partial** | Parsed; wire collapses |
| `SGR 5` | Slow blink | `BLINK` | **Stable** | Rendering of actual blink cadence is client-defined |
| `SGR 6` | Rapid blink | `BLINK` | **Partial** | Parsed; mapped to same `BLINK` flag — cadence distinction lost |
| `SGR 7` | Reverse video | `INVERSE` | **Stable** | Server pre-swaps fg/bg before sending; `DEFAULT_*` flags account for the swap |
| `SGR 8` | Conceal / Invisible | `HIDDEN` | **Stable** | |
| `SGR 9` | Strikethrough | `STRIKETHROUGH` | **Stable** | |
| `SGR 10–19` | Font selection | — | **Not planned** | No font concept in cell-grid model |
| `SGR 21` | Double underline (or Bold off, terminal-dependent) | `UNDERLINE` | **Partial** | Parsed; interpretation varies by terminal emulator |
| `SGR 22` | Normal intensity (clear bold/dim) | — | **Stable** | |
| `SGR 23–29` | Individual attribute resets | — | **Stable** | |
| `SGR 53` | Overline | — | **Unimplemented** | Parsed by libghostty-vt; no wire bit; clients cannot render it |
| `SGR 58;2;…` | Underline colour (RGB) | — | **Unimplemented** | Parsed by libghostty-vt; wire has no underline-colour field |
| `SGR 58;5;n` | Underline colour (indexed) | — | **Unimplemented** | Same as above |
| `SGR 73/74/75` | Superscript / subscript / reset | — | **Not planned** | |

#### Colour

All named/indexed colours are resolved to 24-bit RGB on the server using the
default xterm-256 palette.  The wire protocol carries a `(u8, u8, u8)` RGB
triple for every cell's foreground and background.

| Sequence | Colour | Status | Notes |
|----------|--------|--------|-------|
| `SGR 30–37` | 8 standard foreground colours | **Stable** | Resolved to RGB |
| `SGR 38;5;n` | 256-colour foreground | **Stable** | Resolved to RGB |
| `SGR 38;2;r;g;b` | Truecolor foreground | **Stable** | Pass-through; tested in `feed_truecolor_text` |
| `SGR 39` | Default foreground | **Stable** | `CellAttrs::DEFAULT_FG` flag; client substitutes theme colour |
| `SGR 40–47` | 8 standard background colours | **Stable** | Resolved to RGB |
| `SGR 48;5;n` | 256-colour background | **Stable** | Resolved to RGB |
| `SGR 48;2;r;g;b` | Truecolor background | **Stable** | Pass-through |
| `SGR 49` | Default background | **Stable** | `CellAttrs::DEFAULT_BG` flag |
| `SGR 90–97` | Bright / high-intensity foreground | **Stable** | Resolved to RGB |
| `SGR 100–107` | Bright / high-intensity background | **Stable** | Resolved to RGB |

### Cursor shape — DECSCUSR (`CSI Ps SP q`)

libghostty-vt tracks all six variants.  The wire protocol (`CursorShape`) has
five variants: `Block`, `Underline`, `Bar`, `HollowBlock`, `Hidden`, plus a
separate `CursorState.blink` bool. The blink request is carried (DEC mode 12,
below), so the shape and the blinking-vs-steady distinction both reach clients.
DECSCUSR `0`/no-param (the default) is treated as a blinking block per xterm, so
a program that issues no DECSCUSR still gets a blinking cursor (see
[terminal-backend.md](terminal-backend.md) → Blink).

| Sequence | Shape | Wire variant | Blink | Status | Notes |
|----------|-------|-------------|-------|--------|-------|
| `CSI 0 SP q` | Default | `Block` | `true` | **Stable** | xterm default = blinking block |
| `CSI 1 SP q` | Blinking block | `Block` | `true` | **Stable** | |
| `CSI 2 SP q` | Steady block | `Block` | `false` | **Stable** | |
| `CSI 3 SP q` | Blinking underline | `Underline` | `true` | **Stable** | |
| `CSI 4 SP q` | Steady underline | `Underline` | `false` | **Stable** | |
| `CSI 5 SP q` | Blinking bar | `Bar` | `true` | **Stable** | |
| `CSI 6 SP q` | Steady bar | `Bar` | `false` | **Stable** | |

`HollowBlock` is carried in the wire protocol for completeness but is not
emitted by the current backend — it may appear in future snapshots from
alternative backends.

### DEC private modes (`CSI ? Ps h/l`)

| Mode | Name | Status | Notes |
|------|------|--------|-------|
| `?1` | DECCKM — Application cursor keys | **Stable** | `TermModes::APP_CURSOR`; client reads this flag and encodes arrows as `SS3 O[ABCD]` vs. `CSI [ABCD]` |
| `?3` | DECCOLM — 132-column mode | **Not planned** | |
| `?5` | DECSCNM — Reverse video (screen) | **Partial** | Parsed by emulator; no dedicated wire flag; effect is absorbed into per-cell INVERSE via the diff |
| `?6` | DECOM — Origin mode | **Stable** | Handled by libghostty-vt |
| `?7` | DECAWM — Auto-wrap mode | **Stable** | Handled by libghostty-vt |
| `?12` | AT&T 610 cursor blink | **Stable** | Forwarded as `CursorState::blink`; clients blink the cursor (gated by the `cursor_blink` setting) |
| `?25` | DECTCEM — Cursor visibility | **Stable** | `CursorState::visible`; tested in `fzf_cursor_hidden_state` |
| `?47` | Alternate screen buffer (old) | **Stable** | Handled by libghostty-vt |
| `?1000` | X10 / normal mouse tracking | **Stable** | `MOUSE_REPORT_CLICK` set whenever any of 1000/1002/1003 is active |
| `?1002` | Button-event mouse tracking | **Stable** | `MOUSE_DRAG` set; tested in `mouse_drag_mode_enable_disable` |
| `?1003` | Any-event mouse tracking | **Stable** | `MOUSE_MOTION` set; tested in `mouse_motion_mode_enable_disable` |
| `?1004` | Focus-in/focus-out reporting | **Unimplemented** | Events not forwarded to PTY |
| `?1005` | UTF-8 mouse encoding | **Not planned** | Superseded by SGR mode |
| `?1006` | SGR extended mouse coordinates | **Stable** | `TermModes::SGR_MOUSE`; client reads this to choose between SGR and legacy X10 encoding for scroll events |
| `?1015` | URXVT mouse encoding | **Not planned** | |
| `?1016` | SGR-pixel mouse coordinates | **Not planned** | |
| `?1049` | Alternate screen (save/restore cursor + clear) | **Stable** | `is_alt_screen()` tracked; scrollback suppressed while on alt screen; tested in `alt_screen_no_scrollback_duplication` |
| `?2004` | Bracketed paste mode | **Stable** | `TermModes::BRACKETED_PASTE`; server wraps `PtyPaste` messages with `ESC[200~`…`ESC[201~`; tested in `bracketed_paste_mode_enable_disable` |
| `?2026` | Synchronized output (BST) | **Unimplemented** | Requires diff batching across the DCS/ST pair |
| `?2048` | In-band resize notifications | **Unimplemented** | |
| `?9001` | Win32 input mode | **Not planned** | |

---

## OSC sequences

| Sequence | Name | Status | Notes |
|----------|------|--------|-------|
| `OSC 0 … BEL/ST` | Set window title and icon title | **Stable** | Forwarded via `BackendEventSink::on_title()`; tested in `event_sink_receives_title` |
| `OSC 1 … BEL/ST` | Set icon title | **Stable** | Mapped to `on_title()` (same as OSC 0) |
| `OSC 2 … BEL/ST` | Set window title | **Stable** | |
| `OSC 4 ; c ; spec BEL/ST` | Set/query colour palette entry | **Partial** | Parsed by libghostty-vt; colour changes affect resolved RGB but palette queries are not replied to clients |
| `OSC 7 … BEL/ST` | Set current working directory | **Unimplemented** | URI not extracted or forwarded |
| `OSC 8 ; … ; uri BEL/ST` | Hyperlink | **Unimplemented** | `BackendEventSink::on_hyperlink()` seam exists; no forwarding yet |
| `OSC 10 / 11 BEL/ST` | Query default fg/bg colour | **Partial** | Parsed; query responses not implemented (no back-channel to the application from the emulator) |
| `OSC 52 ; … BEL/ST` | Clipboard write (set) | **Stable** | `on_osc52_copy()` broadcasts `PaneClipboardCopy` server-wide; the client writes it to the system clipboard, honoring writes from any pane in the session it is viewing (last-in-wins). Clipboard *read* (`OSC 52 ; … ; ?`) is not answered (no client→server clipboard channel) |
| `OSC 133 / 633` | Shell integration / semantic zones | **Not planned** | |
| `OSC 1337` | iTerm2 inline images | **Unimplemented** | Parsed by libghostty-vt; image data dropped silently (Phase A) |
| `OSC 9` | iTerm2 / Windows Terminal growl notification | **Not planned** | |

---

## DCS sequences

| Sequence | Name | Status | Notes |
|----------|------|--------|-------|
| `DCS Ps;Ps;Ps q … ST` | Sixel graphics | **Unimplemented** | libghostty-vt parses sixel payloads; Phase A drops them at the `GhosttyBackend` boundary.  Phase B will extract and forward via an extended `CellState`. |
| `DCS $ q Pt ST` | DECRQSS — Request Selection or Setting | **Stable** | Handled internally by libghostty-vt |
| `DCS + q Pt ST` | XTGETTCAP — Query terminfo capability | **Partial** | Parsed by libghostty-vt; responses go back to the PTY but are not relayed to the wire client |
| `DCS + p Pt ST` | XTSETTCAP | **Partial** | Same as XTGETTCAP |

---

## APC sequences

| Sequence | Name | Status | Notes |
|----------|------|--------|-------|
| `APC G … ST` | Kitty graphics protocol | **Unimplemented** | libghostty-vt parses APC G payloads unconditionally; the `kitty_graphics` capability atomic is recomputed on every attach/detach but not consulted at parse time today.  Phase A drops image data at the `GhosttyBackend` boundary.  The GUI clients currently declare `kitty_graphics: false`. |

---

## Mouse protocol

### Application-side mode detection

The server reports mouse mode state in `TermModes` sent with each diff.

| Mode flag | Meaning | Status |
|-----------|---------|--------|
| `MOUSE_REPORT_CLICK` | Any mouse tracking mode active (1000/1002/1003 union) | **Stable** |
| `MOUSE_DRAG` | Button-event mode (DEC 1002) | **Stable** |
| `MOUSE_MOTION` | Any-event mode (DEC 1003) | **Stable** |
| `SGR_MOUSE` | SGR extended coordinates (DEC 1006) | **Stable** |

### Input encoding (client → PTY)

| Event | Mode | Encoding | Status |
|-------|------|----------|--------|
| Scroll up/down | Legacy X10 | `ESC [ M {cb} {cx} {cy}` | **Stable** |
| Scroll up/down | SGR | `ESC [ < {b} ; {x} ; {y} M` | **Stable** |
| Button press/release | Any | — | **Unimplemented** — only scroll events are forwarded to the PTY; click and drag events are not |
| Focus in/out | `?1004` | `ESC [ I` / `ESC [ O` | **Unimplemented** |

The client uses `modes().mouse_report()` to decide whether scroll events go to
the PTY or to local scrollback, and `modes().sgr_mouse()` to choose the
encoding (`crates/kmux/src/app/mouse_handler.rs`).

---

## Keyboard input

### Input encoding (client → PTY)

| Key / category | Sequence | Status | Notes |
|---------------|---------|--------|-------|
| Printable characters | UTF-8 bytes | **Stable** | |
| `Ctrl+A` – `Ctrl+Z` | `0x01` – `0x1A` | **Stable** | Ctrl+letter mapped to control byte |
| `Enter` | `0x0D` (CR) | **Stable** | |
| `Backspace` | `0x7F` (DEL) | **Stable** | |
| `Tab` | `0x09` | **Stable** | |
| `Escape` | `0x1B` | **Stable** | |
| `Delete` | `ESC [ 3 ~` | **Stable** | |
| `Insert` | `ESC [ 2 ~` | **Stable** | |
| `Page Up` | `ESC [ 5 ~` | **Stable** | |
| `Page Down` | `ESC [ 6 ~` | **Stable** | |
| `Home` (normal cursor) | `ESC [ H` | **Stable** | |
| `Home` (application cursor) | `ESC O H` | **Stable** | |
| `End` (normal cursor) | `ESC [ F` | **Stable** | |
| `End` (application cursor) | `ESC O F` | **Stable** | |
| `↑ ↓ ← →` (normal cursor mode) | `ESC [ A–D` | **Stable** | |
| `↑ ↓ ← →` (application cursor mode) | `ESC O A–D` | **Stable** | Controlled by `TermModes::APP_CURSOR` |
| `F1–F4` | `ESC O P–S` | **Stable** | |
| `F5–F12` | `ESC [ 15–24 ~` | **Stable** | `F6=17`, `F7=18`, `F8=19`, `F9=20`, `F10=21`, `F11=23`, `F12=24` |
| `F13–F24` | — | **Not planned** | |
| `Alt+key` | `ESC {key-bytes}` | **Stable** | Encoded server-side via Ghostty's `key_encode`. |
| `Shift+modifier combos` | `CSI 1 ; Ps [ABCD]` etc. | **Stable** | Encoded server-side via Ghostty's `key_encode`. |
| Kitty keyboard protocol | `CSI > N u` push, `CSI < N u` pop, `CSI N ; M u` modifier reports | **Stable** | Inner program enables via DECSET; daemon's `kmux_ghostty_kitty_flags` getter feeds the flags into `KeyEncodeOptions` per encode call. |
| xterm `modifyOtherKeys` | `CSI 27 ; mod ; code ~` | **Stable** | Default fallback when kitty kbd is not negotiated; Ghostty's `key_encode.legacy` emits this for modified Enter/Backspace/etc. |

---

## Unicode

| Feature | Status | Notes |
|---------|--------|-------|
| UTF-8 input | **Stable** | Raw UTF-8 bytes forwarded to PTY |
| UTF-8 output | **Stable** | libghostty-vt decodes UTF-8; the `char` field in `CellState` is a Rust `char` |
| Wide characters (CJK, emoji) | **Stable** | `CellAttrs::WIDE_CHAR` set on the primary cell; `CellAttrs::WIDE_CHAR_SPACER` set on the following placeholder cell; tested with `'中'` |
| Combining characters | **Partial** | libghostty-vt handles most combining sequences; the wire protocol stores one `char` per cell — combining codepoints are merged by the emulator before serialisation |
| Bidirectional text (BiDi) | **Not planned** | Cell-grid model is left-to-right only |

---

## Scrollback

| Feature | Status | Notes |
|---------|--------|-------|
| Scrollback ring buffer | **Stable** | Server-side: 50 000 lines default (`DEFAULT_SCROLLBACK`), configurable in `BackendConfig` |
| Scrollback during alt screen | **Stable** | Suppressed while alt screen is active; tracked with `saved_main_history_size` so exiting alt screen does not re-emit existing history |
| Client-side scrollback cache | **Stable** | `crates/kmux-client/src/grid/scrollback.rs`; 50 000 line ring buffer (VecDeque); scroll offset tracked for UI navigation |
| Incremental sync on reconnect | **Stable** | Per-pane monotonic `SequenceNo`; client sends last seen sequence on `Attach`; server replays only missing lines |
| Session restore preamble | **Stable** | On session restore, existing scrollback + viewport are serialised back to ANSI (`snapshot_to_ansi`) and re-fed into a fresh emulator so attaching clients receive the full prior history |
| Copy from scrollback | **Stable** | Cell selection in the client grid (`grid/selection.rs`); `cli_clipboard` used for write |

---

## Window title and bell

| Feature | Status | Notes |
|---------|--------|-------|
| Window title (`OSC 0/1/2`) | **Stable** | `BackendEventSink::on_title()` called synchronously inside `advance_bytes`; forwarded via `ServerMessage` (event-bus channel, non-blocking) |
| Icon title (same sequence, `OSC 1`) | **Stable** | Merged into `on_title()` |
| BEL (`0x07`) | **Stable** | `BackendEventSink::on_bell()` called; clients can produce audible or visual bell |

---

## Resize

| Feature | Status | Notes |
|---------|--------|-------|
| Terminal resize | **Stable** | `ClientMessage::Resize` carries `TermSize { rows, cols, pixel_width, pixel_height }`; server picks smallest-wins dimensions across all attached clients; `TIOCSWINSZ` issued outside the session lock |
| Pixel dimensions | **Stable** | `pixel_width` / `pixel_height` forwarded (for future image-protocol scaling); `0` = unknown |
| Resize debounce | **Stable** | 100 ms debounce in the client event loop (`RESIZE_DEBOUNCE`) to avoid flooding the server during window drag |
| `SIGWINCH` forwarding | **Stable** | the toolkit raises resize events; the client issues `ClientMessage::Resize` which triggers a server-side `TIOCSWINSZ` to the PTY child |

---

## Graphics protocols

| Protocol | Status | Notes |
|----------|--------|-------|
| Sixel (DCS) | **Unimplemented** | Phase A: dropped silently at the `GhosttyBackend` boundary.  Phase B: extract from libghostty-vt and forward via extended wire protocol |
| Kitty graphics (APC) | **Unimplemented** | Phase A: same; capability atomic is wired but the GUI clients declare `false`; see APC section above |
| iTerm2 inline images (OSC 1337) | **Unimplemented** | Parsed; dropped silently |

---

## Capability negotiation

Client capabilities are declared at auth time in `ClientCapabilities`:

| Field | Current GUI client value | Effect |
|-------|--------------------------|--------|
| `truecolor` | Detected from `$COLORTERM` / `$TERM` | Reserved for future per-client colour downgrade (today server always sends RGB) |
| `kitty_graphics` | `false` (hardcoded) | Drives the `kitty_graphics` atomic in `CapabilityHandles` (reserved for future parse-time gating) |
| `kitty_keyboard` | Set by the frontend when its toolkit reports keyboard-enhancement support (the GUI clients declare `false`) | Reported to the daemon for future parse-time gating; the *encoding* path uses the live `kitty_keyboard.current()` flags read from the pane's Ghostty terminal each `encode_key_event` call (see `docs/keyboard.md`) |
| `term` | `$TERM` (informational) | Logged; not used for `TERM` selection |
| `term_program` | `$TERM_PROGRAM` (informational) | Logged; not used |

When multiple clients attach to the same pane, the backend uses the
intersection (AND) of their capability flags so that the pane always emits
sequences every attached client can handle (`capability::intersect_for_atomics`).

---

## Known limitations and planned work

1. **Mouse click/drag forwarding** — only scroll events are currently forwarded
   to the PTY.  Click and drag events consumed by the client (e.g. chrome clicks)
   are not passed through even when the application requests any-event tracking.

2. **Image protocols (Phase B)** — libghostty-vt parses kitty (APC G), sixel
   (DCS), and iTerm2 (OSC 1337) image payloads; Phase A discards them at the
   wrapper boundary.  Forwarding to clients requires extending
   `kmux-protocol::messages::CellState` and the diff serialisation format.

3. **Underline variants and colour** — the wire protocol has a single
   `UNDERLINE` bit.  Adding an `underline_style: UnderlineStyle` field and an
   `underline_color: CellColor` field would expose the full SGR 4:n / SGR 58
   repertoire.

4. **Overline** — `SGR 53` is parsed; a corresponding `OVERLINE` bit in
   `CellAttrs` would complete the standard decoration set.

5. **OSC 52 clipboard read** — clipboard *writes* (set) are forwarded and
   applied client-side (see the OSC table above); answering a clipboard *read*
   (`OSC 52 ; … ; ?`) still requires a client→server clipboard channel.

6. **OSC 7 (current directory)** — forwarding this would let the client update
   its session CWD display without a separate RPC.

7. **Synchronized output (`?2026`)** — would allow flicker-free redraws; needs
   the diff engine to defer emission until the closing ST.
