//! The renderer's error type.

/// Errors from constructing or driving a renderer.
///
/// Variants carry `String` detail rather than backend (wgpu) error types so the
/// enum lives in the wgpu-free core and the public surface never leaks the wgpu
/// version. Backend errors are stringified at the boundary.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// No GPU adapter satisfied the request (no usable backend).
    #[error("no compatible GPU adapter found")]
    NoAdapter,
    /// Requesting the logical device/queue from the adapter failed.
    #[error("failed to create GPU device: {0}")]
    Device(String),
    /// Creating or configuring the presentation surface failed.
    #[error("surface error: {0}")]
    Surface(String),
    /// Glyph atlas allocation/upload failed (e.g. a glyph too large to pack).
    #[error("glyph atlas error: {0}")]
    Atlas(String),
    /// Font discovery, loading, or rasterization failed.
    #[error("font error: {0}")]
    Font(String),
}
