# Client frontend architecture

This document describes how the kmux **client** is layered so that the same core
can drive multiple frontends — a native GTK4 GUI (`kmux-gtk`, Linux + macOS) and
a native SwiftUI app (`kmux-swift`, macOS, via the `kmux-ffi` uniffi boundary) —
all fronted by one toolkit-agnostic entrypoint binary, `kmux`. Everything that is
*not* toolkit-specific lives in a shared core (`kmux-app`), so a new frontend is
a thin presentation + input layer over it.

## Layering

```
kmux            ENTRYPOINT binary (toolkit-agnostic): shares run_cli; runs the
      ╎         subcommands, else execs the platform desktop app — kmux-gtk on
      ╎ (execs) Linux, the Swift kmux.app on macOS. Depends only on kmux-app.
kmux-protocol   wire protocol, transport traits, dirs/paths, auth
      │
kmux-client     MECHANISM: SessionManager, terminal grid model (CellGrid),
      │         transports/bootstrap/SSH/supervisor, key model (key::Key)
      ▼
kmux-app        INTERACTION POLICY (toolkit-agnostic): Mode/Action + resolve,
      │         the /-command palette, AppCore view-model + dispatch +
      │         connection orchestration, theme palette (Rgb) + config,
      │         recent-servers, the shared CLI front door (run_cli)
      │         + the FrontendDriver (the shared run loop)
      ├─────────────────────┬─────────────────
      ▼                     ▼
kmux-gtk              kmux-ffi              FRONTENDS
(GTK4 + glib +        (uniffi C ABI →       (presentation only)
 cairo;                SwiftUI macOS app)
 Linux + macOS)
```

**Hard rule:** nothing at or below `kmux-app` may depend on a UI toolkit.
`kmux-app` depends only on `kmux-client` + `kmux-protocol` (plus `clap`/`tabled`
for the CLI and `serde`/`toml` for config — none of which are UI toolkits).
`gtk4`/`gdk`/`cairo` live **only** in `kmux-gtk`. This is enforceable:
`cargo tree -p kmux-app` shows no `gtk4`.

## `AppCore`: driven, not driving

`kmux_app::core::AppCore` is the client view-model. It owns the session manager
plus all interaction and connection state (mode, pickers, server identity,
bootstrap orchestration, the agnostic color `palette`, command history, …). It
is a **passive state machine plus orchestration methods**: it never owns the run
loop, a terminal, or a widget. Each frontend *pumps* it.

The contract a frontend implements:

- **Input in** (toolkit-agnostic): the frontend produces an `Action` and calls
  `core.dispatch_action(action)` — the single entry point. *How* a frontend
  produces actions is its own choice. The GTK frontend is **accelerators-only**:
  native GTK accelerators, menu items, and widgets bind straight to `Action`s /
  `TopBarAction`s, and any key the accelerators don't claim is forwarded to the
  PTY. (The modal keymap `kmux_app::mode::resolve` remains available in the core
  for any frontend that prefers a chord-based input model.) Plus
  `core.set_term_size(size)` whenever the content
  area resizes (the frontend reports its geometry — the core never queries a
  terminal).
- **State out** (no toolkit types): read `core.mode`, `core.palette` (convert to
  your color type at the render leaf), `core.mgr.active_grid()` (the `CellGrid`
  to paint), `core.hud_visible`, the picker fields, etc.
- **Effects out**: `dispatch_action` returns a `KeyResult`. Besides the
  control-flow variants (`Quit`, `Reconnect`, `SwitchServer`), it can return
  `CopyToClipboard(String)` / `RequestPaste` — clipboard access is toolkit-
  specific, so the *frontend* performs it. `ForwardKey` (sending a keystroke to
  the PTY under the live terminal-mode state) also stays frontend-side, because
  the byte/escape encoding is toolkit-specific.
  - The same effect channel carries *server-originated* clipboard writes:
    `handle_session_events(events)` returns `Vec<KeyResult>` so an **OSC 52**
    copy from a remote pane (`SessionEventMsg::PaneClipboardCopy`) reaches the
    local clipboard. The *policy* lives in the core — it honors the write when it
    came from any pane in the session the client is currently viewing (not just
    the focused split, so the most recent OSC 52 write wins) and base64-decodes
    the payload — while the *mechanism* (the actual `set_text`) stays in the
    frontend, reusing the `CopyToClipboard` path. The daemon broadcasts OSC 52
    server-wide, so scoping to the active session is what keeps a pane in an
    unrelated background session from clobbering your clipboard.
- **Channels**: `AppCore` owns the tokio mpsc channels for *network* events
  (server messages, bootstrap outcome, transport upgrade, tunnel death) — those
  are core concerns. The frontend creates the channels, hands the senders to
  `core.start_bootstrap(...)`, and drains the receivers in its own loop.

The GTK frontend and `kmux-ffi` pump `AppCore` through the **`FrontendDriver`**
(below). The render leaf and how each frontend produces `Action`s differ; the
core is identical.

## `FrontendDriver`: the shared run loop

Driving `AppCore` has always meant the same arm-for-arm orchestration: own the
four network channels (server messages, bootstrap outcome, transport upgrade,
tunnel death), drain server messages → `mgr.handle_server_message` →
`handle_session_events`, settle a debounced resize, handle the bootstrap outcome
(and launch the SSH supervisor), apply a transport upgrade, react to a tunnel
death, tick the liveness ping + metrics flush, and advance the cursor blink.
That loop is **not** UI-specific, yet it used to be duplicated inside each
frontend — and for a non-Rust frontend reaching `AppCore` across an FFI boundary
(Swift cannot hold tokio channels, await receivers, or match `KeyResult`), it
could not be expressed at all.

`kmux_app::driver::FrontendDriver` lifts that orchestration into `kmux-app`. It
owns the `AppCore`, the four channels, the liveness/metrics timers, the resize
debounce, the blink phase, and the `/theme` change detection. A frontend:

- builds an `AppCore` (with its own capabilities), wraps it with
  `FrontendDriver::new` (which creates the channels and starts the initial
  bootstrap),
- calls `driver.tick()` once per frame from its own loop (a `glib` timeout, a
  `CVDisplayLink`, …) and acts on the returned `FrontendEffect`s
  (`NeedsRender`, `ForceClear`, `PaletteChanged`, `CopyToClipboard`,
  `RequestPaste`, `Quit`) — clipboard payloads arrive already NUL-sanitized,
- feeds input via `dispatch_action` / `apply_top_bar_action` /
  `activate_picker_selection` (these now return `Vec<FrontendEffect>` and apply
  reconnect / server-switch **internally** — the channel rebuild no longer lives
  in the frontend), plus `send_keys`, `send_input`, `feed_paste`,
  `request_resize` / `set_term_size`, `reconnect`,
- reads state out via `Deref<Target = AppCore>` (`driver.mgr`, `driver.mode`,
  `driver.palette`, …) plus `driver.active_grid()` and `driver.blink_on()`.

It owns no run loop and no runtime — it assumes an *ambient* tokio runtime (its
spawning paths use the current `Handle`), so the caller keeps control of the
loop and the runtime. The toolkit-agnostic helpers that used to live in the GTK
crate now live with it: `driver::blink::advance_blink` (the cursor-blink state
machine) and `driver::clipboard::sanitize_clipboard_text` (NUL stripping).

`kmux-gtk` runs on the driver: its `Frontend` is just
`{ driver, metrics, css_provider }`, and `pump` is `driver.tick()` + apply
effects + reconcile chrome + redraw. The driver has its own unit tests in
`kmux-app/src/driver/`.

## Theme: one palette, per-frontend colors

`kmux_app::theme` owns the source-of-truth palette as a toolkit-neutral `Rgb`
triple, parses the `themes/*.toml` files, and `kmux_app::config::resolve_theme`
returns it. `AppCore.palette` holds the active one (the `/theme` command mutates
it). Each frontend converts to its toolkit's color type at the render boundary:

- `kmux-gtk`: cairo `set_source_rgb` directly from the `Rgb`/`CellColor`
  components.
- `kmux-swift`: the FFI resolves `DEFAULT_*` cells against the palette in Rust
  and the renderer maps the packed colors to `NSColor`.

The field is named `palette` (not `theme`) so it does not shadow a frontend's
own rendered-theme field through a `Deref<Target = AppCore>` wrapper.

The palette includes `cursor_bg` / `cursor_fg` (optional in `themes/*.toml`,
defaulting to `fg` / `bg`). The frontend draws the inner-pane cursor itself and
honors these colors. See
[terminal-backend.md](terminal-backend.md#cursor-rendering-in-cell).

## Appearance: one font model, per-frontend metrics

Fonts and cell geometry follow the same toolkit-neutral pattern as the palette.
`kmux_app::appearance::Appearance` (font family + bold/italic variants, size,
style, OpenType features, cell-size adjustments) is resolved from `config.toml`
by `kmux_app::config::resolve_appearance` and held on `AppCore.appearance`. Each
frontend converts it to its toolkit's font/metrics types at the render leaf:

- `kmux-gtk`: a `pango::FontDescription` per style + a `pango::AttrFontFeatures`
  attribute list, built in `render::Metrics`.
- `kmux-swift`: an `NSFont` per style + CoreText feature settings, built in
  `TerminalMetrics`. The `Appearance` crosses the FFI as the `FfiAppearance`
  record (read via `KmuxDriver::appearance()`) — adding this getter bumped
  `KMUX_FFI_ABI_VERSION` to 8.

Like the palette, appearance never crosses the wire protocol — it is purely a
client render concern. See [appearance.md](appearance.md) for the config keys
and the cell-by-cell ligature limitation.

## Binaries and the shared CLI front door

The naming is: **`kmux`** is the entrypoint (the `kmux` crate's binary) on both
Linux and macOS — it offers the CLI and opens the platform desktop app;
**`kmux-gtk`** is the GTK frontend (the `kmux-gtk` crate's binary — the default +
official client on Linux, also runnable on macOS); **`kmux-swift`** is the native
macOS app; and **`kmuxd`** is the daemon.

`kmux` and the frontends all share one CLI front door — `kmux_app::launch::run_cli`.
It initializes logging, parses the CLI, runs any non-interactive subcommand
(`ls`, `daemon`, `--dry-run`) and returns `Launch::Done`, or returns
`Launch::Interactive(Plan)` — a frontend-agnostic description of the session to
launch. A frontend's `main` is then thin:

```rust
match run_cli(instance_id).await? {
    Launch::Done => Ok(()),
    Launch::Interactive(plan) => frontend::run(plan),  // builds AppCore + runs
}
```

Each frontend builds its own `AppCore` from the `Plan` (supplying its own
capabilities — the GUI declares truecolor) and runs its pump.

The **`kmux` entrypoint** is the same front door minus the in-process GUI: it
runs `run_cli` and, on `Launch::Interactive`, *execs* the platform desktop binary
(`kmux-gtk` on Linux, the Swift `kmux.app` bundle on macOS — located next to the
running executable, then on `PATH`, mirroring `find_server_binary` for `kmuxd`),
forwarding argv. The spawned frontend re-runs `run_cli` and rebuilds the same
`Plan` (a benign double-parse). `kmux` itself depends only on `kmux-app` (no UI
toolkit), so `kmux daemon start`, `kmux ls`, `kmux --server …` work identically
on both platforms without loading GTK — only the interactive presentation is
delegated to the per-platform frontend.

## Running

- Entrypoint: `cargo run -p kmux` (binary `kmux`) — runs the CLI subcommands and,
  for an interactive launch, execs the platform desktop app. For a dev GUI run,
  build the frontend too so the exec target exists (`just start` builds
  `kmux` + `kmux-gtk` then runs `kmux`).
- GUI directly: `cargo run -p kmux-gtk` (binary `kmux-gtk`) — opens a window,
  connects, renders the active session, forwards keystrokes (Linux + macOS).

### Building and running `kmux-gtk`

`kmux-gtk` links the system **GTK4** and **libadwaita** via `gtk4-rs` /
`libadwaita-rs`. These are build *and* runtime dependencies — they are linked
dynamically and are **not** bundled in the release tarball (unlike `kmuxd`'s
`libkmux_ghostty`, which is). Install them from your distro:

- Debian/Ubuntu: `libgtk-4-dev libadwaita-1-dev`
- Fedora: `gtk4-devel libadwaita-devel`
- macOS (Homebrew): `brew install gtk4 libadwaita` — `kmux-gtk` is the Linux
  default but also runs on macOS, where the native default frontend is `kmux-swift`

On a machine where another `pkg-config` (e.g. a Homebrew/linuxbrew one) shadows
the system one in `PATH`, gtk4 resolution fails on transitive X11 `.pc` files.
Point cargo at the system pkg-config for any build that includes `kmux-gtk`:

```
PKG_CONFIG=/usr/bin/pkg-config cargo build -p kmux-gtk
```

This is a machine `PATH` quirk, not a repo setting; on a standard install the
default `pkg-config` resolves gtk4 directly.

`just install` installs the `kmux` entrypoint + the `kmux-gtk` GUI (Linux) plus
its `.desktop` entry and icon into the XDG data dirs (on macOS it installs `kmux`
and instead assembles `~/Applications/kmux.app` — see [the native macOS
frontend](#the-native-macos-frontend-kmux-swift) and
[building-macos.md](building-macos.md#install)); `just package` stages `kmux` +
the `kmux-gtk` binary + desktop files into the release tarball under `share/`.
`kmux-gtk` is the default client on Linux, launched by the `kmux` entrypoint;
preferences (theme + font) open with **Ctrl+,**.

## The native macOS frontend (`kmux-swift`)

On macOS the native client is **`kmux-swift`**, a SwiftUI app that drives the
same `FrontendDriver` as `kmux-gtk` — across the **`kmux-ffi`** uniffi C-ABI
boundary (Swift cannot hold tokio channels, await receivers, or match
`KeyResult`, so it reaches `AppCore` only through the driver). It lives in
`kmux-swift/` as a SwiftPM package *outside* the cargo workspace (so `cargo`
ignores it) and links the `kmux-ffi` staticlib.

- **The seam (`kmux-ffi`).** An opaque, thread-confined `KmuxDriver` wraps
  `FrontendDriver` and owns the tokio runtime its background tasks run on. All
  calls come from the Swift main thread. The surface is versioned
  (`KMUX_FFI_ABI_VERSION`, asserted on the Swift side on top of uniffi's
  binding-checksum check). Beyond lifecycle + `tick` → `FfiEffect`s it exposes: a
  generation-gated packed-cell `grid_snapshot` (16-byte cells, `DEFAULT_*`
  resolved against the palette in Rust, scrollback composited into the visible
  rows when scrolled — see `kmux-ffi/src/cells.rs`); structured
  **mode-aware** key input (`send_char` / `send_named_key`, routed through the
  daemon's Ghostty encoder via `send_keys`, so no escape sequences are
  hand-rolled); the **tiling surface** — `tabs()` + `select_tab`, `layout(area)`
  (the shared resolver's per-pane rects), `dividers(area)` + `apply_divider_drag`
  / `reset_divider` (interactive mouse resize, exposing
  `kmux_app::layout::resolve_dividers` / `ratios_for_drag`), per-pane
  `grid_snapshot_for` / `selection_for` / `scroll_info_for`, `focus_pane`,
  `set_pane_sizes`, `rename_tab`, and the
  split/focus/resize/swap/scheme/zoom/`FocusPaneAt` `FfiAction`s (see
  [layout.md](layout.md));
  scroll- and wrap-aware text
  selection (per-visible-row wash spans, working while scrolled into history) +
  `scroll_at`/`scroll_lines`; the
  `/`-command palette (`command_hints` / `run_command`); a generic `picker`
  getter + drivers for the session/server/directory pickers; session
  `rename`/`close`; metrics + HUD visibility; and `theme` get/set. Every addition
  maps to an existing toolkit-agnostic `AppCore` capability `kmux-gtk` already
  uses — exposure across the boundary, not new policy.
- **The app (`kmux-swift`).** A SwiftPM package with three targets: the
  uniffi-generated C header (`kmux_ffiFFI`, a systemLibrary), the generated Swift
  bindings (`KmuxBindings`), and the app (`KmuxApp`). A main-thread timer pumps
  `KmuxDriver::tick` ~60 Hz (the analog of the GTK `glib` timeout) and acts on the
  returned `FfiEffect`s. The terminal grid is a flipped `NSView` CoreText/
  CoreGraphics renderer (the analog of the cairo/Pango `render.rs`: cell bg +
  glyph passes, text attributes, wide chars, the four cursor shapes + blink,
  selection wash, scroll indicator). Like the GTK leaf it **tiles** the active
  tab's panes from `layout()` (clip + translate per pane, focus border,
  click-to-focus, pane-relative selection/scroll), with divider drag-resize +
  hover cursor (from `dividers()`/`apply_divider_drag`), a right-click pane
  context menu, and `⌘1…9` numbered focus. Everything around it is native
  SwiftUI — a
  sessions sidebar, a tab strip (from `tabs()`, with a rename/close context
  menu), a header with the connection
  badge, the
  command palette, the pickers, session + tab rename/close, preferences (theme),
  and the performance HUD/metrics — each driven by the FFI getters/dispatch,
  file-for-file parallel to `kmux-gtk`'s `sidebar.rs`/`tabs.rs`/`header.rs`/
  `dialogs.rs`/`prefs.rs`.
- **Platform gating.** `kmux-gtk`'s GTK4/libadwaita deps are target-gated to
  Linux **and macOS** (macOS needs Homebrew GTK: `brew install gtk4 libadwaita`),
  and its `main.rs` compiles to a stub only on other targets. `kmux-ffi` is pure
  Rust and builds everywhere; the `kmux` entrypoint is toolkit-free and builds
  everywhere; the Swift app is macOS-only by nature (SwiftPM + macOS CI /
  justfile guards). Note: a full `cargo build --workspace` on macOS now requires
  Homebrew GTK (because `kmux-gtk` is a real target there); the macOS CI job is
  selective rather than `--workspace`.

Build + run (bindings generation, linking, the macOS CI path) are in
[building-macos.md](building-macos.md). The generated bindings are not committed;
`just gen-ffi-bindings` produces them (the ABI assert + uniffi checksum guard
drift).

## Status

The GTK GUI (`kmux-gtk`) is the **primary** client on Linux and is a *native*
libadwaita app — only the terminal panes are drawn like a terminal; everything
around them uses native widgets. It drives `AppCore` through the contract above;
the GTK-specific parts are the pump, the render leaf, and the native widget
mapping:

- Full Pango grid rendering — text attributes, wide chars, cursor shapes, and
  scrollback — at font-derived cell metrics (`render.rs`). One shared
  `DrawingArea` **tiles** the active tab's panes: it resolves the tab's shared
  `LayoutNode` tree against its pixel size via the toolkit-agnostic
  `kmux-app/layout` resolver, then clips + translates per pane and accent-borders
  the focused one (`render_tiled` + `tiles.rs`). The selection wash and
  pointer→cell mapping are scroll- and wrap-aware via the shared
  `CellGrid::visible_selection_spans` / `visible_to_abs` primitives, so they work
  identically while scrolled into history — the same primitives back the Swift
  renderer and the FFI selection getters.
- **Native chrome** (`shell.rs`): an `adw::HeaderBar` (server/session title,
  connection-status indicator, server-switch, command-palette, primary menu); a
  collapsible **sessions sidebar** (`adw::OverlaySplitView` + `GtkListBox`,
  `sidebar.rs`); and a **tab strip** (`adw::TabBar`/`TabView`, `tabs.rs`).
  Sessions and tabs are reconciled against `AppCore` each pump tick (cheap
  per-region signatures); selecting/closing routes through `TopBarAction` /
  `Action`. Under the **Session → Tab → Pane** model the strip shows *tabs* (each
  a tiled layout of one or more panes), not individual panes — see
  [layout.md](layout.md).
- **Accelerators-only keyboard** (`actions.rs`): each command is a `gio` action
  bound to a reserved accelerator (`Ctrl+Shift+…`, function keys, `Ctrl+digit`),
  surfaced in a hamburger menu and a `GtkShortcutsWindow`; the key controller on
  the focused terminal forwards everything the accelerators don't claim to the
  PTY. The modal chord path (`mode::resolve`) is available in the core but not
  used by the GUI.
- **Native dialogs** (`dialogs.rs`), all driven by `core.mode`: session/server/
  directory pickers and the `/`-command palette as `adw::Dialog`s (search entry
  + reconciled list); confirm-close/rename as `adw::AlertDialog`s; connecting/
  disconnected as an `adw::Banner`; status messages as `adw::Toast`s; the
  metrics inspector as an `adw::Dialog`; and the performance HUD as a live
  `.osd` ticker.
- A `glib` pump that calls `FrontendDriver::tick` each frame and applies the
  returned `FrontendEffect`s; the orchestration it used to inline (reconnect,
  server switch, SSH supervisor, transport upgrade, tunnel death, liveness ping
  + timeout, metrics flush, resize debounce, cursor blink) now lives in the
  shared driver. Async clipboard paste (a `RequestPaste` effect → GDK async read
  → `driver.feed_paste`) stays GTK-side.
- Mouse scroll-wheel (PTY mouse-report or local scrollback) and drag text
  selection with copy — selection works while scrolled into history, and a drag
  held at the top/bottom edge auto-scrolls so it can span more than one screen.
  When the focused pane's program has enabled mouse tracking (DEC 1000/1002/1003,
  SGR 1006), a primary-button **press/drag/release** is instead encoded and
  forwarded to the PTY so the program (vim, tmux, htop, …) owns the mouse — the
  decision and encoding are the shared, toolkit-agnostic
  `SessionManager::report_mouse` + `kmux_client::input::encode_mouse_button`
  (siblings of the existing `encode_mouse_scroll`). Holding **Shift** is the
  bypass key: it always forces local selection even inside a mouse-mode program.
  The Swift frontend reaches the same policy through the FFI `mouse_event` method.
- libadwaita styling: the kmux palette feeds libadwaita's `accent_*` named
  colors (reloaded on `/theme`), so the chrome follows the active theme with
  stock styling; preferences (theme + font) open with **Ctrl+,**.

The one toolkit-agnostic addition this required was `AppCore::set_picker_search`
(a sibling of `set_picker_selected`) so a native search entry can drive a picker
filter in one shot; everything else reused the existing `Action` / dispatch /
picker-query surface.

### Future

- **Frontend selection from `kmux`.** The `kmux` entrypoint launches the platform
  desktop app by `exec`. A future flag (e.g. `kmux --gtk` on macOS) could let it
  pick a non-default frontend.
- **macOS interactive args.** `kmux` forwards argv to the Swift `kmux.app`, but
  the Swift app currently ignores connect flags (`--server`/`--theme`/…) and uses
  defaults; honoring them there (parsing argv into a `DriverConfig`) is a
  follow-up. CLI subcommands (`daemon`/`ls`/`--dry-run`) already work via `kmux`.
- **Native macOS polish.** The `kmux-swift` app (see the section above) is
  functional and at feature parity with `kmux-gtk`. Remaining polish: a Metal
  renderer + same-attr run batching if profiling shows need; a configurable font
  in Preferences (the renderer currently uses the system monospaced face); a
  **codesigned + notarized** `.app` bundle (`just install` already assembles an
  unsigned `~/Applications/kmux.app` — see
  [building-macos.md](building-macos.md#install)).
- **Windows.** A native Windows frontend would also drive `FrontendDriver`. The
  Unix-only client paths (`flock`, UDS, daemon spawn) need cfg-gating — see
  `kmux-protocol/src/dirs.rs` for the path resolvers that would gain
  `#[cfg(windows)]` branches.
- **GTK render polish.** Partial damage tracking (`queue_draw_area`, keyed off
  `CellGrid::cells_generation`) and same-attr run batching in the Pango
  renderer if profiling shows need; and
  the per-category + RTT detail in the metrics overlay.
