//! Packed, FFI-stable encoding of the terminal grid for the Swift renderer.
//!
//! The format itself now lives in [`kmux_render::packed`] — the single owner,
//! guarded by [`kmux_render::KMUX_RENDER_API_VERSION`], so the GPU renderer and
//! this FFI path encode/decode identical bytes (see docs/architecture-render.md
//! and issue #132). This module just re-exports the pieces the FFI uses, so the
//! call sites and the generated Swift bindings are unchanged.
//!
//! The layout and behavior are documented on [`kmux_render::packed`]: 16 bytes
//! per cell, row-major, with `DEFAULT_FG`/`DEFAULT_BG` resolved against the
//! palette in Rust and scrollback composited into the visible rows when scrolled.

pub use kmux_render::packed::{cursor_shape_code, encode_cells};

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_client::grid::CellGrid;
    use kmux_protocol::messages::CursorShape;

    #[test]
    fn re_exports_resolve_to_kmux_render() {
        // A light smoke test that the re-exported format is wired through; the
        // exhaustive coverage lives in kmux_render::packed.
        assert_eq!(cursor_shape_code(CursorShape::Bar), 2);
        let theme = kmux_app::theme::default_theme();
        let grid = CellGrid::new(2, 3);
        let packed = encode_cells(&grid, &theme);
        assert_eq!(packed.len(), 2 * 3 * kmux_render::packed::PACKED_CELL_LEN);
    }
}
