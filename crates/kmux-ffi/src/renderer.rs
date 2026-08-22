//! The exported GPU renderer object (issue #132), behind the `gpu` feature.

use super::*;

/// GPU terminal renderer presenting to a macOS `CAMetalLayer` (issue #132).
///
/// An opaque, thread-confined wrapper over [`kmux_render::TerminalRenderer`]
/// (all calls on the Swift main thread). It reads the active tab's grids +
/// layout from a [`KmuxDriver`] and presents directly to the layer — no
/// readback. Built only with the `gpu` feature; the default staticlib omits it.
#[cfg(feature = "gpu")]
#[derive(uniffi::Object)]
pub struct KmuxRenderer {
    inner: Mutex<TerminalRenderer>,
}

#[cfg(feature = "gpu")]
#[uniffi::export]
impl KmuxRenderer {
    /// Build a renderer bound to a `CAMetalLayer` pointer, using the driver's
    /// current appearance + palette. `width`/`height` are physical px.
    ///
    /// The Swift view owns the layer and must keep it alive for the renderer's
    /// lifetime (it drops the renderer before tearing the view down).
    #[uniffi::constructor]
    pub fn new_metal(
        driver: &KmuxDriver,
        layer_ptr: u64,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Result<Arc<Self>, FfiError> {
        tracing::debug!(
            layer_ptr,
            width,
            height,
            scale,
            "KmuxRenderer::new_metal: creating Metal renderer"
        );
        let d = driver.inner.lock().expect("driver mutex poisoned");
        // SAFETY: the Swift view guarantees the layer outlives this renderer.
        let renderer = unsafe {
            TerminalRenderer::new_for_metal_layer(
                layer_ptr,
                width,
                height,
                scale,
                &d.appearance,
                &d.palette,
            )
        }
        .map_err(|e| {
            tracing::error!(width, height, scale, error = %e, "KmuxRenderer::new_metal: GPU init failed");
            FfiError::Render {
                message: e.to_string(),
            }
        })?;
        drop(d);
        Ok(Arc::new(Self {
            inner: Mutex::new(renderer),
        }))
    }

    /// Resize the swapchain to `width × height` physical px at `scale`.
    pub fn resize(&self, width: u32, height: u32, scale: f32) {
        tracing::debug!(width, height, scale, "KmuxRenderer::resize");
        self.inner
            .lock()
            .expect("renderer mutex poisoned")
            .resize(width, height, scale);
    }

    /// Re-read the font appearance from the driver (after a font change).
    pub fn refresh_appearance(&self, driver: &KmuxDriver) {
        tracing::debug!("KmuxRenderer::refresh_appearance");
        let appearance = driver
            .inner
            .lock()
            .expect("driver mutex poisoned")
            .appearance
            .clone();
        self.inner
            .lock()
            .expect("renderer mutex poisoned")
            .set_appearance(&appearance);
    }

    /// Render the active tab and present. `width`/`height` are physical px.
    pub fn render(&self, driver: &KmuxDriver, width: u32, height: u32, scale: f32) {
        tracing::trace!(width, height, scale, "KmuxRenderer::render");
        let mut renderer = self.inner.lock().expect("renderer mutex poisoned");
        let d = driver.inner.lock().expect("driver mutex poisoned");
        render_active_tab(&mut renderer, &d, width, height, scale);
    }

    /// The linked kmux-render API version.
    pub fn api_version(&self) -> u32 {
        kmux_render::KMUX_RENDER_API_VERSION
    }
}

/// Assemble the active tab's frame from the driver and render it. Mirrors the
/// GTK `render_gpu::paint` frame assembly — both read the shared layout + grids
/// and build an identical [`Frame`] with `CellSource::Grid`.
#[cfg(feature = "gpu")]
fn render_active_tab(
    renderer: &mut TerminalRenderer,
    d: &FrontendDriver,
    width: u32,
    height: u32,
    scale: f32,
) {
    if width == 0 || height == 0 {
        return;
    }
    type Entry<'a> = (u16, u16, u16, u16, bool, &'a kmux_client::grid::CellGrid);
    let mut entries: Vec<Entry<'_>> = Vec::new();
    let mut multi = false;
    if let Some(layout) = d.mgr.render_layout() {
        let (cols, rows) = renderer.cols_rows(width as i32, height as i32);
        let rects = kmux_app::layout::resolve_layout(
            &layout,
            cols,
            rows,
            &kmux_app::layout::LayoutConfig::default(),
        );
        multi = rects.len() > 1;
        let focused = d.mgr.active_pane_id().and_then(pane_index);
        let word = d.mgr.active_session().unwrap_or("").to_string();
        for r in &rects {
            let pane_id = format_pane_id(&word, r.pane_index);
            if let Some(grid) = d.mgr.buffer(&pane_id) {
                entries.push((
                    r.col,
                    r.row,
                    r.cols,
                    r.rows,
                    Some(r.pane_index) == focused,
                    grid,
                ));
            }
        }
    } else if let Some(grid) = d.active_grid() {
        entries.push((0, 0, grid.cols as u16, grid.rows as u16, true, grid));
    }

    let spans: Vec<Vec<(u16, u16, u16)>> = entries
        .iter()
        .map(|e| e.5.visible_selection_spans())
        .collect();
    let blink_on = d.blink_on();
    let panes: Vec<PaneView<'_>> = entries
        .iter()
        .zip(spans.iter())
        .map(|(e, sel)| {
            let scrolled = e.5.scroll_offset() > 0;
            PaneView {
                col: e.0,
                row: e.1,
                cols: e.2,
                rows: e.3,
                focused: e.4,
                cells: CellSource::Grid(e.5),
                cursor: (!scrolled).then(|| CursorView::from_state(e.5.cursor())),
                selection: sel,
                scroll: scrolled.then(|| ScrollIndicator {
                    offset: e.5.scroll_offset(),
                    total: e.5.total_scrollback_display_rows(),
                }),
            }
        })
        .collect();

    tracing::trace!(
        panes = panes.len(),
        multi,
        blink_on,
        "kmux-ffi: assembled GPU frame"
    );
    let frame = Frame {
        width,
        height,
        scale,
        palette: &d.palette,
        blink_on,
        panes,
        multi,
    };
    if let Err(e) = renderer.render(&frame) {
        tracing::warn!("kmux-render: metal frame failed: {e}");
    }
}

#[cfg(all(test, feature = "gpu"))]
mod gpu_tests {
    #[test]
    fn render_api_matches_expected() {
        assert_eq!(
            kmux_render::KMUX_RENDER_API_VERSION,
            super::EXPECTED_RENDER_API
        );
    }
}
