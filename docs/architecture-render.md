# Shared GPU terminal rendering (`kmux-render`)

This document describes `kmux-render`, the single cross-platform,
GPU-accelerated terminal cell-grid renderer (issue #132). It replaces the two
per-frontend CPU rasterizers — `kmux-gtk`'s Cairo/Pango `render.rs` and the
Swift app's CoreText `TerminalView` — with one wgpu implementation both
frontends drive, so the cell-grid render leaf lives in one place.

It is **opt-in** today: each frontend keeps its CPU renderer as the default and
selects the GPU path with `KMUX_RENDERER=wgpu`. Flipping the default and removing
the CPU renderers are deliberate follow-ups (see [Status](#status)).

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
`kmux-ffi`), off by default.

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

- **GTK**: build with the feature and set the env —
  `KMUX_RENDERER=wgpu cargo run -p kmux-gtk --features gpu`. Without the env it
  stays on Cairo; if no GPU adapter is available it logs and falls back to Cairo.
- **Swift**: `mise run swift-gpu-run` (builds the FFI with `--features gpu`,
  regenerates bindings that include `KmuxRenderer`, builds the app with
  `-DKMUX_GPU`, and runs with `KMUX_RENDERER=wgpu`). The default `mise run
  swift-run` stays CoreText.

## Testing

Two tiers:

- **Pure-function** (no GPU, always in CI): geometry, packed format, metrics,
  atlas packing/growth, color, and the `Grid`-vs-`Packed` geometry parity test.
- **GPU smoke** (gated): build a `wgpu::Instance`, request an adapter allowing a
  software/fallback one, render one offscreen frame and assert known pixels
  (`cargo test -p kmux-render --features gpu`). These skip cleanly when no
  adapter is present, so headless CI without a GPU still passes the pure tier.

## Status

Landed (opt-in): the `kmux-render` crate (core + text + gpu, GPU smoke-tested),
the GTK offscreen path, the `kmux-ffi` `KmuxRenderer` object, and the Swift Metal
path. Follow-ups: HiDPI-crisp GTK rendering (render at physical resolution),
making the renderer the resize authority on the GTK path too (so tiled layouts
match the Cairo path exactly), Linux `dmabuf` zero-copy, color-emoji glyphs, and
— once proven on both platforms — flipping the default and removing the CPU
renderers.

[`CellGrid`]: ../crates/kmux-client/src/grid/mod.rs
[`Theme`]: ../crates/kmux-app/src/theme.rs
[`Appearance`]: ../crates/kmux-app/src/appearance.rs
[`Frame`]: ../crates/kmux-render/src/frame.rs
