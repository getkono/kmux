# Multi-pane tiling (Session → Tab → Pane)

kmux tiles multiple live terminals on screen at once, with tmux/zellij-style
splits, focus, resize, swap, preset layouts, and zoom. This document describes
the model, the determinism contract that keeps it correct across clients, the
server-authoritative mutation flow, and the keymap.

## The model

```
Session ──┬── Tab ──┬── Pane (one PTY)
          │         ├── Pane
          │         └── Pane
          └── Tab ──── Pane
```

- A **Session** (`word_id`, e.g. `eagle`) owns a flat pool of **panes**, each one
  PTY identified by `PaneId = "{word_id}/{pane_index}"`. `pane_index` is
  monotonic per session and is the *only* id in the hot PTY path (attach, diffs,
  scrollback) — it is unchanged from the pre-tiling design.
- A **Tab** is a named tiling layout over a subset of the session's panes. One
  client views one tab at a time; *which* tab is client-local, but the tab's
  layout tree and focus are **server-authoritative and shared** across all
  clients viewing it (and persist across detach/reattach).
- A **Pane** is a leaf of a tab's layout tree (a single tiled PTY).

This is the zellij naming. The user-facing "pane" of older kmux (a switchable
full-screen PTY shown as a chrome tab) is now a **Tab**; a **Pane** is a tiled
split inside one.

## The layout tree (`LayoutNode`)

`kmux_protocol::messages::LayoutNode` (in `crates/kmux-protocol/src/messages/session.rs`)
is a resolution-independent tree:

```rust
enum LayoutNode {
    Leaf  { pane_index: u32 },
    Split { dir: SplitDir, ratios: Vec<u16>, children: Vec<LayoutNode> },
}
```

- `SplitDir::Horizontal` lays children **left ↔ right**; `Vertical` lays them
  **top ↕ bottom**.
- `ratios` are **permille** integers (`u16`, ~summing to 1000), one per child.
  Permille (not `f32`) is deliberate: the tree is **bit-exact** across clients
  and safe to compare for change-suppression. Never compare layouts with float
  equality.
- A `TabInfo` carries `{ tab_index, name, layout, focused_pane }`. The
  `focused_pane` is the shared input target within the tab.

## The shared resolver (the determinism contract)

The single most important shared component is `kmux_app::layout::resolve_layout`
(in `crates/kmux-app/src/layout.rs`). It turns a `(tree, area_cols, area_rows,
config)` into one `PaneRect { pane_index, col, row, cols, rows }` per leaf. **All
three frontends call it; none reimplements it** (it lives below the UI-toolkit
line, so GTK, Swift-via-FFI, and any future frontend share it byte-for-byte).

Why determinism is a *hard* requirement, not a nicety: each client resolves the
*same shared ratio tree* against *its own* window into per-pane cell rects, then
attaches each pane at that sub-size. The daemon's smallest-wins
`PaneRelay::effective_size()` reconciles the differing per-client sizes. If two
clients with the same window disagreed by a single cell, the PTY would thrash
(resize-loop). So the resolver is:

- **Integer-only largest-remainder apportionment** — parts sum to exactly the
  available cells; remainder ties break toward the lower index.
- **Gutters subtracted before apportioning** — a 1-cell divider between children
  is removed from the axis first, then the remainder is split by ratio.
- **Min-size clamped** — never emits a 0-cell pane.

`LayoutConfig::default()` (gutter 1×1, min 1×1) is what every frontend passes, so
they agree. `focus_neighbor` (the geometric tmux/zellij neighbor search) and
`resize_split` (which split + ratios a keyboard resize should send) also live
here, so focus and resize geometry are shared too.

The frontend resolves, renders each pane into its rect (GTK clips + translates on
one `DrawingArea`; Swift does the same on one `NSView`), and pushes the resolved
sizes back to the client via `set_pane_sizes`, which `Resize`s each attached pane
to its tile.

## Server-authoritative mutation + the race model

Every layout change is applied **on the daemon, under the `sessions` write lock**,
in `crates/kmuxd/src/app/layout.rs` (pure, PTY-free, unit-tested tree mutations)
and `tab_crud.rs` (the locked wrappers). After any mutation the daemon broadcasts
the single authoritative

```
ServerMessage::LayoutUpdate { word_id, tab_index, layout, focused_pane }
```

to every client viewing the tab. **Clients never merge trees** — they replace
their cached copy and reconcile their attach set + focus. This is last-writer-
wins; a client may apply an optimistic local change but always reconciles to the
next `LayoutUpdate` (mirroring the existing server-authoritative tab-close veto).

The pure mutations:

- **`split`** — splits a leaf, *appending a sibling* when the parent already
  splits in the same direction (flatter, tmux-like) rather than nesting.
- **`remove_pane`** — collapses the parent when it drops to one child and
  redistributes the freed ratio. The out-of-band `PaneExited` path (a program
  exiting, not a `PaneClose`) **also** collapses + broadcasts, so the tree never
  references a dead leaf.
- **`set_ratios`** — clamps each child to a `MIN_RATIO` (20‰) and renormalizes to
  1000.
- **`swap`** — exchanges two leaves' `pane_index` in place (ratios untouched);
  focus follows the moved PTY.
- **`apply_scheme`** — regenerates the tree into a preset (see below).

Lifecycle edges: closing the last pane in a tab closes the tab; closing the last
tab closes the session.

## Operations

| Operation | Wire intent | Server | Notes |
|-----------|-------------|--------|-------|
| Split L/R/U/D | `PaneSplit` | spawn PTY, `split` | new pane gets focus |
| Move focus | `SetFocus` | `set_tab_focus` | target chosen by `focus_neighbor` (client-side) |
| Resize | `SetLayoutRatios` | `set_ratios` | ratios computed by `resize_split` (client-side) |
| Swap | `PaneSwap` | `swap` | focus follows the moved pane |
| Close pane | `PaneClose` | `remove_pane` (collapse) | last pane → close tab |
| New / close / rename tab | `TabCreate` / `TabClose` / `TabRename` | tab CRUD | |
| Select tab | *(client-local)* | — | which tab is viewed is not shared |
| Preset layout | `ApplyLayoutScheme` | `apply_scheme` | server rebuilds the tree |
| Zoom | *(client-local)* | — | a view flag; see below |

### Preset layouts (schemes)

`ApplyLayoutScheme` rebuilds a tab's tree from its current panes (in leaf order)
into one of `LayoutScheme::{EvenHorizontal, EvenVertical, MainVertical,
MainHorizontal}` — the tmux preset layouts. `Main*` puts the first pane in a
~50% "main" region and evenly arranges the rest in the orthogonal "stack".
`cycle_layout` rotates through the presets (tmux "next-layout").

### Zoom

Zoom is purely **client-local** — a view flag, no tree mutation, no protocol
message. `SessionManager::render_layout()` returns the tab's tree normally, or a
single-leaf layout of just the focused pane when zoomed; frontends render/size
against `render_layout()` (not `active_layout()`), so a zoomed tab fills the area
with the focused pane and the others stay attached but hidden. Toggling zoom off
restores the tiled tree.

## Keymap

GTK uses reserved accelerators (it never shadows keys the inner program needs);
the macOS app uses the parallel ⌘-based shortcuts in its "Pane" / "Session" menus.

| Action | GTK (`kmux-gtk`) | macOS (`kmux-swift`) |
|--------|------------------|----------------------|
| Split right / down | `Ctrl+Shift+\` / `Ctrl+Shift+-` | `⌘D` / `⌘⇧D` |
| Move focus | `Ctrl+Alt+←/→/↑/↓` | `⌘⌥←/→/↑/↓` |
| Resize | `Ctrl+Shift+Alt+←/→/↑/↓` | `⌘⌃←/→/↑/↓` |
| Move pane (swap) | `Ctrl+Shift+,` / `Ctrl+Shift+.` | `⌘⌃[` / `⌘⌃]` |
| Cycle preset layout | `Ctrl+Shift+Space` | `⌘⇧Space` |
| Zoom focused pane | `Ctrl+Shift+Z` | `⌘⌃Z` |
| New tab | `Ctrl+Shift+T` | `⌘T` |
| Next / previous tab | `Ctrl+Shift+→` / `Ctrl+Shift+←` | `⌘⌥]` / `⌘⌥[` |
| Rename tab | `Shift+F2` | *(native sheet — follow-up)* |
| Close tab | *(tab-bar ✕ / menu)* | *(menu)* |
| Close pane | `Ctrl+Shift+Q` | `⌘⇧W` |

## Versioning

- `PROTOCOL_VERSION = 21` — the tab + layout messages (`PaneSplit`, `PaneSwap`,
  `SetLayoutRatios`, `SetFocus`, `Tab*`, `ApplyLayoutScheme`, `LayoutUpdate`).
- `STATE_VERSION = 3` — daemon checkpoint persistence; the v2→v3 migration wraps
  each persisted session's panes in a default one-tab-one-pane layout.
- `KMUX_FFI_ABI_VERSION = 4` — the Swift tiling surface (`tabs`/`layout`/per-pane
  grid/`focus_pane`/`set_pane_sizes` + the tiling/scheme/zoom `FfiAction`s).
