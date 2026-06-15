//! kmux-render — shared, cross-platform GPU terminal cell-grid renderer.
//!
//! kmux's VT parsing and terminal grid *state* are already consolidated
//! server-side (kmuxd parses via libghostty-vt and ships [`CellState`] diffs
//! the client applies to [`kmux_client::grid::CellGrid`]). What used to be
//! duplicated was the *render leaf*: each GUI frontend re-implemented cell-grid
//! drawing on its own CPU rasterizer (GTK via Cairo/Pango, the Swift app via
//! CoreText/CoreGraphics). This crate replaces both with one GPU-accelerated
//! (wgpu) renderer that both frontends drive — GTK directly (Rust→Rust), the
//! Swift app through `kmux-ffi`.
//!
//! ## Layering
//!
//! `kmux-render` sits *beside* the frontends and is consumed by them. It
//! depends downward on `kmux-protocol`/`kmux-client`/`kmux-app` for the
//! read-only types it renders ([`CellGrid`](kmux_client::grid::CellGrid),
//! [`Theme`](kmux_app::theme::Theme), [`Appearance`](kmux_app::appearance::Appearance))
//! but is **not** a dependency of any of them, so the hard rule "nothing at or
//! below `kmux-app` may depend on a UI toolkit" is preserved (and `kmux-render`
//! itself depends on no UI toolkit — wgpu's Metal/Vulkan/GL backends are GPU
//! driver APIs, the same category as `kmux-ghostty-sys` linking libghostty).
//!
//! ## Feature tiers
//!
//! The wgpu-free **core** ([`frame`], [`packed`], [`geometry`], [`color`]) is
//! always compiled, so `kmux-ffi` can own the packed-cell format without
//! pulling wgpu. Two opt-in tiers layer on top:
//!
//! - `text` — font metrics + glyph atlas ([`metrics`], [`atlas`]) via
//!   swash/etagere/fontdb. CPU-only and headlessly testable.
//! - `gpu` — the GPU renderer ([`renderer`], [`pipeline`]); implies `text`.
//!
//! [`CellState`]: kmux_protocol::messages::CellState

/// API/ABI version of the kmux-render public surface.
///
/// Bumped on any breaking change to the renderer API or the wire-packed cell
/// layout. `kmux-ffi` asserts that the `kmux-render` it links matches what it
/// was built against, mirroring the `KMUX_FFI_ABI_VERSION` /
/// `kmux-ghostty-sys::EXPECTED_ABI_VERSION` versioning discipline.
pub const KMUX_RENDER_API_VERSION: u32 = 1;

mod error;
pub use error::RenderError;

pub mod color;
pub mod frame;
pub mod geometry;
pub mod packed;

#[cfg(feature = "text")]
pub mod atlas;
#[cfg(feature = "text")]
pub mod metrics;

#[cfg(feature = "gpu")]
pub mod pipeline;
#[cfg(feature = "gpu")]
pub mod renderer;
