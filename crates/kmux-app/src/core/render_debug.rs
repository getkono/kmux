//! Toolkit-agnostic render-debug snapshot: the logical state the renderer is
//! handed for the focused pane each frame. Built to debug incorrect cursor
//! rendering — it exposes the cursor's grid position, shape, visibility and the
//! `is_drawn` decision so a frontend can compare them against what it actually
//! paints.
//!
//! ## Why logical-only
//!
//! kmux-app cannot depend on `kmux-render` — `kmux-render` depends on `kmux-app`,
//! so the reverse direction would be a cargo dependency cycle. This layer
//! therefore reports only *logical* data (no pixel rects, no `CellMetrics`). The
//! frontends, which already depend on `kmux-render`, turn the logical cursor into
//! the exact pixel rects the renderer fills via [`kmux_render::cursor_geometry`]
//! and overlay that next to their own draw — the divergence is the bug signal.

use kmux_protocol::messages::CursorShape;

/// What the renderer is handed for the focused pane this frame. The frontend
/// supplies the pixel/scale/renderer context it owns; kmux-app fills the logical
/// pane + cursor state.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderDebugSnapshot {
    /// Content-area width in physical px, as the frontend sees it.
    pub frame_width: u32,
    /// Content-area height in physical px.
    pub frame_height: u32,
    /// Device scale (e.g. `2.0` on a Retina display).
    pub scale: f32,
    /// The active renderer leaf: `"cairo"` | `"wgpu"` | `"coretext"` | `"metal"`.
    pub renderer: String,
    /// The frame-wide cursor-blink phase the renderer uses this frame.
    pub blink_on: bool,
    /// The focused pane's logical state, or `None` when no pane is active.
    pub pane: Option<PaneDebug>,
}

/// The focused pane's logical render state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneDebug {
    /// Focused pane id (`"session/index"`).
    pub pane_id: String,
    /// Grid width in cells.
    pub grid_cols: u16,
    /// Grid height in cells.
    pub grid_rows: u16,
    /// Rows scrolled back from the live bottom (`0` = live screen).
    pub scroll_offset: usize,
    /// The cursor, or `None` when scrolled into history — matching the renderer,
    /// which hides the cursor against scrollback.
    pub cursor: Option<CursorDebug>,
}

/// The focused pane's cursor as the renderer sees it (logical, not yet pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorDebug {
    /// Cursor column (0-based, viewport-relative).
    pub col: u16,
    /// Cursor row (0-based, viewport-relative).
    pub row: u16,
    /// Cursor shape (the inner program's DECSCUSR request).
    pub shape: CursorShape,
    /// Whether the program requested a blinking cursor (DECSCUSR).
    pub blink: bool,
    /// Whether the cursor is visible at all (DECTCEM).
    pub visible: bool,
    /// Whether the cursor actually paints this frame:
    /// `visible && shape != Hidden && (!blink || blink_on)`. This mirrors
    /// `kmux_render::CursorView::is_drawn`, duplicated as a one-liner because
    /// kmux-app cannot depend on kmux-render (see the module docs).
    pub is_drawn: bool,
}

impl CursorDebug {
    /// Build from a grid [`CursorShape`]-bearing cursor state, computing
    /// `is_drawn` against the frame's `blink_on`.
    fn new(
        col: u16,
        row: u16,
        shape: CursorShape,
        blink: bool,
        visible: bool,
        blink_on: bool,
    ) -> Self {
        let is_drawn = visible && !matches!(shape, CursorShape::Hidden) && (!blink || blink_on);
        Self {
            col,
            row,
            shape,
            blink,
            visible,
            is_drawn,
        }
    }
}

impl super::AppCore {
    /// Assemble a [`RenderDebugSnapshot`] for the focused pane.
    ///
    /// `blink_on` is the current blink phase (kmux-app's blink lives on the
    /// [`crate::driver::FrontendDriver`], so the caller passes it). `renderer` is
    /// the frontend's active renderer leaf, and `frame_width`/`frame_height`/
    /// `scale` are the pixel context the frontend owns.
    pub fn render_debug_snapshot(
        &self,
        frame_width: u32,
        frame_height: u32,
        scale: f32,
        renderer: &str,
        blink_on: bool,
    ) -> RenderDebugSnapshot {
        let pane = match (self.mgr.active_pane_id(), self.mgr.active_grid()) {
            (Some(id), Some(grid)) => {
                let scroll_offset = grid.scroll_offset();
                // The cursor only shows against the live screen, not scrollback —
                // matching the GPU/CPU paint paths.
                let cursor = (scroll_offset == 0).then(|| {
                    let cs = grid.cursor();
                    CursorDebug::new(cs.col, cs.row, cs.shape, cs.blink, cs.visible, blink_on)
                });
                Some(PaneDebug {
                    pane_id: id.to_string(),
                    grid_cols: grid.cols as u16,
                    grid_rows: grid.rows as u16,
                    scroll_offset,
                    cursor,
                })
            }
            _ => None,
        };
        RenderDebugSnapshot {
            frame_width,
            frame_height,
            scale,
            renderer: renderer.to_string(),
            blink_on,
            pane,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CursorDebug;
    use crate::core::AppCore;
    use kmux_client::session_manager::SessionManager;
    use kmux_protocol::messages::{ClientCapabilities, CursorShape};

    fn empty_core() -> AppCore {
        let mgr = SessionManager::new(
            "127.0.0.1".into(),
            0,
            String::new(),
            true,
            ClientCapabilities::default(),
        );
        AppCore::for_test(mgr)
    }

    #[test]
    fn snapshot_passes_through_context_and_has_no_pane_when_idle() {
        let core = empty_core();
        let snap = core.render_debug_snapshot(800, 600, 2.0, "wgpu", true);
        assert_eq!(snap.frame_width, 800);
        assert_eq!(snap.frame_height, 600);
        assert_eq!(snap.scale, 2.0);
        assert_eq!(snap.renderer, "wgpu");
        assert!(snap.blink_on);
        assert!(snap.pane.is_none()); // a bare manager has no active pane
    }

    #[test]
    fn cursor_is_drawn_follows_visibility_blink_and_shape() {
        // Steady + visible → drawn.
        assert!(CursorDebug::new(0, 0, CursorShape::Block, false, true, false).is_drawn);
        // DECTCEM off → never drawn, regardless of blink phase.
        assert!(!CursorDebug::new(0, 0, CursorShape::Block, false, false, true).is_drawn);
        // Hidden shape → never drawn.
        assert!(!CursorDebug::new(0, 0, CursorShape::Hidden, false, true, true).is_drawn);
        // Blinking: off phase hides it, on phase shows it.
        assert!(!CursorDebug::new(0, 0, CursorShape::Bar, true, true, false).is_drawn);
        assert!(CursorDebug::new(0, 0, CursorShape::Bar, true, true, true).is_drawn);
    }
}
