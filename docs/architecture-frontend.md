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

## Running

- TUI: `cargo run -p kmux-tui` (the binary is still named `kmux` during the
  migration; see the launcher note below).
- GTK scaffold: `cargo run -p kmux-gtk` — opens a window, connects to the local
  daemon, renders the active session, forwards keystrokes. Proof-of-seam, not a
  finished GUI.

### Building `kmux-gtk` and the system pkg-config

`kmux-gtk` links the system GTK4 via `gtk4-rs`. On a machine where another
`pkg-config` (e.g. a Homebrew/linuxbrew one) shadows the system one in `PATH`,
gtk4 resolution fails on transitive X11 `.pc` files. Point cargo at the system
pkg-config for any build that includes `kmux-gtk`:

```
PKG_CONFIG=/usr/bin/pkg-config cargo build -p kmux-gtk
```

This is a machine `PATH` quirk, not a repo setting; on a standard install the
default `pkg-config` resolves gtk4 directly.

## Remaining: the `kmux` launcher (not yet implemented)

The intended end state is a thin `kmux` launcher binary that:

1. parses the CLI and runs the non-interactive subcommands (`ls`, `daemon`,
   `--dry-run`) — these already live in `kmux-app` (frontend-free);
2. for the interactive path, builds an `AppCore` and launches the
   cfg-selected native frontend — GTK on Linux by default, `--tui` (or a build
   feature) for the terminal frontend.

At that point the user-facing `kmux` command becomes the GUI and the terminal
client is the standalone `kmux-tui` binary (kept for SSH/headless use). The
shared CLI front door means neither binary needs the other's toolkit. Until the
launcher lands, `kmux-tui` and `kmux-gtk` are run as separate binaries.
