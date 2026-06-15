//! Color conversion for the renderer.
//!
//! Colors reach the GPU as straight (non-premultiplied) normalized components in
//! the surface's own color space. The renderer uses a non-sRGB (UNORM) surface
//! and standard src-over alpha blending — the same as the CPU renderers it
//! replaces (Cairo's `set_source_rgb` / CoreGraphics), which also blend in
//! display space. No gamma (sRGB↔linear) conversion is applied, so the GPU
//! output matches the existing look pixel-for-pixel for opaque cells.

use kmux_app::theme::Rgb;
use kmux_protocol::messages::CellColor;

/// Normalize an 8-bit component to `[0.0, 1.0]`.
#[inline]
pub fn unorm(c: u8) -> f32 {
    c as f32 / 255.0
}

/// Convert 8-bit RGBA to normalized float RGBA.
#[inline]
pub fn rgba8(c: [u8; 4]) -> [f32; 4] {
    [unorm(c[0]), unorm(c[1]), unorm(c[2]), unorm(c[3])]
}

/// Convert an opaque [`Rgb`] to normalized float RGBA (alpha = 1).
#[inline]
pub fn rgb(c: Rgb) -> [f32; 4] {
    [unorm(c.r), unorm(c.g), unorm(c.b), 1.0]
}

/// Convert an opaque [`CellColor`] to normalized float RGBA (alpha = 1).
#[inline]
pub fn cell_color(c: CellColor) -> [f32; 4] {
    [unorm(c.r), unorm(c.g), unorm(c.b), 1.0]
}

/// Return `color` with its alpha component replaced.
#[inline]
pub fn with_alpha(mut color: [f32; 4], a: f32) -> [f32; 4] {
    color[3] = a;
    color
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unorm_endpoints() {
        assert_eq!(unorm(0), 0.0);
        assert_eq!(unorm(255), 1.0);
        assert!((unorm(128) - 0.5019608).abs() < 1e-6);
    }

    #[test]
    fn rgb_is_opaque() {
        assert_eq!(rgb(Rgb::new(255, 0, 128)), [1.0, 0.0, unorm(128), 1.0]);
    }

    #[test]
    fn rgba8_preserves_alpha() {
        assert_eq!(rgba8([0, 255, 0, 0]), [0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn with_alpha_only_touches_alpha() {
        assert_eq!(with_alpha([0.1, 0.2, 0.3, 1.0], 0.5), [0.1, 0.2, 0.3, 0.5]);
    }
}
