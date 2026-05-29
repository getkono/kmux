# Client frontend architecture

This document describes how the kmux **client** is layered so that the same
core can drive multiple frontends — the terminal UI (`kmux-tui`) and a native
GUI (`kmux-gtk`, GTK4 on Linux; other platforms later). It is the result of the
TUI→GUI extraction: pulling everything that is *not* terminal-specific out of
the TUI binary into a shared, toolkit-agnostic core.

## Layering

```
kmux-protocol   wire protocol, transport traits, dirs/paths, auth
      │
kmux-client     MECHANISM: SessionManager, terminal grid model (CellGrid),
      │         transports/bootstrap/SSH/supervisor, key model (key::Key)
      ▼
kmux-app        INTERACTION POLICY (toolkit-agnostic): Mode/Action + resolve,
      │         the /-command palette, AppCore view-model + dispatch +
      │         connection orchestration, theme palette (Rgb) + config,
      │         recent-servers, and the non-interactive CLI subcommands
      ├───────────────┬─────────────────
      ▼               ▼
kmux-tui          kmux-gtk            FRONTENDS (presentation only)
(ratatui +        (GTK4 + glib +
 crossterm)        cairo)
```

**Hard rule:** nothing at or below `kmux-app` may depend on a UI toolkit.
`kmux-app` depends only on `kmux-client` + `kmux-protocol` (plus `clap`/`tabled`
for the CLI and `serde`/`toml` for config — none of which are UI toolkits).
`ratatui`/`crossterm` live **only** in `kmux-tui`; `gtk4`/`gdk`/`cairo` live
**only** in `kmux-gtk`. This is enforceable: `cargo tree -p kmux-app` shows no
`ratatui`/`crossterm`/`gtk4`.

## `AppCore`: driven, not driving

`kmux_app::core::AppCore` is the client view-model. It owns the session manager
plus all interaction and connection state (mode, pickers, server identity,
bootstrap orchestration, the agnostic color `palette`, command history, …). It
is a **passive state machine plus orchestration methods**: it never owns the run
loop, a terminal, or a widget. Each frontend *pumps* it.

The contract a frontend implements:

- **Input in** (toolkit-agnostic): `handle_key`-style flow — convert the
  toolkit's key event to `kmux_client::key::{Key, Modifiers}`, call
  `kmux_app::mode::resolve(&core.mode, &key, mods)` to get the next `Mode` +
  `Action`, then `core.dispatch_action(action)`. Plus `core.set_term_size(size)`
  whenever the content area resizes (the frontend reports its geometry — the
  core never queries a terminal).
- **State out** (no toolkit types): read `core.mode`, `core.palette` (convert to
  your color type at the render leaf), `core.mgr.active_grid()` (the `CellGrid`
  to paint), `core.hud_visible`, the picker fields, etc.
- **Effects out**: `dispatch_action` returns a `KeyResult`. Besides the
  control-flow variants (`Quit`, `Reconnect`, `SwitchServer`), it can return
  `CopyToClipboard(String)` / `RequestPaste` — clipboard access is toolkit-
  specific, so the *frontend* performs it. `ForwardKey` (sending a keystroke to
  the PTY under the live terminal-mode state) also stays frontend-side, because
  the byte/escape encoding is toolkit-specific.
- **Channels**: `AppCore` owns the tokio mpsc channels for *network* events
  (server messages, bootstrap outcome, transport upgrade, tunnel death) — those
  are core concerns. The frontend creates the channels, hands the senders to
  `core.start_bootstrap(...)`, and drains the receivers in its own loop.

The TUI pumps `AppCore` from a `tokio::select!` loop (`kmux-tui/src/app/
event_loop.rs`); the GTK frontend pumps it from a `glib` timeout
(`kmux-gtk/src/main.rs::pump`). Only the pump and the render leaf differ.

## Theme: one palette, per-frontend colors

`kmux_app::theme` owns the source-of-truth palette as a toolkit-neutral `Rgb`
triple, parses the `themes/*.toml` files, and `kmux_app::config::resolve_theme`
returns it. `AppCore.palette` holds the active one (the `/theme` command mutates
it). Each frontend converts to its toolkit's color type at the render boundary:

- `kmux-tui`: a ratatui-typed `Theme` built via `From<kmux_app::theme::Theme>`,
  refreshed from `core.palette` before each draw (`kmux-tui/src/theme.rs`).
- `kmux-gtk`: cairo `set_source_rgb` directly from the `Rgb`/`CellColor`
  components.

The field is named `palette` (not `theme`) so it does not shadow a frontend's
own rendered-theme field through the `App: Deref<Target = AppCore>` wrapper.

## The TUI `App` wrapper

`kmux-tui`'s `App` is a thin presentation wrapper: `{ core: AppCore, theme
(ratatui), hit-boxes, paste_tx }`. It `Deref`s to `AppCore` so the event loop
and renderers reach core state (`self.mgr`, `self.mode`, …) directly — a
deliberate newtype-wrapper ergonomic. A native GUI frontend wraps the same
`AppCore` the same way (or holds it directly, as `kmux-gtk` does).

## Binaries and the shared CLI front door

The naming is: **`kmux`** is the GUI (the `kmux-gtk` crate's binary, Linux);
**`kmux-tui`** is the terminal client (the `kmux-tui` crate's binary, kept for
SSH/headless use); **`kmuxd`** is the daemon.

Both client binaries share one CLI front door — `kmux_app::launch::run_cli`. It
initializes logging, parses the CLI, runs any non-interactive subcommand
(`ls`, `daemon`, `--dry-run`) and returns `Launch::Done`, or returns
`Launch::Interactive(Plan)` — a frontend-agnostic description of the session to
launch. Each binary's `main` is then thin:

```rust
match run_cli(instance_id).await? {
    Launch::Done => Ok(()),
    Launch::Interactive(plan) => frontend::run(plan),  // builds AppCore + runs
}
```

The frontend builds its own `AppCore` from the `Plan` (supplying its own
capabilities — the TUI probes the terminal; the GUI declares truecolor) and
runs its pump. So `kmux daemon start`, `kmux ls`, `kmux --server …`, etc. all
work on either binary; only the interactive presentation differs.

## Running

- GUI: `cargo run -p kmux-gtk` (binary `kmux`) — opens a window, connects,
  renders the active session, forwards keystrokes. Proof-of-seam GTK scaffold.
- TUI: `cargo run -p kmux-tui` (binary `kmux-tui`).

### Building and running `kmux-gtk`

`kmux-gtk` links the system **GTK4** and **libadwaita** via `gtk4-rs` /
`libadwaita-rs`. These are build *and* runtime dependencies — they are linked
dynamically and are **not** bundled in the release tarball (unlike `kmuxd`'s
`libkmux_ghostty`, which is). Install them from your distro:

- Debian/Ubuntu: `libgtk-4-dev libadwaita-1-dev`
- Fedora: `gtk4-devel libadwaita-devel`

On a machine where another `pkg-config` (e.g. a Homebrew/linuxbrew one) shadows
the system one in `PATH`, gtk4 resolution fails on transitive X11 `.pc` files.
Point cargo at the system pkg-config for any build that includes `kmux-gtk`:

```
PKG_CONFIG=/usr/bin/pkg-config cargo build -p kmux-gtk
```

This is a machine `PATH` quirk, not a repo setting; on a standard install the
default `pkg-config` resolves gtk4 directly.

`just install` installs the `kmux` GUI (Linux) plus its `.desktop` entry and
icon into the XDG data dirs; `just package` stages the GUI binary + desktop
files into the release tarball under `share/`. The GUI is the primary `kmux`
command on Linux; preferences (theme + font) open with **Ctrl+,**.

## Status

The GTK GUI (`kmux`) has reached feature parity with the TUI and is the
**primary** client on Linux. It drives `AppCore` through the same contract as
the TUI; only the pump and the render/input leaves are GTK-specific:

- Full Pango grid rendering — text attributes, wide chars, cursor shapes, and
  scrollback — at font-derived cell metrics (`kmux-gtk/src/render.rs`).
- Structured key forwarding (the daemon's Ghostty encoder), and a `glib` pump
  that mirrors every arm of the TUI's `tokio::select!`: reconnect, server
  switch, SSH supervisor, transport upgrade, tunnel death, liveness ping +
  timeout, metrics flush, resize debounce, plus async clipboard paste.
- Native chrome (session/status/hint bars) and overlays (session/server/dir
  pickers, command palette, help, confirm-close, rename, connecting/
  disconnected, HUD, metrics) as views of `AppCore` state.
- Mouse scroll-wheel (PTY mouse-report or local scrollback) and drag text
  selection with copy.
- libadwaita window styling with palette-driven CSS (reloaded on `/theme`) and
  a preferences window for theme + font, opened with **Ctrl+,**.

`kmux-tui` is **deprecated** but kept building and tested: it remains the
headless/SSH client and the regression oracle for the shared `kmux-app`
interaction layer. No new feature work targets it.

### Future

- **In-process frontend selection.** Today you run `kmux` (GUI) or `kmux-tui`
  (TUI) as separate binaries. A `kmux --tui` flag could launch the terminal
  frontend in-process from the GUI binary when there is no display.
- **Other platforms.** macOS/Windows native frontends (e.g. `kmux-cocoa`) would
  each provide the `kmux` binary for their platform, built per-target. The
  Unix-only client paths (`flock`, UDS, daemon spawn) need cfg-gating for a
  Windows client — see `kmux-protocol/src/dirs.rs` for the path resolvers that
  would gain `#[cfg(windows)]` branches.
- **GTK render polish.** Partial damage tracking (`queue_draw_area`, keyed off
  `CellGrid::cells_generation`) and same-attr run batching in the Pango
  renderer if profiling shows need; selection within scrolled-back history; and
  the per-category + RTT detail in the metrics overlay.
