//! GPU render path for the GTK frontend (opt-in via `KMUX_RENDERER=wgpu`).
//!
//! The shared [`kmux_render`] crate renders the active tab's panes into an
//! offscreen RGBA texture; we read the pixels back, swizzle RGBA→BGRA, and paint
//! them into the `DrawingArea`'s cairo context as an `ImageSurface`. This keeps
//! GTK's compositor happy with a single low-risk presentation path on both Linux
//! and macOS (a Linux zero-copy dmabuf fast path is a future optimization). The
//! Cairo renderer in [`super::render`] stays the default; this path is selected
//! only when the env var is set and a GPU adapter is available.
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
use kmux_render::{CellSource, CursorView, Frame, PaneView, ScrollIndicator, TerminalRenderer};

use super::render::GUTTER;

/// Whether `KMUX_RENDERER` selects the wgpu backend. Pure so it is testable.
fn renderer_pref(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.eq_ignore_ascii_case("wgpu"))
}

fn wants_gpu() -> bool {
    renderer_pref(std::env::var("KMUX_RENDERER").ok().as_deref())
}

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
}

impl GpuState {
    /// Build the GPU state, honoring `KMUX_RENDERER`. Falls back to Cairo (an
    /// inert state) when the env var is unset or GPU init fails.
    pub(crate) fn new(appearance: &Appearance, theme: &Theme) -> Self {
        if !wants_gpu() {
            return Self { renderer: None };
        }
        match TerminalRenderer::new_offscreen(1, 1, 1.0, appearance, theme) {
            Ok(renderer) => {
                tracing::info!("kmux-render: GPU renderer active (KMUX_RENDERER=wgpu)");
                Self {
                    renderer: Some(renderer),
                }
            }
            Err(e) => {
                tracing::warn!("KMUX_RENDERER=wgpu set but GPU init failed: {e}; using Cairo");
                Self { renderer: None }
            }
        }
    }

    /// Whether the GPU path should be used for drawing.
    pub(crate) fn enabled(&self) -> bool {
        self.renderer.is_some()
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
        let focused = core
            .mgr
            .active_pane_id()
            .and_then(|p| p.rsplit_once('/'))
            .and_then(|(_, i)| i.parse::<u32>().ok());
        let word = core.mgr.active_session().unwrap_or("").to_string();
        for r in &rects {
            let pane_id = format!("{word}/{}", r.pane_index);
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
}

#[cfg(test)]
mod tests {
    use super::renderer_pref;

    #[test]
    fn renderer_pref_selects_wgpu_case_insensitively() {
        assert!(renderer_pref(Some("wgpu")));
        assert!(renderer_pref(Some("WGPU")));
        assert!(!renderer_pref(Some("cairo")));
        assert!(!renderer_pref(Some("")));
        assert!(!renderer_pref(None));
    }
}
