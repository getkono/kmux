//! wgpu pipelines + GPU vertex/uniform types for the two render passes.
//!
//! Both passes generate a unit quad from the vertex index (no vertex buffer) and
//! read one instance per quad. The solid pass (`bg_quad.wgsl`) draws flat-color
//! rectangles; the glyph pass (`glyph_quad.wgsl`) samples an atlas page bound at
//! group 1 and tints by the cell color. Straight (non-premultiplied) alpha
//! blending matches the CPU renderers' display-space compositing.

use bytemuck::{Pod, Zeroable};

/// Per-frame uniform: the viewport size used to map pixels → NDC. 16-byte
/// aligned (`vec2` + padding) for std140/uniform layout.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Globals {
    /// Content-area size in physical pixels.
    pub viewport: [f32; 2],
    pub _pad: [f32; 2],
}

/// One solid-color quad instance.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct SolidInstance {
    /// `x, y, w, h` in physical px (top-left origin).
    pub rect: [f32; 4],
    /// Straight RGBA.
    pub color: [f32; 4],
}

/// One textured glyph quad instance.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct GlyphInstance {
    /// `x, y, w, h` in physical px.
    pub rect: [f32; 4],
    /// `u0, v0, u1, v1` atlas texture coords.
    pub uv: [f32; 4],
    /// Straight RGBA tint.
    pub color: [f32; 4],
}

const SOLID_ATTRS: [wgpu::VertexAttribute; 2] =
    wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4];
const GLYPH_ATTRS: [wgpu::VertexAttribute; 3] =
    wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];

fn solid_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: size_of::<SolidInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &SOLID_ATTRS,
    }
}

fn glyph_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: size_of::<GlyphInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &GLYPH_ATTRS,
    }
}

/// Straight (non-premultiplied) src-over alpha blending.
fn alpha_blend() -> wgpu::BlendState {
    wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::SrcAlpha,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        },
    }
}

/// The two render pipelines + the bind group layouts they share.
pub struct Pipelines {
    /// Flat-color quad pipeline.
    pub solid: wgpu::RenderPipeline,
    /// Textured glyph pipeline.
    pub glyph: wgpu::RenderPipeline,
    /// Group 0: the `Globals` uniform (both pipelines).
    pub globals_layout: wgpu::BindGroupLayout,
    /// Group 1: an atlas page texture + sampler (glyph pipeline).
    pub atlas_layout: wgpu::BindGroupLayout,
}

impl Pipelines {
    /// Build both pipelines for a color target of `format`.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kmux-render globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kmux-render atlas"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let solid_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bg_quad"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/bg_quad.wgsl").into()),
        });
        let glyph_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glyph_quad"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/glyph_quad.wgsl").into()),
        });

        let solid_pl_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kmux-render solid layout"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });
        let glyph_pl_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kmux-render glyph layout"),
            bind_group_layouts: &[Some(&globals_layout), Some(&atlas_layout)],
            immediate_size: 0,
        });

        let target = wgpu::ColorTargetState {
            format,
            blend: Some(alpha_blend()),
            write_mask: wgpu::ColorWrites::ALL,
        };

        let solid = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("kmux-render solid"),
            layout: Some(&solid_pl_layout),
            vertex: wgpu::VertexState {
                module: &solid_shader,
                entry_point: Some("vs_main"),
                buffers: &[solid_layout()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &solid_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(target.clone())],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let glyph = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("kmux-render glyph"),
            layout: Some(&glyph_pl_layout),
            vertex: wgpu::VertexState {
                module: &glyph_shader,
                entry_point: Some("vs_main"),
                buffers: &[glyph_layout()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &glyph_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(target)],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            solid,
            glyph,
            globals_layout,
            atlas_layout,
        }
    }
}
