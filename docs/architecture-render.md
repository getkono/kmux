# Shared GPU terminal rendering (`kmux-render`)

This document describes `kmux-render`, the single cross-platform,
GPU-accelerated terminal cell-grid renderer (issue #132). It replaces the two
per-frontend CPU rasterizers — `kmux-gtk`'s Cairo/Pango `render.rs` and the
Swift app's CoreText `TerminalView` — with one wgpu implementation both
frontends drive, so the cell-grid render leaf lives in one place.

The GPU renderer is **compiled in by default** (the `gpu` feature is on, issue
#132), so the standard build/test paths always cover it. The only opt-in is at
**runtime**: each frontend keeps its CPU renderer as the startup default and
switches to the GPU path when `renderer = "gpu"` is set in `config.toml`. A lean, wgpu-free
build is still available with `--no-default-features` (CI compiles that path too,
via `mise run build-no-gpu`). Removing the CPU renderers outright is a deliberate
follow-up (see [Status](#status)).

## Where it sits

```
kmux-protocol → kmux-client → kmux-app
                                 ├──────────────┬──────────────
                                 ▼              ▼
                             kmux-gtk        kmux-ffi
                                 └──────┬───────┘
                                        ▼
                                  kmux-render   (consumes protocol/client/app types)
```

`kmux-render` depends *downward* on `kmux-protocol`/`kmux-client`/`kmux-app` for
the read-only types it draws ([`CellGrid`], [`Theme`], [`Appearance`]) and is
consumed by `kmux-gtk` (Rust→Rust) and `kmux-ffi` (on behalf of the Swift app).
It is **not** a dependency of anything at or below `kmux-app`, so the hard rule
"nothing at or below `kmux-app` may depend on a UI toolkit" holds — and
`kmux-render` itself depends on no UI toolkit (wgpu's Metal/Vulkan/GL backends
are GPU driver APIs, the same category as `kmux-ghostty-sys` linking libghostty).

## Feature tiers

The crate is layered so the lean builds pull no wgpu:

- **core** (always compiled): `frame`, `packed`, `geometry`, `color`. Pure Rust,
  no heavy deps. `kmux-ffi` uses `packed` for the FFI cell format without wgpu.
- **`text`**: `metrics`, `atlas` — font measurement + glyph rasterization via
  `swash`/`etagere`/`fontdb`. CPU-only and headlessly testable.
- **`gpu`** (implies `text`): `renderer`, `pipeline` — the wgpu device, WGSL
  pipelines, and surfaces. Pulls `wgpu`, `raw-window-handle`, `bytemuck`,
  `pollster`.

Frontends enable `kmux-render/gpu` through their own `gpu` feature (`kmux-gtk`,
`kmux-ffi`), **on by default**; `--no-default-features` drops back to the lean,
wgpu-free path.

## The render input: `Frame`

The renderer is fed a toolkit-neutral [`Frame`] the caller assembles from
already-resolved state — pane tile rects come from the shared `kmux_app::layout`
resolver; cursor/selection/scroll from the grid (or the FFI getters). The
renderer never touches `AppCore`, the session manager, or layout resolution, so
both frontends build identical frames.

Cells reach it two ways, via `CellSource`:

- `Grid(&CellGrid)` — GTK borrows the client grid directly (zero copy).
- `Packed { cells, cols, rows }` — the Swift path reuses the existing 16-byte
  packed FFI buffer (zero re-pack).

Both describe the same displayed grid (scrollback already composited into the
top rows when scrolled). The single definition of "which cell shows at (row,
col)" is `geometry::for_each_displayed_cell`; `packed::encode_cells` and the
`Grid` render path both go through it, and a parity test asserts the two sources
produce identical geometry.

## How a frame is drawn

`geometry::build_scene` turns a `Frame` into draw primitives in the CPU
renderers' order: cell backgrounds → glyphs (dim → reduced alpha) → overlays
(underline/strikethrough rules, selection wash, the four cursor shapes, focus
border, scroll indicator) → overlay glyphs (the block cursor's glyph in
`cursor_fg`, scroll text). The renderer uploads these as two instanced passes:

- **solid quads** (`bg_quad.wgsl`) — flat-color rectangles.
- **glyph quads** (`glyph_quad.wgsl`) — textured quads sampling a glyph atlas
  page and tinting by the cell color.

Glyphs are rasterized once per `(face, char)` by `swash`, shelf-packed into RGBA
atlas pages by `etagere` (white + coverage-in-alpha for monochrome glyphs, so
the shader tints them), and uploaded incrementally. The target is a non-sRGB
(UNORM) surface with straight (non-premultiplied) alpha blending, so output
matches the CPU renderers' display-space compositing with no gamma surprises.

`RenderMetrics` measures the monospace cell from the [`Appearance`] via
`swash`/`fontdb` and is the single **cols/rows authority** — both frontends route
their resize geometry through it (`cols_rows`) on the GPU path, replacing
per-toolkit Pango/CoreText measurement.

## Two presentation targets

The renderer supports both:

- **Offscreen** (`new_offscreen`) — renders to an internal RGBA texture;
  `read_pixels` returns tightly-packed RGBA8. Used by **GTK** on every platform:
  the result is blitted into the `DrawingArea`'s cairo context as an
  `ImageSurface` (RGBA→ARGB32 swizzle). This GPU→CPU→GPU round-trip is the
  low-risk, cross-platform baseline; a Linux zero-copy `dmabuf` fast path is a
  follow-up.
- **Surface** (`new_for_metal_layer`) — presents directly to a macOS
  `CAMetalLayer` (built via wgpu `create_surface_unsafe`), no readback. Used by
  the **Swift** app: `TerminalNSView` swaps its backing layer to a `CAMetalLayer`
  and routes `needsDisplay` → `updateLayer()` → the FFI `KmuxRenderer`, reusing
  all of its existing input/sizing/pump wiring.

## The FFI seam

`kmux-ffi` (behind its `gpu` feature) exposes a `KmuxRenderer` uniffi object
wrapping `TerminalRenderer`: `new_metal(driver, layer_ptr, …)`, `render(driver,
…)`, `resize`, `refresh_appearance`, `api_version`. `render` locks the driver,
assembles the active tab's `Frame` (`CellSource::Grid`, mirroring the GTK path),
and presents to the layer.

## Versioning

Per the repo's correctness rule, the renderer boundary is versioned.
`kmux_render::KMUX_RENDER_API_VERSION` is bumped on any breaking change to the
public API or the wire-packed cell layout. `kmux-ffi` pins `EXPECTED_RENDER_API`
and a **compile-time** `const` assert guarantees the linked `kmux-render`
matches; `kmux_ffi_render_api_version()` exposes it across the C ABI.
`KMUX_FFI_ABI_VERSION` was bumped 10 → 11 for the additive `KmuxRenderer`
surface (uniffi's binding-checksum check guards the rest).

## Selecting the GPU path

The `gpu` feature is on by default, so the renderer is already compiled in; these
just flip the **runtime** switch via the `renderer` key in `config.toml`
(`renderer = "gpu"`; default `"cairo"`). It is a config key — not a CLI flag —
because a kmux GUI client is a singleton process, so a flag on a second launch
would never reach the running renderer. The render-debug overlay
(GTK `win.toggle-render-debug`, Swift ⌘⇧G) shows the **effective** renderer,
which falls back to the CPU path if GPU init fails.

- **GTK**: `renderer = "gpu"` then `./kmux` (Linux). Without it GTK stays on
  Cairo; if no GPU adapter is available it logs and falls back to Cairo.
- **Swift**: `renderer = "gpu"` then `./kmux` (macOS). The dev build always
  compiles the Swift Metal view (`./kmux` passes `-DKMUX_GPU`), so the config key
  alone flips between Metal and CoreText at runtime — no separate build. Without
  it, or if Metal init fails, the app stays on CoreText.

Both clients write logs to the client log file (`mise run tail-client-log`); the
GPU path logs adapter/surface setup, resizes, and frame errors there (raise
verbosity with `RUST_LOG=kmux=debug` or `=trace`). The daemon logs separately to
the daemon log file.

## Render debugging (cursor geometry et al.)

When the terminal renders something wrong — the original motivating case was the
**cursor** — there are two ways to see what the renderer is actually handed,
without a debugger.

**Render-debug overlay** — a live OSD (top-leading, opposite the perf HUD) that
shows, for the focused pane: the active renderer leaf (`cairo`/`coretext`/`wgpu`),
the frame/grid/cell geometry, the cursor's *logical* state (col, row, shape,
blink, visible, `is_drawn`), and the exact pixel rect
[`cursor_geometry`](../crates/kmux-render/src/geometry.rs) computes for it. On the
GPU path it also shows the scene primitive counts. Toggle it:

- **GTK**: `Ctrl+Shift+D` (hamburger menu → *Render Debug*).
- **Swift**: `⌘⇧G` (Session menu → *Render Debug*).

Compare the overlay's `px:` line against what you see drawn. The overlay's rect
comes from `cursor_geometry`, which shares its per-shape rect math
(`cursor_shape_rects`) with the renderer's `emit_cursor`, so it provably matches
the GPU path. The **CPU** paths rasterize the cursor directly with their *own*
constants — GTK Cairo and Swift CoreText both hardcode a **2px** bar/underline,
while the renderer uses a scale-aware `cursor_thickness` (`(cell_h*0.1).max(1)`).
That divergence is exactly the kind of bug the overlay surfaces.

**Structured traces** — under the `kmux::render_debug` target:

- `RUST_LOG="kmux::render_debug=trace"` — per-frame cursor geometry. The GPU path
  logs `cursor_geometry`'s rect; the GTK Cairo path logs its hardcoded constants
  *next to* the renderer's `cursor_thickness` (one line, side by side) — diffing
  the two pinpoints a cursor mismatch. Also emits a one-shot line on renderer reset.
- `RUST_LOG=kmux_render=trace` — the renderer's own render/resize/atlas-rebuild
  lines (adapter, surface, frame skips).

**Renderer reset** — a diagnostic that rebuilds the renderer + glyph atlas (and
re-measures cell metrics) and forces a full repaint, to clear any corrupt cached
state. GTK: `Ctrl+Shift+F5` (menu → *Reset Renderer*). Swift: Session menu →
*Reset Renderer*. On the CPU paths (no persistent atlas) it degrades to a full
re-pack + repaint. It routes through `Action::ResetRenderer` →
`FrontendEffect::ResetRenderer`, because the renderer object is frontend-owned, so
only the frontend can drop and recreate it.

The toolkit-agnostic half lives in `kmux-app`
([`core::render_debug`](../crates/kmux-app/src/core/render_debug.rs)): it assembles
the *logical* snapshot only (no kmux-render types — `kmux-render` depends on
`kmux-app`, so the reverse would be a cargo cycle). Each frontend turns the logical
cursor into pixel rects via `kmux_render::cursor_geometry` (GTK directly; Swift via
the `kmux-ffi` `render_debug` getter), so the rects reflect that frontend's *own*
cell metrics.

## Diagnostic test patterns (`kmux diagnostic`)

Where the render-debug overlay shows what the renderer was *handed*, the diagnostic
suite (issue #145) feeds it a *known input* and lets you eyeball the output:

```
kmux diagnostic            # list the patterns
kmux diagnostic <test>     # open the GUI with a session painting <test>
kmux diagnostic <test> --emit   # write the pattern to the host terminal instead
```

Patterns ([`kmux_app::diagnostic`](../crates/kmux-app/src/diagnostic/)): `glyphs`
(ASCII + Unicode glyph grid — the original "glyphs not rendered" repro), `attrs`
(bold/italic/underline/… across the four faces, exercising the atlas `FaceStyle`
keys), `colors` (16 / 256 / truecolor ramps), `unicode` (wide CJK, emoji, combining
marks), `boxes` (box-drawing alignment grid), `progress` (animated OSC 9;4
progress-bar states — issue #125; see below), and `all`. `progress` is excluded
from `all` because it paints *window chrome* and loops over time rather than
emitting a one-shot grid; its emitter sweeps the states on a timer (step overridable
via `KMUX_DIAG_PROGRESS_STEP_MS`) until the pane closes.

`kmux diagnostic <test>` is an **interactive** launch: it opens the GUI with a
*fresh, dedicated* session whose program is the emitter — the `kmux` binary itself
run as `kmux diagnostic <test> --emit`, which writes
[`pattern_bytes`](../crates/kmux-app/src/diagnostic/patterns.rs) (the single source
of truth) and then blocks on stdin so the pane stays up. Both frontends resolve the
same launch command via `diagnostic::session_command`: GTK threads it through the
`Plan`'s `initial_program`; Swift forwards the test name through
`DriverConfig.diagnostic` (FFI ABI 15) and `build_core` resolves it the same way.
`AppCore::auto_select_session` opens a new session for it instead of attaching to an
existing one.

Scope is the **local daemon**: the emitter is *this* host's `kmux` binary (located
via `KMUX_BIN` → next-to-the-executable → `PATH`), which is also the daemon host. A
remote daemon would need `kmux` installed there.

## OSC 9;4 progress bar (issue #125)

The ConEmu / Windows-Terminal progress report (`OSC 9 ; 4 ; state ; pct`) renders
as a thin **per-pane** bar, mirroring Ghostty's window bar but per tile (kmux tabs
can tile several panes). The end-to-end path reuses the OSC 0/2 title pipeline:

```
libghostty-vt parses 9;4 → kmux Zig wrapper Handler.vt(.progress_report)  (C ABI v4)
  → kmux-ghostty EventSink::on_progress(ProgressReport)
  → kmuxd BackendEventSink::on_progress(PaneProgressState, Option<u8>)
  → PaneEventSink: dedup + store in PaneRelay.progress + broadcast PaneProgressChanged
                   (+ PaneInfo snapshot carries it, so late clients see the bar)
  → client SessionManager updates cached PaneInfo
  → frontend reads PaneInfo on the render tick and paints the bar
```

Rendering lives in the **Cairo** path ([`render::render_tiled`](../crates/kmux-gtk/src/imp/render.rs))
and the **Swift** CoreText path ([`TerminalView.draw`](../kmux-swift/Sources/KmuxApp/TerminalView.swift)):
a ~3px bar along each pane's bottom edge, width = `progress/100` of the tile (full
width for `Indeterminate`), looked up via `SessionManager::pane_info` (GTK) /
`FfiPaneRect.progress_state`/`progress` (Swift, FFI ABI 16). State→colour: `Set`→accent,
`Error`→red, `Pause`→orange, `Indeterminate`→accent, `Remove`→no bar. A
`PaneProgressChanged` event marks the frame dirty so the bar updates live without a
keystroke. The **GPU path** (`renderer = "gpu"`) does not yet draw the bar —
surfacing it through the shared `kmux-render` scene is a follow-up.

## Symbol glyph fallback

The configured monospace font usually lacks Powerline separators (U+E0B0–) and
Nerd Font Private Use Area icons, which would otherwise render as tofu/blank
(issue #145). A single bundled font — `crates/kmux-render/assets/SymbolsNerdFontMono-Regular.ttf`
(Symbols Nerd Font Mono, exposed as `kmux_render::symbol_fallback_bytes()` over
the FFI) — supplies them as a per-glyph fallback. Each of the three render paths
wires it in differently, but with the same effect: a glyph the primary face
lacks is drawn from the symbol font instead.

- **GPU atlas** (`kmux-render`): explicit fallback in `atlas.rs` — if the primary
  face doesn't map the char, try `symbol_fallback()` before rendering blank.
- **GTK Cairo/Pango**: the font is registered with fontconfig
  (`FcConfigAppFontAddFile`, `kmux-gtk/src/imp.rs`); Pango's automatic fallback
  search then resolves missing glyphs from it.
- **Swift CoreText**: registering the font alone is **not** enough — that only
  makes it discoverable by name and does not add it to any cascade list. The
  terminal faces in `TerminalMetrics` install it via `kCTFontCascadeListAttribute`
  (prepended to the default cascade list), so `NSAttributedString.draw()`
  substitutes it for missing glyphs while keeping the system's emoji/CJK
  fallbacks.

Verify visually with `kmux diagnostic glyphs` on each path; the diagnostic
asserts nothing about pixels.

## Testing

Two tiers:

- **Pure-function** (no GPU, always in CI): geometry, packed format, metrics,
  atlas packing/growth, color, and the `Grid`-vs-`Packed` geometry parity test.
- **GPU smoke** (gated): build a `wgpu::Instance`, request an adapter allowing a
  software/fallback one, render one offscreen frame and assert known pixels
  (`cargo test -p kmux-render --features gpu`). These skip cleanly when no
  adapter is present, so headless CI without a GPU still passes the pure tier.

## Status

Landed: the `kmux-render` crate (core + text + gpu, GPU smoke-tested), the GTK
offscreen path, the `kmux-ffi` `KmuxRenderer` object, and the Swift Metal path.
The `gpu` feature is now **on by default** (compiled + tested everywhere; CI also
compiles the wgpu-free path), so only the runtime switch stays opt-in. Follow-ups:
HiDPI-crisp GTK rendering (render at physical resolution), making the renderer the
resize authority on the GTK path too (so tiled layouts match the Cairo path
exactly), Linux `dmabuf` zero-copy, color-emoji glyphs, and — once proven on both
platforms — making the GPU renderer the runtime default and removing the CPU
renderers.

[`CellGrid`]: ../crates/kmux-client/src/grid/mod.rs
[`Theme`]: ../crates/kmux-app/src/theme.rs
[`Appearance`]: ../crates/kmux-app/src/appearance.rs
[`Frame`]: ../crates/kmux-render/src/frame.rs
