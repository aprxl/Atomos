//! [`Layer`] — a retained, independently-cached drawing surface.
//!
//! A `Layer` owns its own offscreen GPU texture and a queue of pending
//! [`DrawCommand`]s. Calling a `draw_*` method never touches the GPU
//! directly — it only records a command and marks the layer dirty; the
//! actual tessellation, upload, and render-to-texture happen once per frame
//! inside [`super::Renderer::render`], which also composites every live
//! layer's texture onto the screen in bottom-to-top order.
//!
//! Because a layer renders into its own texture, applying a shader to
//! "everything in the layer" later just means swapping which pipeline the
//! composite step binds for that layer — the seam is already here, just
//! unused for now (see `LayerPipelines::composite_pipeline`).
//!
//! `Layer` is a cheap-to-clone handle (`Rc<RefCell<LayerInner>>`); dropping
//! every clone of a given layer removes it from the renderer's composite
//! stack on the next frame (the registry holds only a `Weak`).

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use wgpu::util::DeviceExt;

use lyon::path::{self, builder::BorderRadii, Path};
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, FillVertexConstructor,
    VertexBuffers,
};
use lyon_extra::parser::{ParserOptions, PathParser, Source};

use super::color::Color;
use super::draw_command::DrawCommand;
use super::pixels::Pixels;
use super::rounding::Rounding;
use super::text_stack::TextStack;
use super::{Alignment, Font, FontParameters, TextSpan, Vertex, MSAA_SAMPLE_COUNT};

/// Error from [`Layer::draw_path_from_file`]: either the file couldn't be
/// read, or its contents didn't parse as SVG path data.
#[derive(Debug)]
pub enum PathFileError {
    Io(std::io::Error),
    Parse(lyon_extra::parser::ParseError),
}

impl std::fmt::Display for PathFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "failed to read path file: {e}"),
            Self::Parse(e) => write!(f, "failed to parse path data: {e:?}"),
        }
    }
}

impl std::error::Error for PathFileError {}

impl From<std::io::Error> for PathFileError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<lyon_extra::parser::ParseError> for PathFileError {
    fn from(e: lyon_extra::parser::ParseError) -> Self {
        Self::Parse(e)
    }
}

/// How a [`Layer`] decides its cached content is stale and needs rebuilding.
/// Chosen once, at [`super::Renderer::new_layer_top`]/
/// [`super::Renderer::new_layer_bottom`] time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerInvalidation {
    /// The caller re-issues the full set of `draw_*` calls every frame
    /// (immediate-mode style). The layer hashes the incoming content each
    /// frame and only re-tessellates/re-renders when the hash differs from
    /// the previous frame's — an unchanged frame's worth of `draw_*` calls
    /// costs a hash comparison, not a GPU rebuild.
    Automatic,
    /// `draw_*` calls accumulate (they are *not* cleared automatically) —
    /// call [`Layer::clear`] before redrawing when content actually
    /// changes. No per-frame hashing; the layer only rebuilds when
    /// something told it to.
    Manual,
}

enum InvalidationState {
    Automatic { last_hash: Option<u64> },
    Manual { dirty: bool },
}

/// Pixel-space `screen_size` uniform, shared by every layer's internal
/// solid pipeline (mirrors the renderer's existing `Transform` uniform, but
/// layers never need translate/scale — a `DrawCommand`'s own position is
/// already absolute pixel-space, baked straight into its tessellated
/// vertices).
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct ScreenSize {
    screen_size: [f32; 2],
}

/// The pixel-space solid-color shader every layer's internal render pass
/// uses to draw its tessellated `DrawCommand` geometry.
const LAYER_SOLID_SHADER_SRC: &str = r#"
struct ScreenSize {
    screen_size: vec2<f32>,
};
@group(0) @binding(0) var<uniform> screen: ScreenSize;

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) in_pos: vec2<f32>,
    @location(1) in_color: vec4<f32>,
) -> VsOut {
    var out: VsOut;
    let x = 2.0 * in_pos.x / screen.screen_size.x - 1.0;
    let y = 1.0 - 2.0 * in_pos.y / screen.screen_size.y;
    out.clip_pos = vec4<f32>(x, y, 0.0, 1.0);
    out.color = in_color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

/// The composite shader: samples one layer's already-rendered texture over
/// a full-screen triangle pair straight onto the swap-chain view. No
/// per-layer transform is needed — the layer's texture already covers the
/// whole surface 1:1.
const COMPOSITE_SHADER_SRC: &str = r#"
struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(
    @location(0) in_pos: vec2<f32>,
    @location(1) in_uv: vec2<f32>,
) -> VsOut {
    var out: VsOut;
    out.clip_pos = vec4<f32>(in_pos, 0.0, 1.0);
    out.uv = in_uv;
    return out;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

/// Vertex format for the composite full-screen quad: clip-space position
/// plus texture coordinate.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct CompositeVertex {
    position: [f32; 2],
    uv: [f32; 2],
}

impl CompositeVertex {
    const ATTRS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: size_of::<CompositeVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

/// A full-screen quad in clip space, UVs oriented so `(0,0)` samples the
/// top-left of the source texture (row 0 in a layer's own render pass is
/// pixel-space `y = 0`, i.e. the top of the layer's canvas).
const COMPOSITE_QUAD: &[CompositeVertex] = &[
    CompositeVertex { position: [-1.0, 1.0], uv: [0.0, 0.0] },
    CompositeVertex { position: [1.0, 1.0], uv: [1.0, 0.0] },
    CompositeVertex { position: [1.0, -1.0], uv: [1.0, 1.0] },
    CompositeVertex { position: [-1.0, 1.0], uv: [0.0, 0.0] },
    CompositeVertex { position: [1.0, -1.0], uv: [1.0, 1.0] },
    CompositeVertex { position: [-1.0, -1.0], uv: [0.0, 1.0] },
];

/// Vertex format for `draw_image`: a pixel-space position (same convention
/// as [`Vertex`]) plus a texture coordinate and a per-vertex tint color
/// (resolved from the image's tint [`Color`] against its own bounding box,
/// same rule [`Color::resolve`] uses everywhere else).
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct ImageVertex {
    position: [f32; 2],
    uv: [f32; 2],
    tint: [f32; 4],
}

impl ImageVertex {
    const ATTRS: [wgpu::VertexAttribute; 3] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: size_of::<ImageVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

/// The shader `draw_image` draws through: pixel-space position (via the
/// same `screen_size` uniform every layer's solid pipeline uses) and a
/// bound texture, sampled and multiplied by a per-vertex tint.
const LAYER_IMAGE_SHADER_SRC: &str = r#"
struct ScreenSize {
    screen_size: vec2<f32>,
};
@group(0) @binding(0) var<uniform> screen: ScreenSize;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) tint: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) in_pos: vec2<f32>,
    @location(1) in_uv: vec2<f32>,
    @location(2) in_tint: vec4<f32>,
) -> VsOut {
    var out: VsOut;
    let x = 2.0 * in_pos.x / screen.screen_size.x - 1.0;
    let y = 1.0 - 2.0 * in_pos.y / screen.screen_size.y;
    out.clip_pos = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = in_uv;
    out.tint = in_tint;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv) * in.tint;
}
"#;

/// Every GPU resource shared by all layers: the two pipelines and their
/// bind group layouts, plus the one composite quad every layer's composite
/// draw reuses. Built once in [`super::Renderer::new`], cloned (cheaply —
/// every field here is an `Arc`-backed wgpu handle) into each
/// [`LayerInner`].
pub(super) struct LayerPipelines {
    solid_pipeline: wgpu::RenderPipeline,
    screen_size_bind_group_layout: wgpu::BindGroupLayout,
    // `draw_image`'s pipeline. Shares `screen_size_bind_group_layout` (group
    // 0) with `solid_pipeline`, and reuses `composite_bind_group_layout`/
    // `composite_sampler` (group 1) — a texture+sampler pair, the exact
    // shape the composite step already needed.
    image_pipeline: wgpu::RenderPipeline,
    composite_pipeline: wgpu::RenderPipeline,
    composite_bind_group_layout: wgpu::BindGroupLayout,
    composite_sampler: wgpu::Sampler,
    composite_quad_vbuf: wgpu::Buffer,
}

impl LayerPipelines {
    pub(super) fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
        let screen_size_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("atomos-layer-screen-size-bgl"),
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

        let solid_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("atomos-layer-solid-shader"),
            source: wgpu::ShaderSource::Wgsl(LAYER_SOLID_SHADER_SRC.into()),
        });
        let solid_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("atomos-layer-solid-pipeline-layout"),
            bind_group_layouts: &[Some(&screen_size_bind_group_layout)],
            immediate_size: 0,
        });
        // Renders into the layer's multisampled offscreen target (see
        // `MSAA_SAMPLE_COUNT`), resolved into the layer's actual texture.
        let solid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("atomos-layer-solid-pipeline"),
            layout: Some(&solid_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &solid_shader,
                entry_point: Some("vs_main"),
                buffers: &[Vertex::layout()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &solid_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: MSAA_SAMPLE_COUNT,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        let composite_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("atomos-layer-composite-bgl"),
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

        let image_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("atomos-layer-image-shader"),
            source: wgpu::ShaderSource::Wgsl(LAYER_IMAGE_SHADER_SRC.into()),
        });
        let image_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("atomos-layer-image-pipeline-layout"),
            bind_group_layouts: &[
                Some(&screen_size_bind_group_layout),
                Some(&composite_bind_group_layout),
            ],
            immediate_size: 0,
        });
        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("atomos-layer-image-pipeline"),
            layout: Some(&image_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &image_shader,
                entry_point: Some("vs_main"),
                buffers: &[ImageVertex::layout()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &image_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    // Decoded images may carry their own alpha (e.g.
                    // transparent-background icons).
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: MSAA_SAMPLE_COUNT,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        let composite_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("atomos-layer-composite-shader"),
            source: wgpu::ShaderSource::Wgsl(COMPOSITE_SHADER_SRC.into()),
        });
        let composite_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("atomos-layer-composite-pipeline-layout"),
                bind_group_layouts: &[Some(&composite_bind_group_layout)],
                immediate_size: 0,
            });
        let composite_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("atomos-layer-composite-pipeline"),
            layout: Some(&composite_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &composite_shader,
                entry_point: Some("vs_main"),
                buffers: &[CompositeVertex::layout()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &composite_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    // A layer's texture is cleared to transparent, so
                    // untouched areas must let whatever composited before
                    // it show through.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let composite_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atomos-layer-composite-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let composite_quad_vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("atomos-layer-composite-quad-vbuf"),
            contents: bytemuck::cast_slice(COMPOSITE_QUAD),
            usage: wgpu::BufferUsages::VERTEX,
        });

        Self {
            solid_pipeline,
            screen_size_bind_group_layout,
            image_pipeline,
            composite_pipeline,
            composite_bind_group_layout,
            composite_sampler,
            composite_quad_vbuf,
        }
    }
}

/// The GPU/CPU state shared by every clone of a [`Layer`]. See the module
/// doc for the overall design.
pub(super) struct LayerInner {
    device: wgpu::Device,
    queue: wgpu::Queue,
    solid_pipeline: wgpu::RenderPipeline,
    image_pipeline: wgpu::RenderPipeline,
    composite_pipeline: wgpu::RenderPipeline,
    composite_bind_group_layout: wgpu::BindGroupLayout,
    composite_sampler: wgpu::Sampler,
    composite_quad_vbuf: wgpu::Buffer,
    format: wgpu::TextureFormat,

    size: (u32, u32),
    // Kept alive so `texture_view` stays valid; not read directly otherwise.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    texture_view: wgpu::TextureView,
    // The solid/image pipelines actually render into this (multisampled),
    // resolving into `texture_view` — never sampled, so `msaa_texture` is
    // only kept alive for `msaa_view`'s sake.
    #[allow(dead_code)]
    msaa_texture: wgpu::Texture,
    msaa_view: wgpu::TextureView,
    screen_size_buffer: wgpu::Buffer,
    screen_size_bind_group: wgpu::BindGroup,
    composite_bind_group: wgpu::BindGroup,

    vertex_buffer: Option<wgpu::Buffer>,
    index_buffer: Option<wgpu::Buffer>,
    num_indices: u32,
    // One per `DrawCommand::Image` in `pending` — each needs its own
    // texture/bind group, so unlike the solid batch above these can't share
    // one buffer.
    image_draws: Vec<ImageDraw>,
    // `FontSystem`/`TextAtlas`/etc. are shared by every layer; the
    // `TextRenderer` below is not — see `text_stack.rs`'s module doc.
    text_stack: Rc<RefCell<TextStack>>,
    text_renderer: glyphon::TextRenderer,

    pending: Vec<DrawCommand>,
    invalidation: InvalidationState,
}

/// GPU state for one `DrawCommand::Image`, rebuilt whenever the layer
/// rebuilds (see [`LayerInner::build_buffers`]).
struct ImageDraw {
    vertex_buffer: wgpu::Buffer,
    // Kept alive so `bind_group`'s texture view stays valid; not read
    // directly otherwise.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

impl LayerInner {
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipelines: &LayerPipelines,
        text_stack: Rc<RefCell<TextStack>>,
        format: wgpu::TextureFormat,
        size: (u32, u32),
        invalidation: LayerInvalidation,
    ) -> Self {
        let (texture, texture_view) = create_layer_texture(device, format, size);
        let (msaa_texture, msaa_view) = create_layer_msaa_texture(device, format, size);
        let screen_size_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("atomos-layer-screen-size-ubo"),
            contents: bytemuck::bytes_of(&ScreenSize {
                screen_size: [size.0 as f32, size.1 as f32],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let screen_size_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atomos-layer-screen-size-bind-group"),
            layout: &pipelines.screen_size_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: screen_size_buffer.as_entire_binding(),
            }],
        });
        let composite_bind_group = create_composite_bind_group(
            device,
            &pipelines.composite_bind_group_layout,
            &pipelines.composite_sampler,
            &texture_view,
        );

        let invalidation_state = match invalidation {
            LayerInvalidation::Automatic => InvalidationState::Automatic { last_hash: None },
            LayerInvalidation::Manual => InvalidationState::Manual { dirty: false },
        };

        // Own `TextRenderer` per layer, registered against the shared
        // `TextAtlas` — see `TextStack::create_text_renderer`'s doc for why
        // sharing one `TextRenderer` across layers is unsound.
        let text_renderer = text_stack.borrow_mut().create_text_renderer(device);

        Self {
            device: device.clone(),
            queue: queue.clone(),
            solid_pipeline: pipelines.solid_pipeline.clone(),
            image_pipeline: pipelines.image_pipeline.clone(),
            composite_pipeline: pipelines.composite_pipeline.clone(),
            composite_bind_group_layout: pipelines.composite_bind_group_layout.clone(),
            composite_sampler: pipelines.composite_sampler.clone(),
            composite_quad_vbuf: pipelines.composite_quad_vbuf.clone(),
            format,
            size,
            texture,
            texture_view,
            msaa_texture,
            msaa_view,
            screen_size_buffer,
            screen_size_bind_group,
            composite_bind_group,
            vertex_buffer: None,
            index_buffer: None,
            num_indices: 0,
            image_draws: Vec::new(),
            text_stack,
            text_renderer,
            pending: Vec::new(),
            invalidation: invalidation_state,
        }
    }

    fn push(&mut self, command: DrawCommand) {
        match &mut self.invalidation {
            InvalidationState::Automatic { .. } => self.pending.push(command),
            InvalidationState::Manual { dirty } => {
                self.pending.push(command);
                *dirty = true;
            }
        }
    }

    // The layer spike's one `Manual` layer never redraws, so nothing calls
    // this yet — real `Manual`-layer callers are the next pass's work.
    #[allow(dead_code)]
    fn clear(&mut self) {
        self.pending.clear();
        if let InvalidationState::Manual { dirty } = &mut self.invalidation {
            *dirty = true;
        }
    }

    /// Resize the offscreen texture (and everything that references it) to
    /// match the current surface size. Cached vertex/index data is left
    /// alone — `DrawCommand` positions are already absolute pixel
    /// coordinates, so they stay correct on a resized canvas without
    /// re-tessellating.
    fn resize_if_needed(&mut self, size: (u32, u32)) {
        if size == self.size {
            return;
        }
        self.size = size;
        let (texture, texture_view) = create_layer_texture(&self.device, self.format, size);
        let (msaa_texture, msaa_view) = create_layer_msaa_texture(&self.device, self.format, size);
        self.queue.write_buffer(
            &self.screen_size_buffer,
            0,
            bytemuck::bytes_of(&ScreenSize {
                screen_size: [size.0 as f32, size.1 as f32],
            }),
        );
        self.composite_bind_group = create_composite_bind_group(
            &self.device,
            &self.composite_bind_group_layout,
            &self.composite_sampler,
            &texture_view,
        );
        self.texture = texture;
        self.texture_view = texture_view;
        self.msaa_texture = msaa_texture;
        self.msaa_view = msaa_view;
        // Shared across every layer, so this runs redundantly once per
        // layer on an actual resize — harmless, it's just a small
        // `Resolution` update.
        self.text_stack.borrow_mut().resize(&self.queue, size);
    }

    /// Rebuild the GPU vertex/index buffers from `pending` if this layer's
    /// `LayerInvalidation` rule says the content actually changed, then
    /// reset that rule's state for the next frame. Returns whether a
    /// rebuild happened (useful for smoke-testing that unchanged
    /// `Automatic` layers really do skip work).
    fn rebuild_if_needed(&mut self) -> bool {
        let should_rebuild = match &mut self.invalidation {
            InvalidationState::Automatic { last_hash } => {
                let mut hasher = DefaultHasher::new();
                for command in &self.pending {
                    command.hash(&mut hasher);
                }
                let new_hash = hasher.finish();
                let changed = *last_hash != Some(new_hash);
                *last_hash = Some(new_hash);
                changed
            }
            InvalidationState::Manual { dirty } => std::mem::take(dirty),
        };

        if should_rebuild {
            self.build_buffers();
        }

        // Text always re-shapes/re-prepares regardless of `should_rebuild`
        // — see `text_stack.rs`'s module doc for why — and must happen
        // before `pending` is cleared below (an `Automatic` layer's text
        // commands wouldn't survive to `render_to_texture` otherwise).
        if let Err(e) = self.text_stack.borrow_mut().prepare_layer(
            &self.device,
            &self.queue,
            &self.pending,
            &mut self.text_renderer,
        ) {
            eprintln!("Atomos: text glyph atlas full, some text may not render this frame: {e:?}");
        }

        // `Automatic` layers expect a full re-description of their content
        // every frame; `Manual` layers persist theirs until `clear()`.
        if matches!(self.invalidation, InvalidationState::Automatic { .. }) {
            self.pending.clear();
        }

        should_rebuild
    }

    fn build_buffers(&mut self) {
        let mut vertices: Vec<Vertex> = Vec::new();
        let mut indices: Vec<u16> = Vec::new();
        self.image_draws.clear();
        for command in &self.pending {
            // `Image` needs its own texture/bind group, not a shared
            // vertex/index buffer — built separately below.
            if let DrawCommand::Image { pixels, top_left, size, tint } = command {
                self.image_draws.push(build_image_draw(
                    &self.device,
                    &self.queue,
                    &self.composite_bind_group_layout,
                    &self.composite_sampler,
                    pixels,
                    *top_left,
                    *size,
                    tint,
                ));
                continue;
            }
            let (mut cmd_vertices, cmd_indices) = tessellate_command(command);
            let base = vertices.len() as u16;
            vertices.append(&mut cmd_vertices);
            indices.extend(cmd_indices.into_iter().map(|i| i + base));
        }

        self.num_indices = indices.len() as u32;
        self.vertex_buffer = if vertices.is_empty() {
            None
        } else {
            Some(
                self.device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("atomos-layer-vbuf"),
                        contents: bytemuck::cast_slice(&vertices),
                        usage: wgpu::BufferUsages::VERTEX,
                    }),
            )
        };
        self.index_buffer = if indices.is_empty() {
            None
        } else {
            Some(
                self.device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("atomos-layer-ibuf"),
                        contents: bytemuck::cast_slice(&indices),
                        usage: wgpu::BufferUsages::INDEX,
                    }),
            )
        };
    }

    /// Render this layer's cached geometry into its own offscreen texture.
    /// A separate render pass from both other layers and the main
    /// swap-chain pass — cheap to skip entirely for layers with no
    /// geometry (an empty `pending` still needs its texture cleared to
    /// transparent so it doesn't keep showing stale content).
    fn render_to_texture(&self, encoder: &mut wgpu::CommandEncoder) {
        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("atomos-layer-pass"),
            // Render into the multisampled target; the GPU resolves it into
            // `texture_view` automatically. The multisampled contents
            // themselves are never read again (cleared fresh every
            // rebuild), so `Discard` rather than `Store`.
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.msaa_view,
                depth_slice: None,
                resolve_target: Some(&self.texture_view),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Discard,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        if let (Some(vbuf), Some(ibuf)) = (&self.vertex_buffer, &self.index_buffer) {
            rpass.set_pipeline(&self.solid_pipeline);
            rpass.set_bind_group(0, &self.screen_size_bind_group, &[]);
            rpass.set_vertex_buffer(0, vbuf.slice(..));
            rpass.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint16);
            rpass.draw_indexed(0..self.num_indices, 0, 0..1);
        }

        if !self.image_draws.is_empty() {
            rpass.set_pipeline(&self.image_pipeline);
            rpass.set_bind_group(0, &self.screen_size_bind_group, &[]);
            for image_draw in &self.image_draws {
                rpass.set_bind_group(1, &image_draw.bind_group, &[]);
                rpass.set_vertex_buffer(0, image_draw.vertex_buffer.slice(..));
                rpass.draw(0..6, 0..1);
            }
        }

        // Drawn last, on top of this layer's shapes/images — whatever
        // `prepare_layer` uploaded moments ago in `rebuild_if_needed`.
        if let Err(e) = self.text_stack.borrow().render(&self.text_renderer, &mut rpass) {
            eprintln!("Atomos: text render error: {e:?}");
        }
    }

    /// Composite this layer's already-rendered texture onto `view`
    /// (expected to already contain everything drawn below it — the caller
    /// is responsible for `LoadOp::Load` and drawing layers in
    /// bottom-to-top order).
    fn composite(&self, rpass: &mut wgpu::RenderPass<'_>) {
        rpass.set_pipeline(&self.composite_pipeline);
        rpass.set_bind_group(0, &self.composite_bind_group, &[]);
        rpass.set_vertex_buffer(0, self.composite_quad_vbuf.slice(..));
        rpass.draw(0..6, 0..1);
    }
}

fn create_layer_texture(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    (width, height): (u32, u32),
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("atomos-layer-texture"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// Build the offscreen multisampled colour texture the solid/image
/// pipelines actually render into — resolved into the layer's real
/// (single-sample) texture at the end of the pass. Never sampled — only
/// `RENDER_ATTACHMENT` usage — mirroring the main swap-chain's old MSAA
/// target.
fn create_layer_msaa_texture(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    (width, height): (u32, u32),
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("atomos-layer-msaa-texture"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: MSAA_SAMPLE_COUNT,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// Upload one `DrawCommand::Image`'s pixels as a texture and build the
/// [`ImageDraw`] that draws it: a 6-vertex quad (pixel-space `top_left` to
/// `top_left + size`, no index buffer) with `tint` resolved per-corner via
/// [`Color::resolve`] against that same rectangle.
///
/// Eight params, all distinct types except `top_left`/`size` — a params
/// struct here would exist for this one private function alone, so the
/// lint is silenced rather than worked around (same call this codebase
/// already made for the old `build_image_handle_rgba`).
#[allow(clippy::too_many_arguments)]
fn build_image_draw(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture_bind_group_layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    pixels: &Pixels,
    top_left: [f32; 2],
    size: [f32; 2],
    tint: &Color,
) -> ImageDraw {
    let extent = wgpu::Extent3d {
        width: pixels.width.max(1),
        height: pixels.height.max(1),
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("atomos-layer-image-texture"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels.data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * pixels.width.max(1)),
            rows_per_image: Some(pixels.height.max(1)),
        },
        extent,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = create_composite_bind_group(device, texture_bind_group_layout, sampler, &view);

    let min = top_left;
    let max = [top_left[0] + size[0], top_left[1] + size[1]];
    let corner = |x: f32, y: f32, u: f32, v: f32| ImageVertex {
        position: [x, y],
        uv: [u, v],
        tint: tint.resolve([x, y], min, max),
    };
    let tl = corner(min[0], min[1], 0.0, 0.0);
    let tr = corner(max[0], min[1], 1.0, 0.0);
    let br = corner(max[0], max[1], 1.0, 1.0);
    let bl = corner(min[0], max[1], 0.0, 1.0);
    let quad = [tl, tr, br, tl, br, bl];
    let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("atomos-layer-image-vbuf"),
        contents: bytemuck::cast_slice(&quad),
        usage: wgpu::BufferUsages::VERTEX,
    });

    ImageDraw { vertex_buffer, texture, bind_group }
}

fn create_composite_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    texture_view: &wgpu::TextureView,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("atomos-layer-composite-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(texture_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Simple vertex constructor for lyon's fill tessellator that resolves each
/// output vertex's color from its position, via [`Color::resolve`] — see
/// that method for why position (rather than original-vertex-index) is
/// what colors get resolved from.
struct ColorAtPosition<'a> {
    color: &'a Color,
    min: [f32; 2],
    max: [f32; 2],
}

impl FillVertexConstructor<Vertex> for ColorAtPosition<'_> {
    fn new_vertex(&mut self, v: FillVertex) -> Vertex {
        let p = v.position();
        Vertex {
            position: [p.x, p.y],
            color: self.color.resolve([p.x, p.y], self.min, self.max),
        }
    }
}

fn tessellate_command(command: &DrawCommand) -> (Vec<Vertex>, Vec<u16>) {
    match command {
        DrawCommand::Rectangle {
            top_left,
            size,
            color,
            rounding,
        } => tessellate_rectangle(*top_left, *size, color, rounding),
        DrawCommand::Polygon { vertices, color } => tessellate_polygon(vertices, color),
        DrawCommand::Circle { center, radius, color } => tessellate_circle(*center, *radius, color),
        DrawCommand::Path { d, position, color } => tessellate_svg_path(d, *position, color),
        // Handled directly in `LayerInner::build_buffers`, which needs its
        // own texture per image rather than a shared vertex/index buffer
        // — filtered out before ever reaching here.
        DrawCommand::Image { .. } => (Vec::new(), Vec::new()),
        // Handled entirely in `LayerInner::rebuild_if_needed`/
        // `render_to_texture` via the shared `TextStack`, not the solid
        // vertex/index batch.
        DrawCommand::Text { .. } | DrawCommand::StyledText { .. } => (Vec::new(), Vec::new()),
    }
}

/// Parse an SVG path `d` string into a lyon [`Path`]. Not a full SVG
/// document parser — one path's worth of `d` syntax (`M`/`L`/`C`/`Z`/etc.),
/// authored in the path's own local units.
pub(super) fn parse_svg_path(d: &str) -> Result<Path, lyon_extra::parser::ParseError> {
    let options = ParserOptions::DEFAULT;
    let mut parser = PathParser::new();
    let mut builder = Path::builder();
    let mut src = Source::new(d.chars());
    parser.parse(&options, &mut src, &mut builder)?;
    Ok(builder.build())
}

/// Approximate bounding box of a path's own local-space geometry, scanning
/// every endpoint and control point. Slightly looser than the tightest
/// possible fit for curved segments (control points can lie outside the
/// curve itself) — fine for [`Color::Gradient`] resolution, which doesn't
/// need pixel-perfect bounds.
fn path_bounding_box(path: &Path) -> ([f32; 2], [f32; 2]) {
    let mut min = [f32::MAX, f32::MAX];
    let mut max = [f32::MIN, f32::MIN];
    let mut visit = |p: lyon::math::Point| {
        min[0] = min[0].min(p.x);
        min[1] = min[1].min(p.y);
        max[0] = max[0].max(p.x);
        max[1] = max[1].max(p.y);
    };
    for event in path.iter() {
        match event {
            path::Event::Begin { at } => visit(at),
            path::Event::Line { from, to } => {
                visit(from);
                visit(to);
            }
            path::Event::Quadratic { from, ctrl, to } => {
                visit(from);
                visit(ctrl);
                visit(to);
            }
            path::Event::Cubic { from, ctrl1, ctrl2, to } => {
                visit(from);
                visit(ctrl1);
                visit(ctrl2);
                visit(to);
            }
            path::Event::End { last, first, .. } => {
                visit(last);
                visit(first);
            }
        }
    }
    if min[0] > max[0] {
        min = [0.0, 0.0];
        max = [0.0, 0.0];
    }
    (min, max)
}

/// Tessellate an SVG `d` string, re-parsed here (see [`DrawCommand::Path`]
/// for why): resolves each vertex's color against the path's own local
/// bounding box, then translates every vertex by `position` — `position` is
/// a plain offset, not a scale, so the path's authored units become pixels
/// directly.
fn tessellate_svg_path(d: &str, position: [f32; 2], color: &Color) -> (Vec<Vertex>, Vec<u16>) {
    let path = parse_svg_path(d)
        .expect("Layer::draw_path already validated this string's syntax when it was queued");
    let (min, max) = path_bounding_box(&path);
    let (mut vertices, indices) = tessellate_filled(&path, color, min, max);
    for vertex in &mut vertices {
        vertex.position[0] += position[0];
        vertex.position[1] += position[1];
    }
    (vertices, indices)
}

/// Fill an already-built lyon `Path` and resolve each output vertex's color
/// via [`Color::resolve`] against the shape's own bounding box (`min`,
/// `max`) — shared by every `DrawCommand`'s tessellation, only the path
/// construction differs.
fn tessellate_filled(path: &Path, color: &Color, min: [f32; 2], max: [f32; 2]) -> (Vec<Vertex>, Vec<u16>) {
    let mut tessellator = FillTessellator::new();
    let mut buffers: VertexBuffers<Vertex, u16> = VertexBuffers::new();
    let ctor = ColorAtPosition { color, min, max };
    let mut buf_builder = BuffersBuilder::new(&mut buffers, ctor);
    // Tolerance is in pixels here (unlike the old NDC-scale tessellation
    // this replaced), so a value much larger than a NDC-tuned `0.0005`
    // still looks smooth at UI scale.
    let fill_opts = FillOptions::DEFAULT.with_tolerance(0.1);
    tessellator
        .tessellate_path(path, &fill_opts, &mut buf_builder)
        .expect("fill tessellation should never fail for a valid path");
    (buffers.vertices, buffers.indices)
}

fn tessellate_rectangle(
    top_left: [f32; 2],
    size: [f32; 2],
    color: &Color,
    rounding: &Rounding,
) -> (Vec<Vertex>, Vec<u16>) {
    let min = path::geom::point(top_left[0], top_left[1]);
    let max = path::geom::point(top_left[0] + size[0], top_left[1] + size[1]);

    let mut builder = Path::builder();
    let bbox = path::geom::Box2D { min, max };
    if rounding.is_none() {
        builder.add_rectangle(&bbox, path::Winding::Positive);
    } else {
        builder.add_rounded_rectangle(
            &bbox,
            &BorderRadii {
                top_left: rounding.top_left,
                top_right: rounding.top_right,
                bottom_left: rounding.bottom_left,
                bottom_right: rounding.bottom_right,
            },
            path::Winding::Positive,
        );
    }

    tessellate_filled(
        &builder.build(),
        color,
        top_left,
        [top_left[0] + size[0], top_left[1] + size[1]],
    )
}

/// Bounding box of a vertex list. Empty input degenerates to a zero-sized
/// box at the origin — harmless, since an empty polygon tessellates to no
/// geometry anyway.
fn bounding_box(vertices: &[[f32; 2]]) -> ([f32; 2], [f32; 2]) {
    let mut min = [f32::MAX, f32::MAX];
    let mut max = [f32::MIN, f32::MIN];
    for v in vertices {
        min[0] = min[0].min(v[0]);
        min[1] = min[1].min(v[1]);
        max[0] = max[0].max(v[0]);
        max[1] = max[1].max(v[1]);
    }
    if vertices.is_empty() {
        min = [0.0, 0.0];
        max = [0.0, 0.0];
    }
    (min, max)
}

fn tessellate_polygon(vertices: &[[f32; 2]], color: &Color) -> (Vec<Vertex>, Vec<u16>) {
    let mut builder = Path::builder();
    let mut points = vertices.iter();
    if let Some(first) = points.next() {
        builder.begin(path::geom::point(first[0], first[1]));
        for v in points {
            builder.line_to(path::geom::point(v[0], v[1]));
        }
        builder.end(true);
    }

    let (min, max) = bounding_box(vertices);
    tessellate_filled(&builder.build(), color, min, max)
}

fn tessellate_circle(center: [f32; 2], radius: f32, color: &Color) -> (Vec<Vertex>, Vec<u16>) {
    let mut builder = Path::builder();
    builder.add_circle(
        path::geom::point(center[0], center[1]),
        radius,
        path::Winding::Positive,
    );

    let min = [center[0] - radius, center[1] - radius];
    let max = [center[0] + radius, center[1] + radius];
    tessellate_filled(&builder.build(), color, min, max)
}

/// A retained drawing surface with its own offscreen texture and pending
/// draw-command queue. See the module doc for the full design.
///
/// Cloning a `Layer` is cheap and shares the same underlying content — both
/// clones draw into (and see) the same layer. Create one with
/// [`super::Renderer::new_layer_top`] or
/// [`super::Renderer::new_layer_bottom`].
#[derive(Clone)]
pub struct Layer(pub(super) Rc<RefCell<LayerInner>>);

impl Layer {
    pub(super) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipelines: &LayerPipelines,
        text_stack: Rc<RefCell<TextStack>>,
        format: wgpu::TextureFormat,
        size: (u32, u32),
        invalidation: LayerInvalidation,
    ) -> Self {
        Self(Rc::new(RefCell::new(LayerInner::new(
            device,
            queue,
            pipelines,
            text_stack,
            format,
            size,
            invalidation,
        ))))
    }

    /// Draw a filled, optionally-rounded rectangle.
    ///
    /// - `top_left`: pixel position of the rectangle's top-left corner.
    /// - `size`: `(width, height)` in pixels.
    /// - `color`: fill color — any [`Color`] variant is accepted; see
    ///   [`Color`]'s docs for how each variant is resolved across the
    ///   rectangle's area.
    /// - `rounding`: per-corner radii; use [`Rounding::NONE`] for a sharp
    ///   rectangle.
    pub fn draw_rectangle(&self, top_left: (f32, f32), size: (f32, f32), color: Color, rounding: Rounding) {
        self.0.borrow_mut().push(DrawCommand::Rectangle {
            top_left: [top_left.0, top_left.1],
            size: [size.0, size.1],
            color,
            rounding,
        });
    }

    /// Draw a filled polygon from an arbitrary list of pixel-space vertices,
    /// in order. Implicitly closed — the last vertex connects back to the
    /// first, so callers don't repeat the starting point.
    ///
    /// `color`: any [`Color`] variant is accepted. [`Color::PerVertex`]
    /// currently resolves against the polygon's own bounding-box corners
    /// (the same rule [`Layer::draw_rectangle`] uses), not literally one
    /// color per input vertex — see the layer-abstraction plan's
    /// deferred-work note on why (lyon vertex-identity guarantees for
    /// simple polygons need a closer look first).
    pub fn draw_polygon(&self, vertices: &[(f32, f32)], color: Color) {
        self.0.borrow_mut().push(DrawCommand::Polygon {
            vertices: vertices.iter().map(|&(x, y)| [x, y]).collect(),
            color,
        });
    }

    /// Draw a filled circle.
    ///
    /// - `center`: pixel position of the circle's center.
    /// - `radius`: pixel radius.
    /// - `color`: only [`Color::Solid`]/[`Color::Gradient`] are accepted —
    ///   a circle has no caller-supplied vertices to pin a
    ///   [`Color::PerVertex`] corner color to.
    pub fn draw_circle(&self, center: (f32, f32), radius: f32, color: Color) {
        debug_assert!(
            !matches!(color, Color::PerVertex(_)),
            "draw_circle doesn't support Color::PerVertex — use Solid or Gradient"
        );
        self.0.borrow_mut().push(DrawCommand::Circle {
            center: [center.0, center.1],
            radius,
            color,
        });
    }

    /// Draw a filled shape parsed from an SVG-style path-data string (the
    /// same syntax as an SVG `<path d="...">` attribute: `M`/`L`/`C`/`Q`/
    /// `Z`/etc., relative or absolute). Not a full SVG document parser —
    /// one path, one color.
    ///
    /// - `d`: the path-data string, authored in whatever local unit the
    ///   path was designed in (e.g. a 24x24 icon viewBox).
    /// - `position`: a pixel-space *offset* added to the path's own local
    ///   coordinates — not a scale, so the path's authored units become
    ///   pixels directly. Author icons at their intended on-screen size.
    /// - `color`: only [`Color::Solid`]/[`Color::Gradient`] are accepted —
    ///   same reasoning as [`Layer::draw_circle`].
    ///
    /// Returns an error immediately if `d` doesn't parse, rather than
    /// silently failing later when the layer happens to rebuild.
    pub fn draw_path(
        &self,
        d: &str,
        position: (f32, f32),
        color: Color,
    ) -> Result<(), lyon_extra::parser::ParseError> {
        debug_assert!(
            !matches!(color, Color::PerVertex(_)),
            "draw_path doesn't support Color::PerVertex — use Solid or Gradient"
        );
        parse_svg_path(d)?;
        self.0.borrow_mut().push(DrawCommand::Path {
            d: d.to_string(),
            position: [position.0, position.1],
            color,
        });
        Ok(())
    }

    /// [`Layer::draw_path`], reading the path-data string from a file
    /// (e.g. a `.svg`'s single `<path d="...">` value saved to its own
    /// file) instead of an in-memory string.
    pub fn draw_path_from_file(
        &self,
        path: &std::path::Path,
        position: (f32, f32),
        color: Color,
    ) -> Result<(), PathFileError> {
        let d = std::fs::read_to_string(path)?;
        self.draw_path(&d, position, color)?;
        Ok(())
    }

    /// Draw a raw RGBA8 pixel buffer, stretched to `size`.
    ///
    /// - `pixels`: the raw image data (see [`Pixels`]), taken by value —
    ///   clone it yourself if you need to draw the same image more than
    ///   once.
    /// - `top_left`: pixel position of the image's top-left corner.
    /// - `size`: `(width, height)` in pixels — independent, unlike
    ///   [`Layer::draw_path`]'s uniform positioning, since images often
    ///   need non-uniform stretching.
    /// - `tint`: multiplies the sampled image color; any [`Color`] variant
    ///   is accepted — [`Color::PerVertex`] resolves one color per image
    ///   corner, which (unlike [`Layer::draw_circle`]/[`Layer::draw_path`])
    ///   is well-defined here since an image is already exactly a
    ///   4-cornered quad. [`Color::rgb(0xFF, 0xFF, 0xFF)`](Color::rgb) (or
    ///   any full-white solid color) leaves the image untinted.
    pub fn draw_image(&self, pixels: Pixels, top_left: (f32, f32), size: (f32, f32), tint: Color) {
        self.0.borrow_mut().push(DrawCommand::Image {
            pixels,
            top_left: [top_left.0, top_left.1],
            size: [size.0, size.1],
            tint,
        });
    }

    /// [`Layer::draw_image`], decoding the pixels from an image file (PNG,
    /// JPEG, or anything else the `image` crate supports) instead of a raw
    /// [`Pixels`] buffer already in memory.
    ///
    /// Unexercised by the current demo (no bundled image asset to point it
    /// at yet) — the real entry point for a future caller, e.g. a
    /// file-tree icon loaded from disk.
    #[allow(dead_code)]
    pub fn draw_image_from_file(
        &self,
        path: &std::path::Path,
        top_left: (f32, f32),
        size: (f32, f32),
        tint: Color,
    ) -> Result<(), image::ImageError> {
        let decoded = image::open(path)?.to_rgba8();
        let pixels = Pixels::new(decoded.width(), decoded.height(), decoded.into_raw());
        self.draw_image(pixels, top_left, size, tint);
        Ok(())
    }

    /// Draw a string of text.
    ///
    /// - `text`: the string to draw.
    /// - `position`: a pixel-space anchor point — which part of the text's
    ///   own bounding box it anchors is decided by `alignment`, not always
    ///   the top-left corner (see [`Alignment`]).
    /// - `color`: only [`Color::Solid`] is accepted — per-glyph color would
    ///   need cosmic-text's rich-text spans, a bigger feature than this
    ///   covers.
    /// - `alignment`: which point of the text's bounding box `position`
    ///   represents.
    /// - `font`: which font to shape/rasterize with.
    /// - `font_parameters`: size, continuous faux-bold/condense, and
    ///   tracking — see [`FontParameters`].
    pub fn draw_text(
        &self,
        text: &str,
        position: (f32, f32),
        color: Color,
        alignment: Alignment,
        font: Font,
        font_parameters: FontParameters,
    ) {
        debug_assert!(
            matches!(color, Color::Solid(_)),
            "draw_text only supports Color::Solid"
        );
        self.0.borrow_mut().push(DrawCommand::Text {
            text: text.to_string(),
            position: [position.0, position.1],
            font,
            font_parameters,
            alignment,
            color,
        });
    }

    /// Measure the pixel size `text` would occupy shaped with `font`/
    /// `font_parameters`, before any [`Alignment`] offset is applied — the
    /// same measurement [`Layer::draw_text`] uses internally to resolve
    /// alignment. Exposed so callers can do their own layout (e.g. sizing
    /// a background box to fit a label) without duplicating text-shaping
    /// logic themselves.
    pub fn get_text_size(&self, text: &str, font: &Font, font_parameters: &FontParameters) -> (f32, f32) {
        self.0.borrow().text_stack.borrow_mut().measure(text, font, font_parameters)
    }

    /// Draw multiple independently-styled runs of text as one logical
    /// unit — e.g. one syntax-highlighted line, one [`TextSpan`] per
    /// token. Every span is shaped together as a single cosmic-text
    /// rich-text buffer, so this costs one shape operation regardless of
    /// how many spans/colors are involved — unlike calling
    /// [`Layer::draw_text`] once per token, which pays full shaping cost
    /// per call.
    ///
    /// - `spans`: the styled runs, concatenated in order.
    /// - `position`/`alignment`: apply to the *combined* bounding box of
    ///   every span together, the same way [`Layer::draw_text`]'s do for
    ///   its one string.
    pub fn draw_styled_text(&self, spans: &[TextSpan], position: (f32, f32), alignment: Alignment) {
        debug_assert!(
            spans.iter().all(|span| matches!(span.color, Color::Solid(_))),
            "draw_styled_text only supports Color::Solid per span"
        );
        self.0.borrow_mut().push(DrawCommand::StyledText {
            spans: spans.to_vec(),
            position: [position.0, position.1],
            alignment,
        });
    }

    /// [`Layer::get_text_size`], for a [`Layer::draw_styled_text`] call —
    /// the combined bounding box every span in `spans` would occupy
    /// shaped together, before any [`Alignment`] offset.
    pub fn get_styled_text_size(&self, spans: &[TextSpan]) -> (f32, f32) {
        self.0.borrow().text_stack.borrow_mut().measure_spans(spans)
    }

    /// Discard every pending/previously-built draw command for this layer.
    /// Mainly meaningful for [`LayerInvalidation::Manual`] layers (call
    /// this before re-describing content that changed); harmless on
    /// [`LayerInvalidation::Automatic`] layers, which already clear their
    /// own pending content every rebuilt frame.
    // Not called by the layer spike (its one `Manual` layer draws once and
    // never redraws) — kept as the real API `Manual`-layer callers need.
    #[allow(dead_code)]
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }
}

/// Advance one frame for every live layer in `order` (front = bottom of the
/// visual stack, back = top): resize/rebuild as needed, render each into
/// its own texture, then composite all of them onto `view` in the same
/// order. `view` is expected to already hold whatever was drawn below the
/// layer stack (this pass uses `LoadOp::Load`).
pub(super) fn render_layers(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    order: &mut std::collections::VecDeque<std::rc::Weak<RefCell<LayerInner>>>,
    surface_size: (u32, u32),
) {
    order.retain(|weak| weak.strong_count() > 0);
    let live: Vec<Rc<RefCell<LayerInner>>> = order.iter().filter_map(|weak| weak.upgrade()).collect();

    for inner in &live {
        let mut inner = inner.borrow_mut();
        inner.resize_if_needed(surface_size);
        inner.rebuild_if_needed();
        inner.render_to_texture(encoder);
    }

    if live.is_empty() {
        return;
    }

    let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("atomos-layer-composite-pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    for inner in &live {
        inner.borrow().composite(&mut rpass);
    }
}
