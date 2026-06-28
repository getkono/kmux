//! `TerminalRenderer`: the wgpu device + targets + per-frame draw.
//!
//! Construction picks any adapter (a software/fallback adapter is accepted, so
//! the offscreen path runs headlessly in CI), builds the two pipelines, and
//! derives [`RenderMetrics`] from the appearance. [`TerminalRenderer::render`]
//! turns a [`Frame`] into scene geometry ([`crate::geometry::build_scene`]),
//! resolves each glyph through the atlas, and draws four passes in order: cell
//! backgrounds → cell glyphs → overlays (rules/wash/cursor/border) → overlay
//! glyphs (block-cursor glyph + scroll text).
//!
//! Two targets are supported: an offscreen texture the caller reads back (the
//! GTK path, via `read_pixels` → `GdkMemoryTexture`) and a direct platform
//! surface presented each frame (the Swift/macOS `CAMetalLayer` path, driven by
//! the FFI renderer object).

use std::mem::size_of;

use kmux_app::appearance::Appearance;
use kmux_app::theme::Theme;

use crate::atlas::{Atlas, GlyphKey};
use crate::color;
use crate::frame::Frame;
use crate::geometry::{self, GlyphQuad, SolidQuad};
use crate::metrics::RenderMetrics;
use crate::pipeline::{Globals, GlyphInstance, Pipelines, SolidInstance};
use crate::{RenderError, geometry::CellMetrics};

/// Atlas page side length in px. One page holds a typical terminal's glyph set.
const ATLAS_PAGE: u32 = 1024;
/// Color target format. Non-sRGB so colors are written straight (no gamma),
/// matching the CPU renderers; the readback is therefore plain RGBA8.
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Pixels read back from an offscreen render: tightly-packed row-major RGBA8.
#[derive(Debug, Clone)]
pub struct RenderedPixels {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes, R,G,B,A per pixel, no row padding.
    pub rgba: Vec<u8>,
}

/// Where the renderer presents: an offscreen texture read back by the caller
/// (GTK), or a platform surface presented directly (Swift/macOS CAMetalLayer).
enum Target {
    /// Render to an internal texture; `read_pixels` copies it out.
    Offscreen { texture: wgpu::Texture },
    /// Render to and present a swapchain surface (no readback). Only ever
    /// constructed by the macOS `CAMetalLayer` path (`new_for_metal_layer`); on
    /// other platforms the GTK client uses the offscreen route, so the variant
    /// is intentionally never built there (its match arms stay for one code path).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Surface {
        surface: wgpu::Surface<'static>,
        config: wgpu::SurfaceConfiguration,
    },
}

fn make_offscreen_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("kmux-render offscreen"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// A GPU terminal renderer that draws into an offscreen RGBA texture.
pub struct TerminalRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipelines: Pipelines,
    globals_buf: wgpu::Buffer,
    globals_bg: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    atlas: Atlas,
    atlas_textures: Vec<wgpu::Texture>,
    atlas_bind_groups: Vec<wgpu::BindGroup>,
    metrics: RenderMetrics,
    appearance: Appearance,
    palette: Theme,
    target: Target,
    width: u32,
    height: u32,
    /// Per-row scene cache: reuses the cell-layer geometry of unchanged rows
    /// across frames so a single-line update does not rebuild the whole scene
    /// (issue #182, §3).
    scene_cache: geometry::SceneCache,
}

impl TerminalRenderer {
    /// Build an offscreen renderer of `width × height` physical px (the GTK
    /// path; `read_pixels` returns the rendered frame).
    pub fn new_offscreen(
        width: u32,
        height: u32,
        scale: f32,
        appearance: &Appearance,
        palette: &Theme,
    ) -> Result<Self, RenderError> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(request_adapter(&instance, None))?;
        let (device, queue) = pollster::block_on(request_device(&adapter))?;
        let (width, height) = (width.max(1), height.max(1));
        let texture = make_offscreen_texture(&device, width, height);
        let info = adapter.get_info();
        tracing::debug!(
            adapter = %info.name,
            backend = ?info.backend,
            width,
            height,
            scale,
            "kmux-render: offscreen renderer created"
        );
        Ok(Self::assemble(
            device,
            queue,
            TARGET_FORMAT,
            Target::Offscreen { texture },
            width,
            height,
            scale,
            appearance,
            palette,
        ))
    }

    /// Build a renderer that presents directly to a `CAMetalLayer` (the
    /// Swift/macOS path; no readback).
    ///
    /// # Safety
    /// `layer_ptr` must be a valid `CAMetalLayer` pointer that outlives the
    /// returned renderer; the caller (the Swift view) owns and keeps it alive.
    pub unsafe fn new_for_metal_layer(
        layer_ptr: u64,
        width: u32,
        height: u32,
        scale: f32,
        appearance: &Appearance,
        palette: &Theme,
    ) -> Result<Self, RenderError> {
        #[cfg(not(target_os = "macos"))]
        {
            // `wgpu::SurfaceTargetUnsafe::CoreAnimationLayer` exists only on Apple
            // targets, so naming it off-Apple fails to compile. The GPU path on
            // other platforms presents via the offscreen route and never reaches
            // this constructor; keep the signature (uniffi exports it across
            // platforms) but bail rather than reference the Apple-only variant.
            let _ = (layer_ptr, width, height, scale, appearance, palette);
            Err(RenderError::Surface(
                "CAMetalLayer surface is only available on macOS".to_string(),
            ))
        }
        #[cfg(target_os = "macos")]
        {
            let instance = wgpu::Instance::default();
            let surface = unsafe {
                instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                    layer_ptr as *mut std::ffi::c_void,
                ))
            }
            .map_err(|e| RenderError::Surface(e.to_string()))?;
            let adapter = pollster::block_on(request_adapter(&instance, Some(&surface)))?;
            let (device, queue) = pollster::block_on(request_device(&adapter))?;
            let (width, height) = (width.max(1), height.max(1));

            let caps = surface.get_capabilities(&adapter);
            // Prefer a non-sRGB format so colors are written straight (no gamma),
            // matching the offscreen/CPU path.
            let format = caps
                .formats
                .iter()
                .copied()
                .find(|f| !f.is_srgb())
                .unwrap_or_else(|| caps.formats[0]);
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width,
                height,
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode: caps
                    .alpha_modes
                    .first()
                    .copied()
                    .unwrap_or(wgpu::CompositeAlphaMode::Auto),
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            };
            surface.configure(&device, &config);

            let info = adapter.get_info();
            tracing::info!(
                adapter = %info.name,
                backend = ?info.backend,
                format = ?format,
                width,
                height,
                scale,
                "kmux-render: Metal surface renderer created"
            );
            Ok(Self::assemble(
                device,
                queue,
                format,
                Target::Surface { surface, config },
                width,
                height,
                scale,
                appearance,
                palette,
            ))
        }
    }

    /// Shared construction once a device/queue/target/format are chosen.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        device: wgpu::Device,
        queue: wgpu::Queue,
        format: wgpu::TextureFormat,
        target: Target,
        width: u32,
        height: u32,
        scale: f32,
        appearance: &Appearance,
        palette: &Theme,
    ) -> Self {
        let pipelines = Pipelines::new(&device, format);
        let metrics = RenderMetrics::from_appearance(appearance, scale);
        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kmux-render globals"),
            size: size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kmux-render globals bg"),
            layout: &pipelines.globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("kmux-render atlas sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Self {
            device,
            queue,
            pipelines,
            globals_buf,
            globals_bg,
            sampler,
            atlas: Atlas::new(ATLAS_PAGE),
            atlas_textures: Vec::new(),
            atlas_bind_groups: Vec::new(),
            metrics,
            appearance: appearance.clone(),
            palette: palette.clone(),
            target,
            width,
            height,
            scene_cache: geometry::SceneCache::default(),
        }
    }

    /// API/ABI version of the renderer surface.
    pub const fn api_version() -> u32 {
        crate::KMUX_RENDER_API_VERSION
    }

    /// The resolved metrics (cols/rows authority, cell geometry, faces).
    pub fn metrics(&self) -> &RenderMetrics {
        &self.metrics
    }

    /// Map a content-area pixel size to `(cols, rows)` via the metrics.
    pub fn cols_rows(&self, w_px: i32, h_px: i32) -> (u16, u16) {
        self.metrics.cols_rows(w_px, h_px)
    }

    /// Resize the target and/or rebuild metrics for a new scale.
    pub fn resize(&mut self, width: u32, height: u32, scale: f32) {
        let (width, height) = (width.max(1), height.max(1));
        if width != self.width || height != self.height {
            tracing::debug!(
                from = ?(self.width, self.height),
                to = ?(width, height),
                "kmux-render: resizing target"
            );
            match &mut self.target {
                Target::Offscreen { texture } => {
                    *texture = make_offscreen_texture(&self.device, width, height);
                }
                Target::Surface { surface, config } => {
                    config.width = width;
                    config.height = height;
                    surface.configure(&self.device, config);
                }
            }
            self.width = width;
            self.height = height;
        }
        if (scale - self.metrics.scale()).abs() > f32::EPSILON {
            tracing::debug!(
                old = self.metrics.scale(),
                new = scale,
                "kmux-render: scale changed; rebuilding metrics + atlas"
            );
            self.metrics = RenderMetrics::from_appearance(&self.appearance, scale);
            self.reset_atlas();
        }
    }

    /// Replace the font appearance (rebuilds faces + atlas).
    pub fn set_appearance(&mut self, appearance: &Appearance) {
        tracing::debug!("kmux-render: appearance changed; rebuilding faces + atlas");
        self.appearance = appearance.clone();
        self.metrics = RenderMetrics::from_appearance(appearance, self.metrics.scale());
        self.reset_atlas();
    }

    /// Replace the palette (affects the clear color + future Grid resolution).
    pub fn set_palette(&mut self, palette: &Theme) {
        self.palette = palette.clone();
    }

    /// Render one frame into the offscreen texture.
    pub fn render(&mut self, frame: &Frame<'_>) -> Result<(), RenderError> {
        tracing::trace!(
            width = frame.width,
            height = frame.height,
            scale = frame.scale,
            panes = frame.panes.len(),
            multi = frame.multi,
            "kmux-render: rendering frame"
        );
        self.resize(frame.width, frame.height, frame.scale);
        self.palette = frame.palette.clone();

        let cell: CellMetrics = *self.metrics.cell();
        let scene = geometry::build_scene_cached(frame, &cell, &mut self.scene_cache);

        self.queue.write_buffer(
            &self.globals_buf,
            0,
            bytemuck::bytes_of(&Globals {
                viewport: [self.width as f32, self.height as f32],
                _pad: [0.0, 0.0],
            }),
        );

        let bg = solids(&scene.bg_quads);
        let overlay = solids(&scene.overlay_quads);
        let (cell_glyphs, cell_ranges) = self.resolve_glyphs(&scene.glyphs);
        let (overlay_glyphs, overlay_ranges) = self.resolve_glyphs(&scene.overlay_glyphs);
        self.sync_atlas_textures();

        let bg_buf = self.instance_buffer(&bg);
        let overlay_buf = self.instance_buffer(&overlay);
        let cell_glyph_buf = self.instance_buffer(&cell_glyphs);
        let overlay_glyph_buf = self.instance_buffer(&overlay_glyphs);

        let clear = self.clear_color();

        // Acquire the color target view; for a surface, also a frame to present.
        let surface_frame = match &self.target {
            Target::Surface { surface, .. } => match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(frame)
                | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => Some(frame),
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    tracing::debug!(
                        "kmux-render: surface outdated/lost; reconfiguring (frame skipped)"
                    );
                    if let Target::Surface { surface, config } = &self.target {
                        surface.configure(&self.device, config);
                    }
                    return Ok(()); // reconfigured; next frame redraws
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    tracing::trace!("kmux-render: surface timeout/occluded; skipping frame");
                    return Ok(()); // transient; skip this frame
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    tracing::warn!("kmux-render: surface validation error acquiring frame");
                    return Err(RenderError::Surface("surface validation error".into()));
                }
            },
            Target::Offscreen { .. } => None,
        };
        let view = match (&self.target, &surface_frame) {
            (Target::Offscreen { texture }, _) => {
                texture.create_view(&wgpu::TextureViewDescriptor::default())
            }
            (_, Some(frame)) => frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default()),
            _ => unreachable!("surface target without an acquired frame"),
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("kmux-render encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("kmux-render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.draw_solids(&mut pass, &bg_buf, bg.len());
            self.draw_glyphs(&mut pass, &cell_glyph_buf, &cell_ranges);
            self.draw_solids(&mut pass, &overlay_buf, overlay.len());
            self.draw_glyphs(&mut pass, &overlay_glyph_buf, &overlay_ranges);
        }
        self.queue.submit(Some(encoder.finish()));
        if let Some(frame) = surface_frame {
            frame.present();
        }
        Ok(())
    }

    /// Read the last rendered frame back as tightly-packed RGBA8 (offscreen
    /// targets only; an error for a surface target).
    pub fn read_pixels(&mut self) -> Result<RenderedPixels, RenderError> {
        let Target::Offscreen { texture } = &self.target else {
            return Err(RenderError::Surface(
                "read_pixels requires an offscreen target".into(),
            ));
        };
        let texture = texture.clone();
        let (w, h) = (self.width, self.height);
        let unpadded = w * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded = unpadded.div_ceil(align) * align;

        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kmux-render readback"),
            size: (padded * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("kmux-render readback encoder"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| RenderError::Device(e.to_string()))?;
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(RenderError::Surface(e.to_string())),
            Err(e) => return Err(RenderError::Surface(e.to_string())),
        }

        let data = slice.get_mapped_range();
        let mut rgba = Vec::with_capacity((unpadded * h) as usize);
        for row in 0..h {
            let start = (row * padded) as usize;
            rgba.extend_from_slice(&data[start..start + unpadded as usize]);
        }
        drop(data);
        buf.unmap();
        Ok(RenderedPixels {
            width: w,
            height: h,
            rgba,
        })
    }

    fn reset_atlas(&mut self) {
        self.atlas = Atlas::new(ATLAS_PAGE);
        self.atlas_textures.clear();
        self.atlas_bind_groups.clear();
    }

    fn clear_color(&self) -> wgpu::Color {
        let c = color::rgb(self.palette.bg);
        wgpu::Color {
            r: c[0] as f64,
            g: c[1] as f64,
            b: c[2] as f64,
            a: 1.0,
        }
    }

    /// Resolve glyph quads to instances grouped by atlas page. Returns the flat
    /// instance list (ordered by page) + `(page, start, count)` ranges.
    fn resolve_glyphs(
        &mut self,
        quads: &[GlyphQuad],
    ) -> (Vec<GlyphInstance>, Vec<(usize, u32, u32)>) {
        let atlas = &mut self.atlas;
        let metrics = &self.metrics;
        let px = metrics.px_size();
        let ascent = metrics.ascent();
        let page = atlas.page_size() as f32;

        // The bundled symbol fallback font (borrowed once): glyphs the configured
        // face lacks — Powerline U+E0Bx, Nerd PUA icons — rasterize from this
        // instead of rendering blank. The `'static` face never conflicts with the
        // `&mut atlas` borrow.
        let fallback = crate::fallback::symbol_fallback().as_ref();

        // Bucket per page, then flatten so each page's instances are contiguous.
        let mut buckets: Vec<Vec<GlyphInstance>> = Vec::new();
        for q in quads {
            let Some(face) = metrics.faces().face(q.style) else {
                continue;
            };
            let Some(font) = face.as_ref() else {
                continue;
            };
            // If the embedded fallback somehow failed to parse, use the primary
            // face as its own fallback (a no-op, preserving old behavior).
            let Some(e) = atlas.get_or_insert(
                font,
                fallback.unwrap_or(font),
                px,
                GlyphKey {
                    style: q.style,
                    ch: q.ch,
                },
            ) else {
                continue;
            };
            if e.page >= buckets.len() {
                buckets.resize_with(e.page + 1, Vec::new);
            }
            buckets[e.page].push(GlyphInstance {
                rect: [
                    q.cell_x + e.left as f32,
                    q.cell_y + ascent - e.top as f32,
                    e.w as f32,
                    e.h as f32,
                ],
                uv: [
                    e.x as f32 / page,
                    e.y as f32 / page,
                    (e.x + e.w) as f32 / page,
                    (e.y + e.h) as f32 / page,
                ],
                color: q.color,
            });
        }

        let mut instances = Vec::new();
        let mut ranges = Vec::new();
        for (page_idx, bucket) in buckets.into_iter().enumerate() {
            if bucket.is_empty() {
                continue;
            }
            let start = instances.len() as u32;
            let count = bucket.len() as u32;
            instances.extend(bucket);
            ranges.push((page_idx, start, count));
        }
        (instances, ranges)
    }

    /// Ensure a GPU texture + bind group exists per atlas page and upload any
    /// pages dirtied since the last call.
    fn sync_atlas_textures(&mut self) {
        while self.atlas_textures.len() < self.atlas.page_count() {
            let size = self.atlas.page_size();
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("kmux-render atlas page"),
                size: wgpu::Extent3d {
                    width: size,
                    height: size,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("kmux-render atlas bg"),
                layout: &self.pipelines.atlas_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.atlas_textures.push(texture);
            self.atlas_bind_groups.push(bind_group);
        }

        let queue = &self.queue;
        let textures = &self.atlas_textures;
        self.atlas.upload_dirty(|i, size, rgba| {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &textures[i],
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size * 4),
                    rows_per_image: Some(size),
                },
                wgpu::Extent3d {
                    width: size,
                    height: size,
                    depth_or_array_layers: 1,
                },
            );
        });
    }

    fn instance_buffer<T: bytemuck::Pod>(&self, data: &[T]) -> wgpu::Buffer {
        use wgpu::util::DeviceExt;
        // Never create a zero-sized buffer; an empty pass simply isn't drawn.
        let bytes = if data.is_empty() {
            vec![0u8; size_of::<T>()]
        } else {
            bytemuck::cast_slice(data).to_vec()
        };
        self.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("kmux-render instances"),
                contents: &bytes,
                usage: wgpu::BufferUsages::VERTEX,
            })
    }

    fn draw_solids(&self, pass: &mut wgpu::RenderPass<'_>, buf: &wgpu::Buffer, count: usize) {
        if count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipelines.solid);
        pass.set_bind_group(0, &self.globals_bg, &[]);
        pass.set_vertex_buffer(0, buf.slice(..));
        pass.draw(0..6, 0..count as u32);
    }

    fn draw_glyphs(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        buf: &wgpu::Buffer,
        ranges: &[(usize, u32, u32)],
    ) {
        if ranges.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipelines.glyph);
        pass.set_bind_group(0, &self.globals_bg, &[]);
        pass.set_vertex_buffer(0, buf.slice(..));
        for &(page, start, count) in ranges {
            pass.set_bind_group(1, &self.atlas_bind_groups[page], &[]);
            pass.draw(0..6, start..start + count);
        }
    }
}

fn solids(quads: &[SolidQuad]) -> Vec<SolidInstance> {
    quads
        .iter()
        .map(|q| SolidInstance {
            rect: [q.x, q.y, q.w, q.h],
            color: q.color,
        })
        .collect()
}

async fn request_adapter(
    instance: &wgpu::Instance,
    compatible_surface: Option<&wgpu::Surface<'_>>,
) -> Result<wgpu::Adapter, RenderError> {
    // Prefer a real adapter; accept a software/fallback one so CI can run.
    for force_fallback_adapter in [false, true] {
        let res = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface,
                force_fallback_adapter,
            })
            .await;
        if let Ok(adapter) = res {
            return Ok(adapter);
        }
    }
    Err(RenderError::NoAdapter)
}

async fn request_device(
    adapter: &wgpu::Adapter,
) -> Result<(wgpu::Device, wgpu::Queue), RenderError> {
    adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("kmux-render device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        })
        .await
        .map_err(|e| RenderError::Device(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{CellSource, PaneView};
    use kmux_app::theme::{Rgb, Theme};
    use kmux_client::grid::CellGrid;
    use kmux_protocol::messages::{
        CellAttrs, CellColor, CellState, CursorState, GridSnapshot, TermModes,
    };

    fn theme() -> Theme {
        let mut t = kmux_app::theme::default_theme();
        t.bg = Rgb::new(0x12, 0x34, 0x56);
        t
    }

    fn try_renderer(w: u32, h: u32) -> Option<TerminalRenderer> {
        match TerminalRenderer::new_offscreen(w, h, 1.0, &Appearance::default(), &theme()) {
            Ok(r) => Some(r),
            Err(RenderError::NoAdapter) => {
                eprintln!("kmux-render GPU test skipped: no adapter");
                None
            }
            Err(e) => panic!("renderer init failed: {e}"),
        }
    }

    fn grid_1x1(bg: CellColor, attrs: u16) -> CellGrid {
        let mut g = CellGrid::new(1, 1);
        g.apply_snapshot(GridSnapshot {
            rows: 1,
            cols: 1,
            cells: vec![CellState {
                c: ' ',
                fg: CellColor::new(0xff, 0xff, 0xff),
                bg,
                attrs: CellAttrs(attrs),
            }],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_base: 0,
            scrollback_tail: Vec::new(),
        });
        g
    }

    fn pane(grid: &CellGrid) -> PaneView<'_> {
        PaneView {
            col: 0,
            row: 0,
            cols: 1,
            rows: 1,
            focused: true,
            cells: CellSource::Grid(grid),
            cursor: None,
            selection: &[],
            scroll: None,
        }
    }

    #[test]
    fn offscreen_clears_to_palette_bg() {
        let Some(mut r) = try_renderer(64, 32) else {
            return;
        };
        let palette = theme();
        // Default-bg cell resolves to palette.bg, as does the cleared area.
        let grid = grid_1x1(
            CellColor::new(0, 0, 0),
            CellAttrs::DEFAULT_FG | CellAttrs::DEFAULT_BG,
        );
        let frame = Frame::single(64, 32, 1.0, &palette, true, pane(&grid));
        r.render(&frame).unwrap();
        let px = r.read_pixels().unwrap();
        assert_eq!((px.width, px.height), (64, 32));
        assert_eq!(px.rgba.len(), 64 * 32 * 4);
        // A pixel away from the single cell is the clear color = palette.bg.
        let far = ((20 * 64 + 40) * 4) as usize;
        assert_eq!(
            &px.rgba[far..far + 3],
            &[palette.bg.r, palette.bg.g, palette.bg.b]
        );
    }

    #[test]
    fn offscreen_paints_explicit_cell_bg() {
        let Some(mut r) = try_renderer(32, 32) else {
            return;
        };
        let palette = theme();
        // An explicit red background (no DEFAULT_BG) should paint the top-left.
        let grid = grid_1x1(CellColor::new(0xff, 0x00, 0x00), 0);
        let frame = Frame::single(32, 32, 1.0, &palette, true, pane(&grid));
        r.render(&frame).unwrap();
        let px = r.read_pixels().unwrap();
        // Top-left pixel is inside cell (0,0)'s background quad.
        assert_eq!(&px.rgba[0..4], &[0xff, 0x00, 0x00, 0xff]);
    }

    #[test]
    fn golden_two_cell_row_positions_colors() {
        let Some(mut r) = try_renderer(64, 32) else {
            return;
        };
        let palette = theme();
        // One row, two cells: red then green (explicit, space chars → no glyph
        // over the background at the sample points).
        let mut grid = CellGrid::new(1, 2);
        grid.apply_snapshot(GridSnapshot {
            rows: 1,
            cols: 2,
            cells: vec![
                CellState {
                    c: ' ',
                    fg: CellColor::new(0xff, 0xff, 0xff),
                    bg: CellColor::new(0xff, 0, 0),
                    attrs: CellAttrs::EMPTY,
                },
                CellState {
                    c: ' ',
                    fg: CellColor::new(0xff, 0xff, 0xff),
                    bg: CellColor::new(0, 0xff, 0),
                    attrs: CellAttrs::EMPTY,
                },
            ],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_base: 0,
            scrollback_tail: Vec::new(),
        });
        let pane = PaneView {
            col: 0,
            row: 0,
            cols: 2,
            rows: 1,
            focused: true,
            cells: CellSource::Grid(&grid),
            cursor: None,
            selection: &[],
            scroll: None,
        };
        r.render(&Frame::single(64, 32, 1.0, &palette, true, pane))
            .unwrap();
        let cw = r.metrics().cell().cell_w;
        let px = r.read_pixels().unwrap();
        let sample = |x: usize, y: usize| {
            let i = (y * 64 + x) * 4;
            [px.rgba[i], px.rgba[i + 1], px.rgba[i + 2]]
        };
        // Center of cell 0 is red; center of cell 1 is green.
        assert_eq!(sample((cw * 0.5) as usize, 8), [0xff, 0, 0]);
        assert_eq!(sample((cw * 1.5) as usize, 8), [0, 0xff, 0]);
    }
}
