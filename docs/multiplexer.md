# kmux Terminal Multiplexer — VT Sequence & Feature Reference

This document catalogues every VT sequence and terminal feature relevant to a
modern, feature-complete multiplexer, and records kmux's current status for
each.

---

## Architecture overview

kmux uses a **server-authoritative VT rendering model**.  Only the daemon
(`kmuxd`) runs a VT emulator; clients receive pre-resolved cell data.

```
PTY stdout ──bytes──▶ WezTermBackend (tattoy-wezterm-term)
                              │
                              ▼
                       DiffEngine<B>   ← frame-to-frame diffing
                              │
                        DiffOp stream  (Cell / Row / Clear)
                              │
                    ServerMessage::TerminalUpdate ──wire──▶ thin clients
```

The backend (`crates/kmuxd/src/backend/wezterm/`) parses raw PTY bytes through
`tattoy-wezterm-term`, resolves all named/indexed colours to RGB, and emits
`CellState` structs containing `(char, fg_rgb, bg_rgb, CellAttrs)`.  Clients
never touch raw escape sequences.  This means:

- **Colour handling is always authoritative** — the xterm-256 palette is
  resolved on the server; clients always receive 24-bit RGB.
- **Feature flags gate what the PTY sees** — e.g. `kitty_keyboard` toggles
  whether the emulator advertises the kitty keyboard protocol to the shell.
- **The wire protocol is the limiting factor** — if a feature is parsed by
  `tattoy-wezterm-term` but has no field in `kmux-protocol::messages`, clients
  cannot act on it.

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
| `TERM` | `xterm-256color` | `tattoy-wezterm-term` is an xterm-family parser; advertising a non-xterm TERM risks sequences the parser cannot handle |
| `COLORTERM` | `truecolor` | The emulator parses 24-bit SGR unconditionally; RGB is always transmitted on the wire |
| `TERM_PROGRAM` | `kmux` | Prevents the launching terminal's `$TERM_PROGRAM` from leaking into panes |
| `TERM_PROGRAM_VERSION` | `<cargo version>` | Stable identity for feature-sniffers (Starship, bat, etc.) |

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
| `ESC c` | RIS — Reset to Initial State | **Stable** | Full terminal reset via wezterm-term |
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
| `CSI u` | SCORC / Kitty query | **Partial** | Restore cursor (SCO) parsed; kitty query path only active when `kitty_keyboard` is enabled (currently hardcoded false in the TUI client) |

### SGR — Select Graphic Rendition

All SGR attributes are parsed by `tattoy-wezterm-term`, resolved to the
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
| `SGR 53` | Overline | — | **Unimplemented** | Parsed by wezterm-term; no wire bit; clients cannot render it |
| `SGR 58;2;…` | Underline colour (RGB) | — | **Unimplemented** | Parsed by wezterm-term; wire has no underline-colour field |
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

wezterm-term tracks all six variants.  The wire protocol (`CursorShape`) has
five variants: `Block`, `Underline`, `Bar`, `HollowBlock`, `Hidden`.  The
blinking vs. steady distinction is **collapsed** — clients see the shape but
not the requested blink state.

| Sequence | Shape | Wire variant | Status | Notes |
|----------|-------|-------------|--------|-------|
| `CSI 0 SP q` | Default (implementation-defined) | `Block` | **Stable** | |
| `CSI 1 SP q` | Blinking block | `Block` | **Partial** | Blink state dropped |
| `CSI 2 SP q` | Steady block | `Block` | **Stable** | |
| `CSI 3 SP q` | Blinking underline | `Underline` | **Partial** | Blink state dropped |
| `CSI 4 SP q` | Steady underline | `Underline` | **Stable** | |
| `CSI 5 SP q` | Blinking bar | `Bar` | **Partial** | Blink state dropped |
| `CSI 6 SP q` | Steady bar | `Bar` | **Stable** | |

`HollowBlock` is carried in the wire protocol for completeness but is not
emitted by the current backend — it may appear in future snapshots from
alternative backends.

### DEC private modes (`CSI ? Ps h/l`)

| Mode | Name | Status | Notes |
|------|------|--------|-------|
| `?1` | DECCKM — Application cursor keys | **Stable** | `TermModes::APP_CURSOR`; client reads this flag and encodes arrows as `SS3 O[ABCD]` vs. `CSI [ABCD]` |
| `?3` | DECCOLM — 132-column mode | **Not planned** | |
| `?5` | DECSCNM — Reverse video (screen) | **Partial** | Parsed by emulator; no dedicated wire flag; effect is absorbed into per-cell INVERSE via the diff |
| `?6` | DECOM — Origin mode | **Stable** | Handled by wezterm-term |
| `?7` | DECAWM — Auto-wrap mode | **Stable** | Handled by wezterm-term |
| `?12` | AT&T 610 cursor blink | **Partial** | Parsed; blink state not forwarded on wire |
| `?25` | DECTCEM — Cursor visibility | **Stable** | `CursorState::visible`; tested in `fzf_cursor_hidden_state` |
| `?47` | Alternate screen buffer (old) | **Stable** | Handled by wezterm-term |
| `?1000` | X10 / normal mouse tracking | **Partial** | Detected via `is_mouse_grabbed()` which returns true for any of 1000/1002/1003; exposed as `MOUSE_REPORT_CLICK` proxy.  Individual mode bits are indistinguishable pending an upstream fix. |
| `?1002` | Button-event mouse tracking | **Partial** | See `?1000`; `MOUSE_DRAG` bit defined in wire but never set by current backend |
| `?1003` | Any-event mouse tracking | **Partial** | See `?1000`; `MOUSE_MOTION` bit defined but never set |
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
| `OSC 4 ; c ; spec BEL/ST` | Set/query colour palette entry | **Partial** | Parsed by wezterm-term; colour changes affect resolved RGB but palette queries are not replied to clients |
| `OSC 7 … BEL/ST` | Set current working directory | **Unimplemented** | URI not extracted or forwarded |
| `OSC 8 ; … ; uri BEL/ST` | Hyperlink | **Unimplemented** | `BackendEventSink::on_hyperlink()` seam exists; no forwarding yet |
| `OSC 10 / 11 BEL/ST` | Query default fg/bg colour | **Partial** | Parsed; query responses not implemented (no back-channel to the application from the emulator) |
| `OSC 52 ; … BEL/ST` | Clipboard read/write | **Unimplemented** | `BackendEventSink::on_osc52_copy()` seam exists; no forwarding yet |
| `OSC 133 / 633` | Shell integration / semantic zones | **Not planned** | |
| `OSC 1337` | iTerm2 inline images | **Unimplemented** | Parsed by wezterm-term; image data dropped silently (Phase A) |
| `OSC 9` | iTerm2 / Windows Terminal growl notification | **Not planned** | |

---

## DCS sequences

| Sequence | Name | Status | Notes |
|----------|------|--------|-------|
| `DCS Ps;Ps;Ps q … ST` | Sixel graphics | **Unimplemented** | wezterm-term delivers image data via `attrs.images()`; Phase A drops it silently.  Phase B will extract and forward via an extended `CellState`. |
| `DCS $ q Pt ST` | DECRQSS — Request Selection or Setting | **Stable** | Handled internally by wezterm-term |
| `DCS + q Pt ST` | XTGETTCAP — Query terminfo capability | **Partial** | Parsed by wezterm-term; responses go back to the PTY but are not relayed to the wire client |
| `DCS + p Pt ST` | XTSETTCAP | **Partial** | Same as XTGETTCAP |

---

## APC sequences

| Sequence | Name | Status | Notes |
|----------|------|--------|-------|
| `APC G … ST` | Kitty graphics protocol | **Unimplemented** | `enable_kitty_graphics()` is wired up via `Arc<AtomicBool>` and the `TerminalConfiguration` trait; image payloads reach wezterm-term only when at least one attached client declares `kitty_graphics: true`.  The TUI client (`crates/kmux`) currently hardcodes `kitty_graphics: false`.  When enabled by a future client, Phase A still drops image data silently. |

---

## Mouse protocol

### Application-side mode detection

The server reports mouse mode state in `TermModes` sent with each diff.

| Mode flag | Meaning | Status |
|-----------|---------|--------|
| `MOUSE_REPORT_CLICK` | Any mouse tracking mode active (1000/1002/1003 union) | **Partial** — proxy because `is_mouse_grabbed()` cannot distinguish individual modes |
| `MOUSE_DRAG` | Button-event mode (DEC 1002) | **Partial** — bit defined; never set by current backend |
| `MOUSE_MOTION` | Any-event mode (DEC 1003) | **Partial** — bit defined; never set by current backend |
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
| `Alt+key` | `ESC {key-bytes}` | **Partial** | The `ALT` modifier is captured from crossterm; the ESC prefix is **not** explicitly added in `key_to_bytes` for character keys.  Behaviour depends on whether the host terminal delivers Alt+char as two events (ESC + char) or as a single event with the modifier flag set.  Works reliably when the host terminal pre-encodes the ESC prefix; may silently drop the modifier otherwise. |
| `Shift+modifier combos` | `CSI 1 ; Ps [ABCD]` etc. | **Unimplemented** | Modifier-encoded sequences (xterm `modifyOtherKeys` style) not generated |
| Kitty keyboard protocol | `CSI = Ps u` | **Unimplemented** | `enable_kitty_keyboard()` is wired via `Arc<AtomicBool>`; the TUI client hardcodes `kitty_keyboard: false`, so the protocol is never advertised to shells today |
| xterm `modifyOtherKeys` | `CSI 27 ; mod ; code ~` | **Not planned** | |

---

## Unicode

| Feature | Status | Notes |
|---------|--------|-------|
| UTF-8 input | **Stable** | Raw UTF-8 bytes forwarded to PTY |
| UTF-8 output | **Stable** | wezterm-term decodes UTF-8; the `char` field in `CellState` is a Rust `char` |
| Wide characters (CJK, emoji) | **Stable** | `CellAttrs::WIDE_CHAR` set on the primary cell; `CellAttrs::WIDE_CHAR_SPACER` set on the following placeholder cell; tested with `'中'` |
| Combining characters | **Partial** | wezterm-term handles most combining sequences; the wire protocol stores one `char` per cell — combining codepoints are merged by the emulator before serialisation |
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
| `SIGWINCH` forwarding | **Stable** | crossterm raises resize events; client issues `ClientMessage::Resize` which triggers a server-side `TIOCSWINSZ` to the PTY child |

---

## Graphics protocols

| Protocol | Status | Notes |
|----------|--------|-------|
| Sixel (DCS) | **Unimplemented** | Phase A: dropped silently.  Phase B: extract from `attrs.images()` and forward via extended wire protocol |
| Kitty graphics (APC) | **Unimplemented** | Phase A: same; `enable_kitty_graphics()` wired but TUI client hardcodes `false`; see APC section above |
| iTerm2 inline images (OSC 1337) | **Unimplemented** | Parsed; dropped silently |

---

## Capability negotiation

Client capabilities are declared at auth time in `ClientCapabilities`:

| Field | Current TUI client value | Effect |
|-------|--------------------------|--------|
| `truecolor` | Detected from `$COLORTERM` / `$TERM` | Reserved for future per-client colour downgrade (today server always sends RGB) |
| `kitty_graphics` | `false` (hardcoded) | Controls `enable_kitty_graphics()` on the backend |
| `kitty_keyboard` | `false` (hardcoded) | Controls `enable_kitty_keyboard()` on the backend |
| `term` | `$TERM` (informational) | Logged; not used for `TERM` selection |
| `term_program` | `$TERM_PROGRAM` (informational) | Logged; not used |

When multiple clients attach to the same pane, the backend uses the
intersection (AND) of their capability flags so that the pane always emits
sequences every attached client can handle (`capability::intersect_for_atomics`).

---

## Known limitations and planned work

1. **Individual mouse mode bits** (`crates/kmuxd/src/backend/wezterm/mod.rs` line 101)
   `is_mouse_grabbed()` returns `true` for any active mouse tracking mode
   (1000, 1002, or 1003) but cannot distinguish between them.  `MOUSE_DRAG`
   and `MOUSE_MOTION` bits in `TermModes` are defined but never set.  A
   contribution to `tattoy-wezterm-term` to expose individual mode flags is
   needed.

2. **Mouse click/drag forwarding** — only scroll events are currently forwarded
   to the PTY.  Click and drag events consumed by the TUI (e.g. badge clicks)
   are not passed through even when the application requests any-event tracking.

3. **Image protocols (Phase B)** — `attrs.images()` in wezterm-term carries
   kitty, sixel, and iTerm2 pixel data.  Forwarding this to clients requires
   extending `kmux-protocol::messages::CellState` and the diff serialisation
   format.

4. **Cursor blink state** — wezterm-term tracks `BlinkingBlock`, `BlinkingBar`,
   `BlinkingUnderline` separately from their steady counterparts, but the wire
   `CursorShape` enum collapses these.  A dedicated `blink: bool` field would
   allow clients to render the requested cadence.

5. **Underline variants and colour** — the wire protocol has a single
   `UNDERLINE` bit.  Adding an `underline_style: UnderlineStyle` field and an
   `underline_color: CellColor` field would expose the full SGR 4:n / SGR 58
   repertoire.

6. **Overline** — `SGR 53` is parsed; a corresponding `OVERLINE` bit in
   `CellAttrs` would complete the standard decoration set.

7. **Alt+key encoding** — `key_to_bytes` does not explicitly emit the ESC
   prefix for Alt+character combinations; see the Keyboard table for details.

8. **Kitty keyboard protocol (input)** — the server-side emulator is gated
   behind `enable_kitty_keyboard()` which is properly wired; the TUI client
   needs to opt in and a matching encoder in `key_to_bytes` is needed.

9. **OSC 52 clipboard** — the `on_osc52_copy` seam in `BackendEventSink`
   exists; full implementation requires a client-to-server clipboard channel.

10. **OSC 7 (current directory)** — forwarding this would let the client update
    its session CWD display without a separate RPC.

11. **Synchronized output (`?2026`)** — would allow flicker-free redraws; needs
    the diff engine to defer emission until the closing ST.
