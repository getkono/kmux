//! GPU render path for the GTK frontend (opt-in via `renderer = "gpu"` in
//! `config.toml`).
//!
//! The shared [`kmux_render`] crate renders the active tab's panes into an
//! offscreen RGBA texture; we read the pixels back, swizzle RGBA→BGRA, and paint
//! them into the `DrawingArea`'s cairo context as an `ImageSurface`. This keeps
//! GTK's compositor happy with a single low-risk presentation path on both Linux
//! and macOS (a Linux zero-copy dmabuf fast path is a future optimization). The
//! Cairo renderer in [`super::render`] stays the default; this path is selected
//! only when the config requests it and a GPU adapter is available.
//!
//! Note: this path uses the renderer's own (swash-derived) cell metrics, while
//! the resize path still uses the Pango metrics, so tiled layouts may differ by
//! a pixel from the Cairo path until the renderer becomes the resize authority
//! too (follow-up). The common single-pane case is unaffected.

use gtk4::cairo;

use kmux_app::appearance::Appearance;
use kmux_app::core::AppCore;
use kmux_app::layout::{LayoutConfig, resolve_layout};
use kmux_app::theme::Theme;
use kmux_protocol::{format_pane_id, pane_index};
use kmux_render::{
    CellSource, CursorView, Frame, PaneView, SceneCounts, ScrollIndicator, TerminalRenderer,
    build_scene, cursor_geometry,
};

use kmux_app::config::RendererKind;

use super::render::GUTTER;

fn cfg() -> LayoutConfig {
    LayoutConfig {
        gutter_cols: GUTTER,
        gutter_rows: GUTTER,
        min_cols: 1,
        min_rows: 1,
    }
}

/// Holds the offscreen GPU renderer when the wgpu backend is selected and
/// available; otherwise inert (the Cairo path is used).
pub(crate) struct GpuState {
    renderer: Option<TerminalRenderer>,
    /// Scene primitive counts from the last painted frame, stashed for the
    /// render-debug overlay (only while it is visible). See [`paint`].
    last_counts: Option<SceneCounts>,
}

impl GpuState {
    /// Build the GPU state for the configured `renderer` backend. Falls back to
    /// Cairo (an inert state) when `renderer` is [`RendererKind::Cairo`] or when
    /// GPU init fails.
    pub(crate) fn new(renderer: RendererKind, appearance: &Appearance, theme: &Theme) -> Self {
        if renderer != RendererKind::Gpu {
            return Self {
                renderer: None,
                last_counts: None,
            };
        }
        match TerminalRenderer::new_offscreen(1, 1, 1.0, appearance, theme) {
            Ok(renderer) => {
                tracing::info!("kmux-render: GPU renderer active (renderer = \"gpu\")");
                Self {
                    renderer: Some(renderer),
                    last_counts: None,
                }
            }
            Err(e) => {
                tracing::warn!(
                    "renderer = \"gpu\" requested but GPU init failed: {e}; using Cairo"
                );
                Self {
                    renderer: None,
                    last_counts: None,
                }
            }
        }
    }

    /// Whether the GPU path should be used for drawing.
    pub(crate) fn enabled(&self) -> bool {
        self.renderer.is_some()
    }

    /// The renderer actually in use this frame — the *effective* backend, which
    /// can differ from the configured one (GPU init failure falls back to Cairo).
    /// Surfaced in the render-debug overlay so it reflects reality, not config.
    pub(crate) fn active_renderer_name(&self) -> &'static str {
        if self.renderer.is_some() {
            "wgpu"
        } else {
            "cairo"
        }
    }

    /// The scene primitive counts from the last painted frame, when the
    /// render-debug overlay is active (the renderer stashes them during [`paint`]).
    pub(crate) fn last_scene_counts(&self) -> Option<SceneCounts> {
        self.last_counts
    }
}

/// One pane's placement + grid + focus, gathered before borrowing for the frame.
struct PaneEntry<'a> {
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
    focused: bool,
    grid: &'a kmux_client::grid::CellGrid,
}

/// Render the active tab via the GPU and paint the result into `cr`. A no-op when
/// the GPU renderer is not active.
pub(crate) fn paint(
    state: &mut GpuState,
    core: &AppCore,
    cr: &cairo::Context,
    width: i32,
    height: i32,
    blink_on: bool,
) {
    let Some(renderer) = state.renderer.as_mut() else {
        return;
    };
    if width <= 0 || height <= 0 {
        return;
    }

    // Gather the panes to draw (tiled layout, or the single active grid).
    let mut entries: Vec<PaneEntry<'_>> = Vec::new();
    let mut multi = false;
    if let Some(layout) = core.mgr.render_layout() {
        let (cols, rows) = renderer.cols_rows(width, height);
        let rects = resolve_layout(&layout, cols, rows, &cfg());
        multi = rects.len() > 1;
        let focused = core.mgr.active_pane_id().and_then(pane_index);
        let word = core.mgr.active_session().unwrap_or("").to_string();
        for r in &rects {
            let pane_id = format_pane_id(&word, r.pane_index);
            if let Some(grid) = core.mgr.buffer(&pane_id) {
                entries.push(PaneEntry {
                    col: r.col,
                    row: r.row,
                    cols: r.cols,
                    rows: r.rows,
                    focused: Some(r.pane_index) == focused,
                    grid,
                });
            }
        }
    } else if let Some(grid) = core.mgr.active_grid() {
        entries.push(PaneEntry {
            col: 0,
            row: 0,
            cols: grid.cols as u16,
            rows: grid.rows as u16,
            focused: true,
            grid,
        });
    }

    // Selection spans are owned per pane; collect first so the frame can borrow.
    let spans: Vec<Vec<(u16, u16, u16)>> = entries
        .iter()
        .map(|e| e.grid.visible_selection_spans())
        .collect();

    let panes: Vec<PaneView<'_>> = entries
        .iter()
        .zip(spans.iter())
        .map(|(e, sel)| {
            let scrolled = e.grid.scroll_offset() > 0;
            PaneView {
                col: e.col,
                row: e.row,
                cols: e.cols,
                rows: e.rows,
                focused: e.focused,
                cells: CellSource::Grid(e.grid),
                // The cursor only shows against the live screen, not while scrolled.
                cursor: (!scrolled).then(|| CursorView::from_state(e.grid.cursor())),
                selection: sel,
                scroll: scrolled.then(|| ScrollIndicator {
                    offset: e.grid.scroll_offset(),
                    total: e.grid.total_scrollback_display_rows(),
                }),
            }
        })
        .collect();

    let frame = Frame {
        width: width as u32,
        height: height as u32,
        scale: 1.0,
        palette: &core.palette,
        blink_on,
        panes,
        multi,
    };

    // Render-debug instrumentation (issue: cursor render debugging). Both paths
    // are gated so they cost nothing when the overlay is hidden and trace logging
    // is off — the overlay stashes scene counts; the trace logs the cursor's
    // pixel rect as kmux-render computes it (compare against the drawn pixels).
    if core.render_debug_visible {
        state.last_counts = Some(build_scene(&frame, renderer.metrics().cell()).counts());
    }
    if tracing::enabled!(target: "kmux::render_debug", tracing::Level::TRACE) {
        let m = renderer.metrics().cell();
        for pane in &frame.panes {
            if let Some(cv) = pane.cursor {
                let (cols, rows) = pane.cells.dims();
                let origin = (pane.col as f32 * m.cell_w, pane.row as f32 * m.cell_h);
                let geo = cursor_geometry(&cv, origin, cols, rows, m);
                tracing::trace!(
                    target: "kmux::render_debug",
                    focused = pane.focused,
                    col = cv.col,
                    row = cv.row,
                    shape = ?cv.shape,
                    blink_on = frame.blink_on,
                    in_range = geo.in_range,
                    rects = geo.rects.len(),
                    rect0 = ?geo.rects.first(),
                    "gpu cursor geometry (kmux-render)"
                );
            }
        }
    }

    if let Err(e) = renderer.render(&frame) {
        tracing::warn!("kmux-render GPU frame failed: {e}");
        return;
    }
    let pixels = match renderer.read_pixels() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("kmux-render read-back failed: {e}");
            return;
        }
    };

    // RGBA8 → cairo ARGB32 (little-endian BGRA; content is opaque so the
    // premultiply is a no-op and we only swap R/B).
    let mut bgra = pixels.rgba;
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let stride = width * 4;
    match cairo::ImageSurface::create_for_data(
        bgra,
        cairo::Format::ARgb32,
        pixels.width as i32,
        pixels.height as i32,
        stride,
    ) {
        Ok(surface) => {
            if cr.set_source_surface(&surface, 0.0, 0.0).is_ok() {
                let _ = cr.paint();
            }
        }
        Err(e) => tracing::warn!("kmux-render cairo surface failed: {e}"),
    }

    // NOTE: the OSC 9;4 per-pane progress bar (issue #125) is currently drawn
    // only on the Cairo path (`render::render_tiled`), the runtime default.
    // Surfacing it through the GPU path means threading progress into the shared
    // `kmux-render` scene — tracked as a follow-up; `renderer = "gpu"` shows no
    // bar until then.
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_app::theme;

    #[test]
    fn cairo_renderer_yields_an_inert_disabled_state() {
        // RendererKind::Cairo must never init the GPU path — this stays headless
        // (no adapter) and reports the effective renderer as cairo.
        let appearance = Appearance::default();
        let theme = theme::default_theme();
        let state = GpuState::new(RendererKind::Cairo, &appearance, &theme);
        assert!(!state.enabled(), "Cairo must not enable the GPU path");
        assert_eq!(state.active_renderer_name(), "cairo");
    }
}
