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
`resize_split` (which split + ratios a *keyboard* resize should send) also live
here, so focus and resize geometry are shared too. So does the **mouse** resize
path: `resolve_dividers` enumerates the draggable gutter boundaries (each with a
split `path`, the pair's axis span, and a hit rectangle), and `ratios_for_drag`
maps a pointer cell to the split's new ratios — clamped to the same `MIN_RATIO`
floor as keyboard resize. Both frontends hit-test and drag against these, so a
mouse resize is as deterministic as a keyboard one (and emits the same
`SetLayoutRatios`). `resolve_dividers` is empty for a single leaf, so a zoomed
tab (whose `render_layout()` is one leaf) has nothing to drag.

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
| Resize | `SetLayoutRatios` | `set_ratios` | ratios from `resize_split` (keyboard) or `ratios_for_drag` (mouse divider drag), client-side; double-clicking a divider resets it to even (`even_ratios_at`) |
| Swap | `PaneSwap` | `swap` | focus follows the moved pane |
| Close pane | `PaneClose` | `remove_pane` (collapse) | last pane → close tab |
| New / close / rename tab | `TabCreate` / `TabClose` / `TabRename` | tab CRUD | |
| Select tab | *(client-local)* | — | which tab is viewed is not shared |
| Preset layout | `ApplyLayoutScheme` | `apply_scheme` | server rebuilds the tree |
| Zoom | *(client-local)* | — | a view flag; see below |

### Soft-close (the 3-second undo, issue #86)

Closing a pane (`Ctrl+Shift+Q`, or `/pane close`) is **deferred**, not immediate, so an accidental close is recoverable. This is purely **client-side** interaction policy in `kmux-app` — the wire `PaneClose` (and thus the kill) is simply withheld:

- On a close request for a **healthy** pane (its shell is `Running`), `AppCore` records a `PendingClose { pane_id, deadline: now + SOFT_CLOSE_GRACE }` (3 s) instead of sending `PaneClose`. The frontends show an **Undo** affordance (a toast button on GTK, a banner on macOS); `Ctrl+Shift+U` / `⌘⇧U` also undoes.
- The frontend pump (`FrontendDriver::tick`) calls `AppCore::fire_due_closes`, which sends the real `PaneClose` for each pane whose deadline has passed.
- Cancelling — via **Undo** or by **re-selecting the pane** within the window — drops the `PendingClose`; the live shell was never touched, so the pane is restored as-is.
- An already-**exited** pane (`is_pane_running` is false) skips the grace and closes at once.

Re-opening *after* the kill is just an ordinary new pane/session — there is no special daemon-side resurrection (the kill is final once the wire `PaneClose` is sent). Tab close remains immediate. Session close always enters a destructive confirmation dialog before the client sends `SessionClose`.

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

## Mouse interaction

Both frontends share the same pointer behaviors (GTK via `kmux-gtk`'s
`imp::input`/`imp::tiles`, macOS via `MouseInput.swift` against the FFI divider
surface):

- **Click-to-focus** — a press in a tile focuses it. Selection and scroll are
  **pane-relative**: coordinates are mapped into the pane under the pointer's
  local cell grid (the scroll wheel scrolls that tile's scrollback), not the
  whole window.
- **Drag a divider** to resize the split live; the pointer shows a col-/row-
  resize cursor on hover. **Double-click a divider** to reset that split to even.
  A press within a few px of the 1-cell gutter grabs the divider (suppressing
  text selection).
- **Right-click a pane** for a context menu (split right/down, zoom, close pane)
  — it focuses the pane under the pointer first.

## Keymap

GTK uses reserved accelerators (it never shadows keys the inner program needs);
the macOS app uses the parallel native shortcuts in its "Pane" / "Session" menus.

| Action | GTK (`kmux-gtk`) | macOS (`kmux-swift`) |
|--------|------------------|----------------------|
| Split right / down | `Ctrl+Shift+\` / `Ctrl+Shift+-` | `⌘D` / `⌘⇧D` |
| Move focus | `Ctrl+Alt+←/→/↑/↓` | `⌘⌥←/→/↑/↓` |
| Jump to session by number | `Ctrl+1…9` | `⌘1…9` |
| Focus pane by number | `Alt+1…9` | `⌘⌥1…9` |
| Resize (keyboard) | `Ctrl+Shift+Alt+←/→/↑/↓` | `⌘⌃←/→/↑/↓` |
| Resize (mouse) | drag a divider · double-click → even | drag a divider · double-click → even |
| Move pane (swap) | `Ctrl+Shift+,` / `Ctrl+Shift+.` | `⌘⌃[` / `⌘⌃]` |
| Cycle preset layout | `Ctrl+Shift+Space` | `⌘⇧Space` |
| Zoom focused pane | `Ctrl+Shift+Z` | `⌘⌃Z` |
| Pane context menu | right-click a pane | right-click a pane |
| New tab | `Ctrl+Shift+T` | `⌘T` |
| Next / previous tab | `Ctrl+Shift+→` / `Ctrl+Shift+←` | `⌘⌥]` / `⌘⌥[` |
| Next / previous session | `Ctrl+Tab` / `Ctrl+Shift+Tab` | `Ctrl+Tab` / `Ctrl+Shift+Tab` (also `⌘⇧]` / `⌘⇧[`) |
| Rename session / tab | `F2` / `Shift+F2` | `F2` / `Shift+F2` |
| Close tab | *(tab-bar ✕ / menu)* | *(tab context menu / menu)* |
| Close pane | `Ctrl+Shift+Q` | `⌘⇧W` |
| Close session | menu, then confirm | sidebar/menu, then confirm |
| Scroll history | `Shift+Page Up/Down` | `Shift+Page Up/Down` |
| Toggle sidebar | `F9` | `F9` |
| Toggle input lock | `Ctrl+Shift+L` | `⌘⇧L` |
| Reset renderer | `Ctrl+Shift+F5` | `⌘⇧F5` |

## Versioning

- `PROTOCOL_VERSION = 21` — the tab + layout messages (`PaneSplit`, `PaneSwap`,
  `SetLayoutRatios`, `SetFocus`, `Tab*`, `ApplyLayoutScheme`, `LayoutUpdate`).
- `STATE_VERSION = 3` — daemon checkpoint persistence; the v2→v3 migration wraps
  each persisted session's panes in a default one-tab-one-pane layout.
- `KMUX_FFI_ABI_VERSION` — gates the Swift tiling surface (`tabs`/`layout`/per-pane
  grid/`focus_pane`/`set_pane_sizes` + the tiling/scheme/zoom `FfiAction`s), plus
  the interactive-divider surface (`dividers`, `apply_divider_drag`,
  `reset_divider`), `FfiAction::FocusPaneAt`, `rename_tab`, and the session-close
  confirmation mode. Defined once in `crates/kmux-ffi/src/lib.rs`; bumped whenever
  this surface changes (no value is hardcoded on the Swift side — uniffi's
  binding-checksum check guards drift).
