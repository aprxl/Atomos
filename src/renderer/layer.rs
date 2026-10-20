//! [`Layer`] — a retained, z-ordered drawing unit.
//!
//! A `Layer` holds a queue of pending [`DrawCommand`]s in logical pixels.
//! Calling a `draw_*` method never touches the GPU directly — it only
//! records a command and marks the layer dirty; the actual tessellation,
//! upload and text preparation happen lazily inside
//! [`super::Renderer::render`].
//!
//! Commands paint in the order they were issued, whatever their kind: a
//! later shape covers earlier text and images. Layers stack bottom to top.
//!
//! By default a layer owns no render target. Its retained geometry is
//! replayed straight into the frame's single multisampled pass, so a plain
//! layer costs no texture memory. Only a layer that needs its pixels
//! isolated — it has a [`ShaderEffect`], or asked for
//! [`CachePolicy::Always`] — renders into an offscreen texture, which is
//! retained until the layer changes and then composited as one quad.
//!
//! Clipping is a scope, not a surface: [`Layer::with_clip`] clips what a
//! closure draws, [`Layer::set_clip`] clips the whole layer (after any
//! effect). Pixel-aligned rectangles are hardware scissors; every other
//! shape uses one shared stencil buffer.
//!
//! `Layer` is a cheap-to-clone handle (`Rc<RefCell<LayerInner>>`); dropping
//! every clone of a given layer removes it from the renderer's composite
//! stack on the next frame (the registry holds only a `Weak`).

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use wgpu::util::DeviceExt;

use lyon::path::{self, Path, builder::BorderRadii};
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, FillVertexConstructor, VertexBuffers,
};
use lyon_extra::parser::{ParserOptions, PathParser, Source};

use super::color::Color;
use super::draw_command::DrawCommand;
use super::path_paint::{FillRule, LineCap, LineJoin, PathPaint, Stroke};
use super::pixels::Pixels;
use super::rounding::Rounding;
use super::shader::ShaderEffect;
use super::stats::RenderStats;
use super::text_stack::TextStack;
use super::{Alignment, Font, FontParameters, TextLayout, TextSpan, Vertex};

use lyon::tessellation::{
    FillRule as LyonFillRule, LineCap as LyonLineCap, LineJoin as LyonLineJoin, StrokeOptions,
    StrokeTessellator, StrokeVertex, StrokeVertexConstructor,
};

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

/// An arbitrary clip shape for [`Layer::with_clip`] and [`Layer::set_clip`]:
/// content only shows where the shape is. All coordinates are logical
/// pixels, the same convention as every `draw_*` call.
///
/// Variants mirror the solid-geometry `draw_*` calls (rectangle including
/// rounded corners, circle, polygon, SVG path), and are tessellated by the
/// exact same code — anything you can draw, you can clip by. Edges are
/// antialiased for free by the same MSAA resolve the drawn geometry gets.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipShape {
    /// Axis-aligned rectangle, optionally rounded. For an *unrounded*
    /// rectangle on whole pixels this is a free hardware scissor; any other
    /// shape uses the stencil.
    Rectangle {
        top_left: (f32, f32),
        size: (f32, f32),
        rounding: Rounding,
    },
    Circle {
        center: (f32, f32),
        radius: f32,
    },
    /// Implicitly closed, same as [`Layer::draw_polygon`].
    Polygon {
        vertices: Vec<(f32, f32)>,
    },
    /// SVG path-data string, same conventions as [`Layer::draw_path`]:
    /// authored units are logical pixels 1:1, `position` is an offset.
    Path {
        d: String,
        position: (f32, f32),
    },
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
    CompositeVertex {
        position: [-1.0, 1.0],
        uv: [0.0, 0.0],
    },
    CompositeVertex {
        position: [1.0, 1.0],
        uv: [1.0, 0.0],
    },
    CompositeVertex {
        position: [1.0, -1.0],
        uv: [1.0, 1.0],
    },
    CompositeVertex {
        position: [-1.0, 1.0],
        uv: [0.0, 0.0],
    },
    CompositeVertex {
        position: [1.0, -1.0],
        uv: [1.0, 1.0],
    },
    CompositeVertex {
        position: [-1.0, -1.0],
        uv: [0.0, 1.0],
    },
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

/// Separable Gaussian blur — one pipeline runs both the horizontal and
/// vertical pass (see [`ShaderEffect::Blur`]), `params.direction` picking
/// the axis. Reads/writes single-sample textures (no MSAA — there's no
/// rasterized-edge aliasing to smooth in a fullscreen texture-sampling
/// pass), so `textureSampleLevel` at a fixed LOD rather than `textureSample`
/// (which would need uniform control flow for its implicit derivatives —
/// not worth relying on for a dynamically-bounded tap loop when an explicit
/// LOD is both simpler and exactly what a single-mip render target needs;
/// glyphon's own shader makes the same choice — see its `shader.wgsl`).
const BLUR_SHADER_SRC: &str = r#"
struct BlurParams {
    direction: vec2<f32>,
    texel_size: vec2<f32>,
    radius: f32,
};
@group(0) @binding(0) var<uniform> params: BlurParams;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

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

const MAX_TAPS: i32 = 32;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let sigma = max(params.radius / 3.0, 0.0001);
    let taps = min(i32(ceil(params.radius)), MAX_TAPS);
    var total_weight = 0.0;
    var color = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    for (var i = -taps; i <= taps; i = i + 1) {
        let offset = f32(i) * params.texel_size * params.direction;
        let weight = exp(-f32(i * i) / (2.0 * sigma * sigma));
        color = color + textureSampleLevel(tex, samp, in.uv + offset, 0.0) * weight;
        total_weight = total_weight + weight;
    }
    return color / max(total_weight, 0.0001);
}
"#;

/// A 4x5 color transform applied per pixel — see [`ColorMatrix`] for the
/// column-vector layout `mat.cols` expects (a transpose of `ColorMatrix`'s
/// public row-major form, done once on the CPU side at `set_effect` time —
/// see `ColorMatrix::to_columns`).
const COLORMATRIX_SHADER_SRC: &str = r#"
struct ColorMatrix {
    cols: array<vec4<f32>, 5>,
};
@group(0) @binding(0) var<uniform> mat: ColorMatrix;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Layer textures are premultiplied, but the matrix is defined on
    // straight (unpremultiplied) linear-light RGBA, so offsets and
    // alpha-changing rows mean what they say. Un-premultiply, transform,
    // clamp, and premultiply again so the output is a valid premultiplied
    // texel: transparent input with an additive offset (invert) stays
    // invisible instead of adding colour.
    let p = textureSampleLevel(tex, samp, in.uv, 0.0);
    var c = vec4<f32>(0.0);
    if (p.a > 0.0) {
        c = vec4<f32>(p.rgb / p.a, p.a);
    }
    let result = clamp(
        mat.cols[0] * c.r + mat.cols[1] * c.g + mat.cols[2] * c.b + mat.cols[3] * c.a + mat.cols[4],
        vec4<f32>(0.0),
        vec4<f32>(1.0),
    );
    return vec4<f32>(result.rgb * result.a, result.a);
}
"#;

/// Uniform buffer layout for [`BLUR_SHADER_SRC`]'s `BlurParams`. wgpu pads
/// a uniform struct's size to its largest member's alignment (`vec2<f32>`,
/// align 8) — 20 bytes rounds up to 24, so `_pad` makes that explicit on
/// the Rust side rather than relying on implicit trailing padding.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct BlurParamsUniform {
    direction: [f32; 2],
    texel_size: [f32; 2],
    radius: f32,
    _pad: f32,
}

/// Uniform buffer layout for [`COLORMATRIX_SHADER_SRC`]'s `ColorMatrix` —
/// matches [`ColorMatrix::to_columns`]'s output exactly.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct ColorMatrixUniform {
    cols: [[f32; 4]; 5],
}

/// Every GPU resource shared by all layers: the two pipelines and their
/// bind group layouts, plus the one composite quad every layer's composite
/// draw reuses. Built once in [`super::Renderer::new`], cloned (cheaply —
/// every field here is an `Arc`-backed wgpu handle) into each
/// [`LayerInner`].
pub(super) struct LayerPipelines {
    solid_pipeline: wgpu::RenderPipeline,
    stencil_incr_pipeline: wgpu::RenderPipeline,
    stencil_decr_pipeline: wgpu::RenderPipeline,
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
    // `ShaderEffect` pipelines — see `shader.rs`'s module doc. Both are
    // single-bind-group (uniform + texture + sampler) fullscreen-quad
    // passes, built once and shared the same way the three pipelines above
    // are; each `LayerInner` using an effect builds its own bind
    // group/uniform buffer against these shared pipeline objects.
    blur_pipeline: wgpu::RenderPipeline,
    blur_bind_group_layout: wgpu::BindGroupLayout,
    colormatrix_pipeline: wgpu::RenderPipeline,
    colormatrix_bind_group_layout: wgpu::BindGroupLayout,
}

impl LayerPipelines {
    pub(super) fn new(
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        msaa_sample_count: u32,
    ) -> Self {
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
        let solid_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("atomos-layer-solid-pipeline-layout"),
                bind_group_layouts: &[Some(&screen_size_bind_group_layout)],
                immediate_size: 0,
            });
        // Renders into the layer's multisampled offscreen target (see
        // `msaa_sample_count`/`pick_msaa_sample_count` in `mod.rs`),
        // resolved into the layer's actual texture.
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
                    // A shape blends over what the layer already holds, the
                    // same as text and images. `REPLACE` let a translucent
                    // fill overwrite the texel, punching its alpha into an
                    // opaque card so whatever was under the layer showed
                    // through. Straight-alpha source in, premultiplied out
                    // (see `build_composite_pipeline`).
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
            depth_stencil: Some(content_stencil_state()),
            multisample: wgpu::MultisampleState {
                count: msaa_sample_count,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        let stencil_incr_pipeline = build_stencil_clip_pipeline(
            device,
            &solid_shader,
            &solid_pipeline_layout,
            surface_format,
            msaa_sample_count,
            wgpu::StencilOperation::IncrementClamp,
        );
        let stencil_decr_pipeline = build_stencil_clip_pipeline(
            device,
            &solid_shader,
            &solid_pipeline_layout,
            surface_format,
            msaa_sample_count,
            wgpu::StencilOperation::DecrementClamp,
        );

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
        let image_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
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
            depth_stencil: Some(content_stencil_state()),
            multisample: wgpu::MultisampleState {
                count: msaa_sample_count,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: None,
        });

        let composite_pipeline = build_composite_pipeline(
            device,
            surface_format,
            msaa_sample_count,
            "atomos-layer-composite",
            COMPOSITE_SHADER_SRC,
            &composite_bind_group_layout,
        );

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

        // Shared layout shape for every effect pipeline below: one uniform
        // buffer (the effect's own parameters) + a texture + a sampler,
        // all fragment-only. The uniform's exact size differs per effect
        // (`BlurParamsUniform` vs `ColorMatrixUniform`), which
        // `BindingType::Buffer`'s `min_binding_size: None` doesn't pin
        // down, so the same three entries describe both layouts.
        let effect_bind_group_layout_entries = [
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ];

        let blur_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("atomos-layer-blur-bgl"),
                entries: &effect_bind_group_layout_entries,
            });
        let blur_pipeline = build_fullscreen_pipeline(
            device,
            surface_format,
            "atomos-layer-blur",
            BLUR_SHADER_SRC,
            &blur_bind_group_layout,
        );

        let colormatrix_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("atomos-layer-colormatrix-bgl"),
                entries: &effect_bind_group_layout_entries,
            });
        let colormatrix_pipeline = build_fullscreen_pipeline(
            device,
            surface_format,
            "atomos-layer-colormatrix",
            COLORMATRIX_SHADER_SRC,
            &colormatrix_bind_group_layout,
        );

        Self {
            solid_pipeline,
            stencil_incr_pipeline,
            stencil_decr_pipeline,
            screen_size_bind_group_layout,
            image_pipeline,
            composite_pipeline,
            composite_bind_group_layout,
            composite_sampler,
            composite_quad_vbuf,
            blur_pipeline,
            blur_bind_group_layout,
            colormatrix_pipeline,
            colormatrix_bind_group_layout,
        }
    }
}

/// A pipeline that draws a clip shape into the stencil only: no colour
/// output, and `op` applied wherever the stencil equals the reference.
fn build_stencil_clip_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    surface_format: wgpu::TextureFormat,
    msaa_sample_count: u32,
    op: wgpu::StencilOperation,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("atomos-layer-stencil-clip-pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[Vertex::layout()],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: None,
                write_mask: wgpu::ColorWrites::empty(),
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: Some(clip_stencil_state(op)),
        multisample: wgpu::MultisampleState {
            count: msaa_sample_count,
            mask: !0,
            alpha_to_coverage_enabled: false,
        },
        multiview_mask: None,
        cache: None,
    })
}

/// Build one of the two composite pipelines (plain and masked): a
/// fullscreen `CompositeVertex` quad onto the swap-chain view. A layer's
/// texture is cleared to transparent, so untouched areas must let whatever
/// composited before it show through.
///
/// The blend is **premultiplied**, because a layer's texture already is:
/// everything drawn into one blends with `ALPHA_BLENDING`, which leaves
/// `rgb * a` behind, and so does the MSAA resolve. Treating that as straight
/// alpha would multiply by alpha a second time, darkening translucent fills
/// and anti-aliased edges over transparent layer.
fn build_composite_pipeline(
    device: &wgpu::Device,
    surface_format: wgpu::TextureFormat,
    msaa_sample_count: u32,
    label: &str,
    shader_src: &str,
    bind_group_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(shader_src.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[CompositeVertex::layout()],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
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
        depth_stencil: Some(content_stencil_state()),
        multisample: wgpu::MultisampleState {
            count: msaa_sample_count,
            mask: !0,
            alpha_to_coverage_enabled: false,
        },
        multiview_mask: None,
        cache: None,
    })
}

/// Build a fullscreen-quad render pipeline for a [`ShaderEffect`] pass:
/// `CompositeVertex`-shaped input (position+uv, same quad the composite
/// pipeline itself draws), one fragment-only bind group, `REPLACE` blend
/// (the pass fully overwrites its target every time — there's nothing
/// under it worth blending with), single-sample (a fullscreen texture-
/// sampling pass has no rasterized edges to anti-alias). Shared by
/// `blur_pipeline`/`colormatrix_pipeline` and, lazily per distinct source,
/// [`ShaderEffect::Custom`]'s pipeline.
fn build_fullscreen_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    label: &str,
    shader_src: &str,
    bind_group_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(shader_src.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[CompositeVertex::layout()],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
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
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// GPU state for a [`super::Layer`]'s active [`ShaderEffect`] — built once
/// when `Layer::set_effect` is called (see `LayerInner::rebuild_effect_gpu`,
/// not per-frame: the effect's own parameters don't change frame to frame,
/// only the *content* it's applied to does), then re-recorded into the
/// frame's encoder every frame by `LayerInner::apply_effect`.
struct EffectGpu {
    // Kept alive so `output_view` stays valid — same convention as
    // `LayerInner.texture`/`ImageDraw.texture`. Always the effect's
    // *final* output regardless of how many passes it took to get there —
    // `composite_bind_group` is repointed here once this exists, so
    // `LayerInner::composite` needs no effect-awareness of its own.
    #[allow(dead_code)] // owns the GPU resource its bind group points at
    output_texture: wgpu::Texture,
    output_view: wgpu::TextureView,
    kind: EffectGpuKind,
}

enum EffectGpuKind {
    /// Two passes: horizontal (`texture_view` -> `ping_view`), then
    /// vertical (`ping_view` -> `EffectGpu::output_view`). Boxed — its six
    /// fields make it far larger than the other variants, and `Option<
    /// EffectGpu>` would otherwise always pay `Blur`'s size even when
    /// holding a `ColorMatrix`/`Custom`/nothing (clippy's
    /// `large_enum_variant`).
    Blur(Box<BlurEffectGpu>),
    /// One pass: `texture_view` -> `EffectGpu::output_view`.
    ColorMatrix {
        #[allow(dead_code)] // owns the GPU resource its bind group points at
        matrix_buffer: wgpu::Buffer,
        bind_group: wgpu::BindGroup,
    },
    /// One pass, caller-supplied pipeline: `texture_view` ->
    /// `EffectGpu::output_view`. `bind_group` is shaped like
    /// `composite_bind_group` (texture + sampler, no uniform — see
    /// `ShaderEffect::Custom`'s fixed contract), built with the same
    /// `create_composite_bind_group` helper.
    Custom {
        pipeline: wgpu::RenderPipeline,
        bind_group: wgpu::BindGroup,
    },
}

/// [`EffectGpuKind::Blur`]'s GPU state, boxed out of the enum itself — see
/// that variant's doc.
struct BlurEffectGpu {
    #[allow(dead_code)] // owns the GPU resource its bind group points at
    ping_texture: wgpu::Texture,
    ping_view: wgpu::TextureView,
    #[allow(dead_code)] // owns the GPU resource its bind group points at
    params_buffer_h: wgpu::Buffer,
    #[allow(dead_code)] // owns the GPU resource its bind group points at
    params_buffer_v: wgpu::Buffer,
    bind_group_h: wgpu::BindGroup,
    bind_group_v: wgpu::BindGroup,
}

/// Bind group for the uniform+texture+sampler layout every
/// [`ShaderEffect`] built-in shares (`blur_bind_group_layout`/
/// `colormatrix_bind_group_layout` — see `LayerPipelines::new`).
fn create_effect_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniform_buffer: &wgpu::Buffer,
    texture_view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("atomos-layer-effect-bind-group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(texture_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Compile and build a pipeline for [`ShaderEffect::Custom`], guarded by a
/// wgpu error scope so invalid caller-supplied WGSL logs and falls back to
/// `None` (the layer keeps rendering without the effect) rather than
/// panicking the renderer. `pollster::block_on` here is the same choice
/// `Renderer::new` already makes for its own one-time async setup — this
/// runs only when a caller calls `Layer::set_effect`, never per-frame, so
/// a brief block is a non-issue (contrast with the per-frame timestamp
/// readback, which specifically avoids blocking for exactly that reason).
fn build_custom_effect_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    source: &str,
    bind_group_layout: &wgpu::BindGroupLayout,
) -> Option<wgpu::RenderPipeline> {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let pipeline = build_fullscreen_pipeline(
        device,
        format,
        "atomos-layer-custom-effect",
        source,
        bind_group_layout,
    );
    if let Some(error) = pollster::block_on(scope.pop()) {
        log::error!("custom shader effect failed to compile, layer left unaffected: {error}");
        return None;
    }
    Some(pipeline)
}

/// Run one fullscreen-quad effect pass: bind `pipeline`/`bind_group`, draw
/// `quad_vbuf` (the same 6-vertex clip-space quad `Layer::composite` uses)
/// into `target`. `target` is always fully overwritten (`LoadOp::Clear` is
/// just the simplest way to say "contents before this pass don't matter"),
/// matching every effect pipeline's `REPLACE` blend state.
fn run_fullscreen_pass(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    bind_group: &wgpu::BindGroup,
    target: &wgpu::TextureView,
    quad_vbuf: &wgpu::Buffer,
) {
    let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("atomos-layer-effect-pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    rpass.set_pipeline(pipeline);
    rpass.set_bind_group(0, bind_group, &[]);
    rpass.set_vertex_buffer(0, quad_vbuf.slice(..));
    rpass.draw(0..6, 0..1);
}

/// The GPU/CPU state shared by every clone of a [`Layer`]. See the module
/// doc for the overall design.
/// One step of a layer's draw order. See `LayerInner::ops`.
#[derive(Clone, Debug, PartialEq)]
enum ReplayOp {
    /// A sub-range of the layer's one solid index buffer.
    Solid(std::ops::Range<u32>),
    /// A range of `image_draws`, each with its own bind group.
    Images(std::ops::Range<usize>),
    /// Index into `text_batches`.
    Text(usize),
    /// Enter a clip that is a pixel-aligned rectangle, as `[x, y, w, h]` in
    /// physical pixels: a free hardware scissor.
    ClipScissor([u32; 4]),
    /// Enter a clip of any other shape: its geometry is this range of the
    /// solid index buffer, drawn into the stencil.
    ClipStencil(std::ops::Range<u32>),
    /// Leave the innermost clip.
    ClipPop,
}

/// The whole-layer clip resolved against the current scale and size: a
/// scissor, or geometry in buffers of its own (so changing the clip never
/// rebuilds the layer's content).
struct LayerClipGpu {
    op: ReplayOp,
    vertex_buffer: Option<wgpu::Buffer>,
    index_buffer: Option<wgpu::Buffer>,
}

/// A run of consecutive text commands with its own glyphon renderer.
struct TextBatch {
    renderer: glyphon::TextRenderer,
    /// The run's commands, in physical pixels (built against the scale
    /// factor current at the last rebuild).
    commands: Vec<DrawCommand>,
    hash: u64,
    /// `(hash, atlas generation)` as of the last successful prepare. A
    /// mismatch with either means the renderer's vertices are stale: the
    /// content changed, or the shared atlas was trimmed or reset since.
    prepared: Option<(u64, u64)>,
}

pub(super) struct LayerInner {
    device: wgpu::Device,
    queue: wgpu::Queue,
    solid_pipeline: wgpu::RenderPipeline,
    stencil_incr_pipeline: wgpu::RenderPipeline,
    stencil_decr_pipeline: wgpu::RenderPipeline,
    image_pipeline: wgpu::RenderPipeline,
    composite_pipeline: wgpu::RenderPipeline,
    composite_bind_group_layout: wgpu::BindGroupLayout,
    composite_sampler: wgpu::Sampler,
    composite_quad_vbuf: wgpu::Buffer,
    format: wgpu::TextureFormat,

    size: (u32, u32),
    // Kept alive so `texture_view` stays valid; not read directly otherwise.
    texture: wgpu::Texture,
    texture_view: wgpu::TextureView,
    // No per-layer MSAA target: layers render into the renderer's one
    // shared multisampled scratch texture (resolving into their own
    // `texture_view`), passed into `render_to_texture` each frame. Safe to
    // share because layer passes are recorded strictly sequentially and
    // the target is cleared at pass start / discarded at pass end — and it
    // saves a full-surface 4x texture (~59 MB at 1440p) per layer.
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
    // `FontSystem`/`TextAtlas`/etc. are shared by every layer; text
    // renderers are not — see `text_stack.rs`'s module doc.
    text_stack: Rc<RefCell<TextStack>>,
    // The layer's draw order: consecutive commands of one kind share an op,
    // so a later shape paints over earlier text and images, not under them.
    ops: Vec<ReplayOp>,
    // One glyphon renderer per run of consecutive text commands. glyphon
    // can only draw everything it prepared in one call, so a text run that
    // must sit between other ops needs a renderer of its own.
    text_batches: Vec<TextBatch>,

    // Shared, pre-built effect pipelines/layouts, cloned in from
    // `LayerPipelines` the same way `solid_pipeline`/`image_pipeline`/
    // `composite_pipeline` already are — see `shader.rs`'s module doc.
    blur_pipeline: wgpu::RenderPipeline,
    blur_bind_group_layout: wgpu::BindGroupLayout,
    colormatrix_pipeline: wgpu::RenderPipeline,
    colormatrix_bind_group_layout: wgpu::BindGroupLayout,
    // The caller's last `set_effect` request, in *logical* pixels (mirrors
    // every `draw_*` command's own convention) — `rebuild_effect_gpu`
    // scales `ShaderEffect::Blur`'s radius by the layer's *current*
    // `scale_factor` each time it actually builds GPU resources, rather
    // than baking a scale in here once, so a DPI change picked up between
    // `set_effect` calls (e.g. the window moves to a different-density
    // monitor, then later resizes) still re-derives a physically-correct
    // radius on the next rebuild instead of staying stale. `effect_gpu`
    // is the GPU resources built from it — `None`/`None` when no effect is
    // active. Kept separate from `pending`/`rebuild_if_needed`'s per-frame
    // hashing: an effect is caller-driven (changes only when `set_effect`
    // is called again), not part of the drawn content.
    effect: Option<ShaderEffect>,
    effect_gpu: Option<EffectGpu>,

    // The texture view `composite_bind_group` currently samples from —
    // `texture_view` directly, or an active effect's output. Tracked so a
    // mask's own composite bind group (which needs the same source at
    // binding 0) can be rebuilt whenever the source is repointed.
    composite_source_view: wgpu::TextureView,

    // `set_clip`'s whole-layer clip, logical pixels. It is the outermost
    // scope of the layer: wrapped around the replayed ops of a direct layer
    // and around the composite of a group (so an effect is clipped too).
    clip: Option<ClipShape>,
    // How `clip` is applied, resolved at build time against the current
    // scale and size: a scissor, or a range of the solid index buffer
    // drawn into the stencil.
    layer_clip: Option<LayerClipGpu>,
    // `CachePolicy::Always`: keep this layer in an offscreen texture even
    // without an effect.
    cache_always: bool,
    // Size of `texture`: the surface for a group, 1x1 for a direct layer
    // (which draws straight into the frame and owns no real target).
    texture_size: (u32, u32),

    // Current DPI scale factor — every `draw_*` method multiplies its
    // caller-supplied *logical*-pixel arguments by this before building a
    // `DrawCommand`, so `DrawCommand`/tessellation/glyph rasterization all
    // stay physical-pixel internally (crisp, no upscale blur) while callers
    // never have to think about DPI. Kept in sync lazily by `render_frame`
    // (same pull-based pattern as `resize_if_needed`), not pushed on
    // change.
    scale_factor: f32,

    pending: Vec<DrawCommand>,
    invalidation: InvalidationState,

    // The retained textures are the cache boundary. A clean layer keeps its
    // rendered content and effect output across frames and is only
    // composited again; these flags say which of the two passes is stale.
    // Both start true because a new texture's contents are undefined even
    // when nothing was drawn.
    content_texture_dirty: bool,
    effect_texture_dirty: bool,
    /// Passes actually recorded, so tests can prove clean frames skip them.
    #[cfg(test)]
    pub(super) content_passes: u32,
    #[cfg(test)]
    pub(super) effect_passes: u32,
    /// Full effect-resource rebuilds (texture allocations), as opposed to
    /// uniform patches.
    #[cfg(test)]
    pub(super) effect_rebuilds: u32,
    /// Geometry/op rebuilds, to prove clean direct layers do no CPU work.
    #[cfg(test)]
    pub(super) geometry_rebuilds: u32,
}

/// GPU state for one `DrawCommand::Image`, rebuilt whenever the layer
/// rebuilds (see [`LayerInner::build_buffers`]).
struct ImageDraw {
    vertex_buffer: wgpu::Buffer,
    // Kept alive so `bind_group`'s texture view stays valid; not read
    // directly otherwise.
    #[allow(dead_code)] // owns the GPU resource its bind group points at
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

impl LayerInner {
    // Eight params, seven distinct types — a params struct would exist for
    // this one private constructor alone, same call this codebase already
    // made for `build_image_draw`.
    #[allow(clippy::too_many_arguments)]
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipelines: &LayerPipelines,
        text_stack: Rc<RefCell<TextStack>>,
        format: wgpu::TextureFormat,
        size: (u32, u32),
        scale_factor: f32,
        invalidation: LayerInvalidation,
    ) -> Self {
        // Direct layers own no real target; groups get one in `sync_target`.
        let (texture, texture_view) = create_layer_texture(device, format, (1, 1));
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

        Self {
            device: device.clone(),
            queue: queue.clone(),
            solid_pipeline: pipelines.solid_pipeline.clone(),
            stencil_incr_pipeline: pipelines.stencil_incr_pipeline.clone(),
            stencil_decr_pipeline: pipelines.stencil_decr_pipeline.clone(),
            image_pipeline: pipelines.image_pipeline.clone(),
            composite_pipeline: pipelines.composite_pipeline.clone(),
            composite_bind_group_layout: pipelines.composite_bind_group_layout.clone(),
            composite_sampler: pipelines.composite_sampler.clone(),
            composite_quad_vbuf: pipelines.composite_quad_vbuf.clone(),
            format,
            size,
            texture,
            composite_source_view: texture_view.clone(),
            texture_view,
            screen_size_buffer,
            screen_size_bind_group,
            composite_bind_group,
            vertex_buffer: None,
            index_buffer: None,
            num_indices: 0,
            image_draws: Vec::new(),
            text_stack,
            ops: Vec::new(),
            text_batches: Vec::new(),
            blur_pipeline: pipelines.blur_pipeline.clone(),
            blur_bind_group_layout: pipelines.blur_bind_group_layout.clone(),
            colormatrix_pipeline: pipelines.colormatrix_pipeline.clone(),
            colormatrix_bind_group_layout: pipelines.colormatrix_bind_group_layout.clone(),
            effect: None,
            effect_gpu: None,
            clip: None,
            layer_clip: None,
            cache_always: false,
            texture_size: (1, 1),
            scale_factor,
            pending: Vec::new(),
            invalidation: invalidation_state,
            content_texture_dirty: true,
            effect_texture_dirty: true,
            #[cfg(test)]
            content_passes: 0,
            #[cfg(test)]
            effect_passes: 0,
            #[cfg(test)]
            effect_rebuilds: 0,
            #[cfg(test)]
            geometry_rebuilds: 0,
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
    fn clear(&mut self) {
        self.pending.clear();
        if let InvalidationState::Manual { dirty } = &mut self.invalidation {
            *dirty = true;
        }
    }

    /// Track the surface size: the screen-size uniform, the text viewport,
    /// and everything built against the old size (clip scissors) are
    /// refreshed. A group's texture is resized by [`LayerInner::sync_target`].
    fn resize_if_needed(&mut self, size: (u32, u32)) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.queue.write_buffer(
            &self.screen_size_buffer,
            0,
            bytemuck::bytes_of(&ScreenSize {
                screen_size: [size.0 as f32, size.1 as f32],
            }),
        );
        // Shared across every layer, so this runs redundantly once per
        // layer on an actual resize — harmless, it's just a small
        // `Resolution` update.
        self.text_stack.borrow_mut().resize(&self.queue, size);
        // Clip scopes were resolved against the old size.
        self.rebuild_layer_clip();
        self.mark_for_rebuild();
    }

    /// Resolve `clip` against the current scale and size. Cheap, and
    /// independent of the layer's content.
    fn rebuild_layer_clip(&mut self) {
        let Some(shape) = self.clip.clone() else {
            self.layer_clip = None;
            return;
        };
        if let Some(rect) = scissor_for_clip(&shape, self.scale_factor, self.size) {
            self.layer_clip = Some(LayerClipGpu {
                op: ReplayOp::ClipScissor(rect),
                vertex_buffer: None,
                index_buffer: None,
            });
            return;
        }
        let geometry = clip_shape_to_command(&shape, self.scale_factor);
        let mut tessellator = FillTessellator::new();
        let mut stroke_tessellator = StrokeTessellator::new();
        let (vertices, indices) =
            tessellate_command(&geometry, &mut tessellator, &mut stroke_tessellator);
        let vertex_buffer = (!vertices.is_empty()).then(|| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("atomos-layer-clip-vbuf"),
                    contents: bytemuck::cast_slice(&vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                })
        });
        let index_buffer = (!indices.is_empty()).then(|| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("atomos-layer-clip-ibuf"),
                    contents: bytemuck::cast_slice(&indices),
                    usage: wgpu::BufferUsages::INDEX,
                })
        });
        self.layer_clip = Some(LayerClipGpu {
            op: ReplayOp::ClipStencil(0..indices.len() as u32),
            vertex_buffer,
            index_buffer,
        });
    }

    /// Force the next `rebuild_if_needed` to rebuild geometry and ops.
    fn mark_for_rebuild(&mut self) {
        match &mut self.invalidation {
            InvalidationState::Automatic { last_hash } => *last_hash = None,
            InvalidationState::Manual { dirty } => *dirty = true,
        }
    }

    /// Whether this layer renders into an offscreen texture that is then
    /// composited: it has an effect, or asked to be cached. Every other
    /// layer is drawn straight into the frame and owns no real target.
    fn is_group(&self) -> bool {
        self.effect.is_some() || self.cache_always
    }

    /// Give a group a surface-sized texture and a direct layer a 1x1
    /// placeholder, reallocating (and rebuilding what samples it) only when
    /// that changes. This is what keeps a plain layer from costing a
    /// surface-sized allocation.
    fn sync_target(&mut self) {
        let wanted = if self.is_group() { self.size } else { (1, 1) };
        if self.texture_size == wanted {
            return;
        }
        self.texture_size = wanted;
        let (texture, texture_view) = create_layer_texture(&self.device, self.format, wanted);
        self.texture = texture;
        self.texture_view = texture_view;
        self.content_texture_dirty = true;
        self.effect_texture_dirty = true;
        self.set_composite_source(self.texture_view.clone());
        if self.effect.is_some() {
            self.rebuild_effect_gpu();
        }
    }

    /// Repoint what the composite step samples from: `texture_view`
    /// directly, or an active effect's output.
    fn set_composite_source(&mut self, view: wgpu::TextureView) {
        self.composite_bind_group = create_composite_bind_group(
            &self.device,
            &self.composite_bind_group_layout,
            &self.composite_sampler,
            &view,
        );
        self.composite_source_view = view;
    }

    /// (Re)build `effect_gpu` from `self.effect` (`None` clears it) and
    /// the layer's *current* `size`/`scale_factor`, repointing
    /// `composite_bind_group` at whatever the effect's final output is (or
    /// back at `texture_view` directly when there's no effect) — so
    /// `composite`/`render_to_texture` need no effect-awareness of their
    /// own. Called from `Layer::set_effect` (a new effect, or clearing one)
    /// and from `resize_if_needed` (an active effect's scratch textures
    /// need to match the new size, and a DPI change since the last build
    /// needs `ShaderEffect::Blur`'s radius re-derived — see the field doc
    /// on `effect` for why it's stored in logical units, not physical).
    ///
    /// ponytail: rebuilding always recompiles `ShaderEffect::Custom`'s
    /// pipeline too, even on a plain resize where only the textures
    /// actually needed to change — a window-drag resize repeatedly
    /// re-validating a user shader (each check briefly blocking via
    /// `pollster::block_on`) is wasteful, if not visibly slow at
    /// human-timescale resize rates. Split "rebuild textures/bind-groups"
    /// from "rebuild pipeline" if profiling ever shows resize hitching
    /// with a `Custom` effect active.
    /// Patch the uniforms of the already-built effect to match `effect`
    /// instead of reallocating its surface-sized textures. Only possible
    /// when `effect` is the same built-in kind as the one in place; returns
    /// whether it was, and the caller falls back to
    /// [`LayerInner::rebuild_effect_gpu`] when not. Marks the effect output
    /// stale on success.
    fn update_effect_uniforms(&mut self, effect: &ShaderEffect) -> bool {
        let Some(effect_gpu) = &self.effect_gpu else {
            return false;
        };
        match (&effect_gpu.kind, effect) {
            (EffectGpuKind::Blur(blur), ShaderEffect::Blur { radius }) => {
                let texel_size = [
                    1.0 / self.size.0.max(1) as f32,
                    1.0 / self.size.1.max(1) as f32,
                ];
                for (buffer, direction) in [
                    (&blur.params_buffer_h, [1.0, 0.0]),
                    (&blur.params_buffer_v, [0.0, 1.0]),
                ] {
                    self.queue.write_buffer(
                        buffer,
                        0,
                        bytemuck::bytes_of(&BlurParamsUniform {
                            direction,
                            texel_size,
                            radius: radius * self.scale_factor,
                            _pad: 0.0,
                        }),
                    );
                }
            }
            (
                EffectGpuKind::ColorMatrix { matrix_buffer, .. },
                ShaderEffect::ColorMatrix(matrix),
            ) => {
                self.queue.write_buffer(
                    matrix_buffer,
                    0,
                    bytemuck::bytes_of(&ColorMatrixUniform {
                        cols: matrix.to_columns(),
                    }),
                );
            }
            _ => return false,
        }
        self.effect_texture_dirty = true;
        true
    }

    fn rebuild_effect_gpu(&mut self) {
        #[cfg(test)]
        {
            self.effect_rebuilds += 1;
        }
        self.effect_texture_dirty = true;
        let Some(effect) = self.effect.clone() else {
            self.effect_gpu = None;
            self.set_composite_source(self.texture_view.clone());
            return;
        };

        let (output_texture, output_view) =
            create_layer_texture(&self.device, self.format, self.size);

        let kind = match &effect {
            ShaderEffect::Blur { radius } => {
                let physical_radius = radius * self.scale_factor;
                let (ping_texture, ping_view) =
                    create_layer_texture(&self.device, self.format, self.size);
                let texel_size = [
                    1.0 / self.size.0.max(1) as f32,
                    1.0 / self.size.1.max(1) as f32,
                ];
                let params_buffer_h =
                    self.device
                        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some("atomos-layer-blur-params-h"),
                            contents: bytemuck::bytes_of(&BlurParamsUniform {
                                direction: [1.0, 0.0],
                                texel_size,
                                radius: physical_radius,
                                _pad: 0.0,
                            }),
                            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        });
                let params_buffer_v =
                    self.device
                        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some("atomos-layer-blur-params-v"),
                            contents: bytemuck::bytes_of(&BlurParamsUniform {
                                direction: [0.0, 1.0],
                                texel_size,
                                radius: physical_radius,
                                _pad: 0.0,
                            }),
                            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        });
                let bind_group_h = create_effect_bind_group(
                    &self.device,
                    &self.blur_bind_group_layout,
                    &params_buffer_h,
                    &self.texture_view,
                    &self.composite_sampler,
                );
                let bind_group_v = create_effect_bind_group(
                    &self.device,
                    &self.blur_bind_group_layout,
                    &params_buffer_v,
                    &ping_view,
                    &self.composite_sampler,
                );
                EffectGpuKind::Blur(Box::new(BlurEffectGpu {
                    ping_texture,
                    ping_view,
                    params_buffer_h,
                    params_buffer_v,
                    bind_group_h,
                    bind_group_v,
                }))
            }
            ShaderEffect::ColorMatrix(matrix) => {
                let matrix_buffer =
                    self.device
                        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some("atomos-layer-colormatrix-uniform"),
                            contents: bytemuck::bytes_of(&ColorMatrixUniform {
                                cols: matrix.to_columns(),
                            }),
                            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        });
                let bind_group = create_effect_bind_group(
                    &self.device,
                    &self.colormatrix_bind_group_layout,
                    &matrix_buffer,
                    &self.texture_view,
                    &self.composite_sampler,
                );
                EffectGpuKind::ColorMatrix {
                    matrix_buffer,
                    bind_group,
                }
            }
            ShaderEffect::Custom(source) => {
                let Some(pipeline) = build_custom_effect_pipeline(
                    &self.device,
                    self.format,
                    source,
                    &self.composite_bind_group_layout,
                ) else {
                    // Compile/validation failed (already logged) — leave
                    // this layer without an effect rather than
                    // propagating the failure.
                    self.effect = None;
                    self.effect_gpu = None;
                    self.set_composite_source(self.texture_view.clone());
                    return;
                };
                let bind_group = create_composite_bind_group(
                    &self.device,
                    &self.composite_bind_group_layout,
                    &self.composite_sampler,
                    &self.texture_view,
                );
                EffectGpuKind::Custom {
                    pipeline,
                    bind_group,
                }
            }
        };

        self.set_composite_source(output_view.clone());
        self.effect_gpu = Some(EffectGpu {
            output_texture,
            output_view,
            kind,
        });
    }

    /// Record this layer's active effect's render pass(es), if any — must
    /// run after `render_to_texture` (needs this frame's freshly-rendered
    /// `texture_view` as input) and before `composite` (which reads
    /// whatever this leaves in `composite_bind_group`, already repointed
    /// by `rebuild_effect_gpu`).
    fn apply_effect_if_needed(&mut self, encoder: &mut wgpu::CommandEncoder) -> bool {
        if !std::mem::take(&mut self.effect_texture_dirty) {
            return false;
        }
        let Some(effect_gpu) = &self.effect_gpu else {
            return false;
        };
        #[cfg(test)]
        {
            self.effect_passes += 1;
        }
        match &effect_gpu.kind {
            EffectGpuKind::Blur(blur) => {
                run_fullscreen_pass(
                    encoder,
                    &self.blur_pipeline,
                    &blur.bind_group_h,
                    &blur.ping_view,
                    &self.composite_quad_vbuf,
                );
                run_fullscreen_pass(
                    encoder,
                    &self.blur_pipeline,
                    &blur.bind_group_v,
                    &effect_gpu.output_view,
                    &self.composite_quad_vbuf,
                );
            }
            EffectGpuKind::ColorMatrix { bind_group, .. } => {
                run_fullscreen_pass(
                    encoder,
                    &self.colormatrix_pipeline,
                    bind_group,
                    &effect_gpu.output_view,
                    &self.composite_quad_vbuf,
                );
            }
            EffectGpuKind::Custom {
                pipeline,
                bind_group,
            } => {
                run_fullscreen_pass(
                    encoder,
                    pipeline,
                    bind_group,
                    &effect_gpu.output_view,
                    &self.composite_quad_vbuf,
                );
            }
        }
        true
    }

    /// Add this layer's targets and counts to `stats`.
    fn add_to_stats(&self, stats: &mut RenderStats) {
        stats.layers += 1;
        stats.text_batches += self.text_batches.len() as u32;
        let texels = self.texture_size.0 as u64 * self.texture_size.1 as u64;
        stats.bytes.group_textures += texels * 4;
        if self.is_group() {
            stats.group_layers += 1;
        } else {
            stats.direct_layers += 1;
        }
        if let Some(effect) = &self.effect_gpu {
            let textures = match effect.kind {
                EffectGpuKind::Blur(_) => 2,
                _ => 1,
            };
            stats.bytes.effect_textures += texels * 4 * textures;
        }
    }

    /// Pick up a new DPI scale factor. Pending commands are logical, so a
    /// change just invalidates the built geometry and prepared text; the
    /// next rebuild derives physical sizes from the new factor.
    pub(super) fn set_scale_factor(&mut self, scale_factor: f32) {
        let changed = self.scale_factor != scale_factor;
        self.scale_factor = scale_factor;
        if changed {
            // Commands are stored in logical pixels, so retained content
            // only needs rebuilding (and its text re-preparing) at the new
            // density: nothing for the caller to redraw.
            match &mut self.invalidation {
                InvalidationState::Automatic { last_hash } => *last_hash = None,
                InvalidationState::Manual { dirty } => *dirty = true,
            }
            // The layer clip and a blur radius are in logical units too.
            self.rebuild_layer_clip();
            self.content_texture_dirty = true;
            if let Some(effect) = self.effect.clone()
                && !self.update_effect_uniforms(&effect)
            {
                self.rebuild_effect_gpu();
            }
        }
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

        // Text re-prepares whenever a batch's content changed, or once per
        // atlas trim or reset (see `TextStack::advance_frame`). This must
        // finish for every batch before anything is recorded: preparing a
        // renderer after its draw was recorded would replace the buffer the
        // recorded draw reads.
        let prepared = self.prepare_text_batches();

        if should_rebuild || prepared {
            self.content_texture_dirty = true;
        }

        // `Automatic` layers expect a full re-description of their content
        // every frame; `Manual` layers persist theirs until `clear()`.
        if matches!(self.invalidation, InvalidationState::Automatic { .. }) {
            self.pending.clear();
        }

        should_rebuild
    }

    /// The pending commands at the current physical scale.
    fn physical_commands(&self) -> Vec<DrawCommand> {
        self.pending
            .iter()
            .map(|command| command.to_physical(self.scale_factor))
            .collect()
    }

    /// Prepare every text batch whose content or atlas generation is stale.
    /// Returns whether anything was prepared.
    fn prepare_text_batches(&mut self) -> bool {
        let mut prepared_any = false;
        // A second sweep covers the one case where the first goes wrong: a
        // later batch's prepare reset the shared atlas, which invalidates
        // every batch prepared earlier in the sweep. A reset during the
        // second sweep is not retried; those batches retry next frame.
        for _ in 0..2 {
            let generation_at_start = self.text_stack.borrow().atlas_generation();
            for batch in &mut self.text_batches {
                let generation = self.text_stack.borrow().atlas_generation();
                if batch.prepared == Some((batch.hash, generation)) {
                    continue;
                }
                prepared_any = true;
                let mut stack = self.text_stack.borrow_mut();
                let mut result = stack.prepare_layer(
                    &self.device,
                    &self.queue,
                    &batch.commands,
                    &mut batch.renderer,
                );
                if result.is_err() {
                    // The atlas is full of glyphs nothing uses any more:
                    // start it afresh and try once more.
                    stack.reset_glyph_atlas(&self.device, &self.queue);
                    result = stack.prepare_layer(
                        &self.device,
                        &self.queue,
                        &batch.commands,
                        &mut batch.renderer,
                    );
                }
                match result {
                    Ok(()) => batch.prepared = Some((batch.hash, stack.atlas_generation())),
                    Err(e) => {
                        // Left unprepared so the next frame tries again
                        // instead of treating stale glyphs as current.
                        log::warn!("text glyph atlas full, text will retry next frame: {e:?}");
                        batch.prepared = None;
                    }
                }
            }
            if self.text_stack.borrow().atlas_generation() == generation_at_start {
                break;
            }
        }
        prepared_any
    }

    fn build_buffers(&mut self) {
        let mut vertices: Vec<Vertex> = Vec::new();
        // u32 indices, not u16 — a busy layer (an editor viewport's worth of
        // selection highlights, cursors, rounded rects at dozens of vertices
        // each) can exceed 65,535 vertices, and a u16 `base` would then wrap
        // *silently* into corrupted geometry. The doubled index memory is
        // noise next to the vertex data itself.
        let mut indices: Vec<u32> = Vec::new();
        // One tessellator (each) for the whole rebuild — lyon reuses its
        // internal allocations across `tessellate_path`/`tessellate_path`
        // calls, which fresh-per-command tessellators would throw away.
        let mut tessellator = FillTessellator::new();
        let mut stroke_tessellator = StrokeTessellator::new();
        self.image_draws.clear();
        let mut ops: Vec<ReplayOp> = Vec::new();
        let mut text_runs: Vec<Vec<DrawCommand>> = Vec::new();
        // Whether the previous command was text, so a run of them shares
        // one batch.
        let mut in_text_run = false;
        let physical = self.physical_commands();
        for command in physical {
            match &command {
                DrawCommand::PushClip(shape) => {
                    in_text_run = false;
                    if let Some(rect) = scissor_for_clip(shape, self.scale_factor, self.size) {
                        ops.push(ReplayOp::ClipScissor(rect));
                    } else {
                        let geometry = clip_shape_to_command(shape, self.scale_factor);
                        let before = indices.len() as u32;
                        let (cmd_vertices, cmd_indices) = tessellate_command(
                            &geometry,
                            &mut tessellator,
                            &mut stroke_tessellator,
                        );
                        append_geometry(&mut vertices, &mut indices, cmd_vertices, cmd_indices);
                        ops.push(ReplayOp::ClipStencil(before..indices.len() as u32));
                    }
                    continue;
                }
                DrawCommand::PopClip => {
                    in_text_run = false;
                    ops.push(ReplayOp::ClipPop);
                    continue;
                }
                _ => {}
            }
            let is_text = matches!(
                command,
                DrawCommand::Text { .. } | DrawCommand::StyledText { .. }
            );
            if is_text {
                if !in_text_run {
                    text_runs.push(Vec::new());
                    ops.push(ReplayOp::Text(text_runs.len() - 1));
                }
                text_runs
                    .last_mut()
                    .expect("a run was just started")
                    .push(command);
                in_text_run = true;
                continue;
            }
            in_text_run = false;

            // `Image` needs its own texture/bind group, not a shared
            // vertex/index buffer.
            if let DrawCommand::Image {
                pixels,
                top_left,
                size,
                tint,
            } = &command
            {
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
                let at = self.image_draws.len() - 1;
                match ops.last_mut() {
                    Some(ReplayOp::Images(range)) => range.end = at + 1,
                    _ => ops.push(ReplayOp::Images(at..at + 1)),
                }
                continue;
            }

            let before = indices.len() as u32;
            let (cmd_vertices, cmd_indices) =
                tessellate_command(&command, &mut tessellator, &mut stroke_tessellator);
            append_geometry(&mut vertices, &mut indices, cmd_vertices, cmd_indices);
            let after = indices.len() as u32;
            if after > before {
                match ops.last_mut() {
                    Some(ReplayOp::Solid(range)) => range.end = after,
                    _ => ops.push(ReplayOp::Solid(before..after)),
                }
            }
        }
        self.ops = ops;
        self.assign_text_batches(text_runs);
        #[cfg(test)]
        {
            self.geometry_rebuilds += 1;
        }

        self.num_indices = indices.len() as u32;
        // Fresh GPU buffers on every rebuild rather than `write_buffer`
        // into persisted, grow-only ones. Known deferred cost — revisit if
        // profiling ever shows rebuild-heavy workloads (per-keystroke
        // invalidation) spending real time here; wgpu buffer creation at
        // these sizes hasn't been the bottleneck so far.
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

    /// Give each text run a batch, reusing batches (and their glyphon
    /// renderers) by position so an unchanged run keeps its prepared state.
    #[cfg(test)]
    pub(super) fn texture_size_for_test(&self) -> (u32, u32) {
        self.texture_size
    }

    #[cfg(test)]
    pub(super) fn text_batch_states_for_test(&self) -> Vec<Option<(u64, u64)>> {
        self.text_batches
            .iter()
            .map(|batch| batch.prepared)
            .collect()
    }

    fn assign_text_batches(&mut self, runs: Vec<Vec<DrawCommand>>) {
        self.text_batches.truncate(runs.len());
        for (index, commands) in runs.into_iter().enumerate() {
            let mut hasher = DefaultHasher::new();
            for command in &commands {
                command.hash(&mut hasher);
            }
            let hash = hasher.finish();
            match self.text_batches.get_mut(index) {
                Some(batch) => {
                    batch.commands = commands;
                    batch.hash = hash;
                }
                None => {
                    let renderer = self
                        .text_stack
                        .borrow_mut()
                        .create_text_renderer(&self.device);
                    self.text_batches.push(TextBatch {
                        renderer,
                        commands,
                        hash,
                        prepared: None,
                    });
                }
            }
        }
    }

    /// Render this layer's cached geometry into its own offscreen texture.
    /// A separate render pass from both other layers and the main
    /// swap-chain pass — cheap to skip entirely for layers with no
    /// geometry (an empty `pending` still needs its texture cleared to
    /// transparent so it doesn't keep showing stale content).
    fn render_to_texture_if_needed(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        msaa: &MsaaTarget,
    ) -> bool {
        if !std::mem::take(&mut self.content_texture_dirty) {
            return false;
        }
        // New content means the effect must run over it again.
        self.effect_texture_dirty = true;
        #[cfg(test)]
        {
            self.content_passes += 1;
        }
        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("atomos-layer-pass"),
            // Render into the renderer's shared multisampled scratch
            // target; the GPU resolves it into this layer's own
            // `texture_view` automatically. The multisampled contents are
            // never read again (cleared fresh at every pass start), so
            // `Discard` rather than `Store` — which is also what makes
            // sharing one scratch texture across layers safe.
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &msaa.color_view,
                depth_slice: None,
                resolve_target: Some(&self.texture_view),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Discard,
                },
            })],
            depth_stencil_attachment: Some(stencil_attachment(&msaa.stencil_view)),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        // A group's content is drawn unclipped; the layer clip applies when
        // it is composited (so an effect is clipped too).
        let mut clip = ClipStack::new([0, 0, self.size.0, self.size.1]);
        self.replay_ops(&mut rpass, &mut clip);
        true
    }

    /// Enter a clip scope: a pixel-aligned rectangle tightens the scissor,
    /// any other shape is drawn into the stencil and tested against by all
    /// content after it.
    fn clip_open(
        &self,
        rpass: &mut wgpu::RenderPass<'_>,
        clip: &mut ClipStack,
        op: &ReplayOp,
        buffers: (&Option<wgpu::Buffer>, &Option<wgpu::Buffer>),
    ) {
        if clip.skipping > 0 {
            clip.skipping += 1;
            clip.scopes.push(Scope::Skipped);
            return;
        }
        match op {
            ReplayOp::ClipScissor(rect) => {
                let next = intersect_scissor(clip.scissor, *rect);
                if next[2] == 0 || next[3] == 0 {
                    clip.skipping += 1;
                    clip.scopes.push(Scope::Skipped);
                } else {
                    clip.scopes.push(Scope::Scissor(clip.scissor));
                    clip.scissor = next;
                    rpass.set_scissor_rect(next[0], next[1], next[2], next[3]);
                }
            }
            ReplayOp::ClipStencil(range) => {
                let (Some(vbuf), Some(ibuf)) = buffers else {
                    clip.scopes.push(Scope::Noop);
                    return;
                };
                if clip.depth >= MAX_CLIP_DEPTH {
                    clip.scopes.push(Scope::Noop);
                    return;
                }
                rpass.set_pipeline(&self.stencil_incr_pipeline);
                rpass.set_bind_group(0, &self.screen_size_bind_group, &[]);
                rpass.set_vertex_buffer(0, vbuf.slice(..));
                rpass.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint32);
                rpass.set_stencil_reference(clip.depth);
                rpass.draw_indexed(range.clone(), 0, 0..1);
                clip.depth += 1;
                clip.scopes
                    .push(Scope::Stencil(range.clone(), vbuf.clone(), ibuf.clone()));
            }
            _ => {}
        }
    }

    /// Leave the innermost clip scope; a stray close is ignored.
    fn clip_close(&self, rpass: &mut wgpu::RenderPass<'_>, clip: &mut ClipStack) {
        match clip.scopes.pop() {
            Some(Scope::Scissor(previous)) => {
                clip.scissor = previous;
                rpass.set_scissor_rect(previous[0], previous[1], previous[2], previous[3]);
            }
            Some(Scope::Stencil(range, vbuf, ibuf)) => {
                rpass.set_pipeline(&self.stencil_decr_pipeline);
                rpass.set_bind_group(0, &self.screen_size_bind_group, &[]);
                rpass.set_vertex_buffer(0, vbuf.slice(..));
                rpass.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint32);
                rpass.set_stencil_reference(clip.depth);
                rpass.draw_indexed(range, 0, 0..1);
                clip.depth -= 1;
            }
            Some(Scope::Skipped) => clip.skipping -= 1,
            Some(Scope::Noop) | None => {}
        }
    }

    /// Close every scope still open, restoring scissor and stencil so the
    /// next layer starts clean.
    fn clip_close_all(&self, rpass: &mut wgpu::RenderPass<'_>, clip: &mut ClipStack) {
        while !clip.scopes.is_empty() {
            self.clip_close(rpass, clip);
        }
    }

    /// Record this layer's ops in draw order.
    fn replay_ops(&self, rpass: &mut wgpu::RenderPass<'_>, clip: &mut ClipStack) {
        for op in &self.ops {
            match op {
                ReplayOp::ClipScissor(_) | ReplayOp::ClipStencil(_) => {
                    self.clip_open(rpass, clip, op, (&self.vertex_buffer, &self.index_buffer))
                }
                ReplayOp::ClipPop => self.clip_close(rpass, clip),
                _ if clip.skipping > 0 => {}
                ReplayOp::Solid(range) => {
                    let (Some(vbuf), Some(ibuf)) = (&self.vertex_buffer, &self.index_buffer) else {
                        continue;
                    };
                    rpass.set_pipeline(&self.solid_pipeline);
                    rpass.set_bind_group(0, &self.screen_size_bind_group, &[]);
                    rpass.set_vertex_buffer(0, vbuf.slice(..));
                    rpass.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint32);
                    rpass.set_stencil_reference(clip.depth);
                    rpass.draw_indexed(range.clone(), 0, 0..1);
                }
                ReplayOp::Images(range) => {
                    rpass.set_pipeline(&self.image_pipeline);
                    rpass.set_bind_group(0, &self.screen_size_bind_group, &[]);
                    rpass.set_stencil_reference(clip.depth);
                    for image_draw in &self.image_draws[range.clone()] {
                        rpass.set_bind_group(1, &image_draw.bind_group, &[]);
                        rpass.set_vertex_buffer(0, image_draw.vertex_buffer.slice(..));
                        rpass.draw(0..6, 0..1);
                    }
                }
                ReplayOp::Text(batch) => {
                    rpass.set_stencil_reference(clip.depth);
                    if let Err(e) = self
                        .text_stack
                        .borrow()
                        .render(&self.text_batches[*batch].renderer, rpass)
                    {
                        log::error!("text render error: {e:?}");
                    }
                }
            }
        }
    }

    /// Draw a direct layer into the frame: its ops, inside its layer clip.
    fn record_direct(&self, rpass: &mut wgpu::RenderPass<'_>, surface: [u32; 4]) {
        if self.ops.is_empty() {
            return;
        }
        let mut clip = ClipStack::new(surface);
        if let Some(layer_clip) = &self.layer_clip {
            self.clip_open(
                rpass,
                &mut clip,
                &layer_clip.op,
                (&layer_clip.vertex_buffer, &layer_clip.index_buffer),
            );
        }
        self.replay_ops(rpass, &mut clip);
        self.clip_close_all(rpass, &mut clip);
    }

    /// Composite a group's finished texture (or effect output) into the
    /// frame, inside the layer clip.
    fn record_group(&self, rpass: &mut wgpu::RenderPass<'_>, surface: [u32; 4]) {
        let mut clip = ClipStack::new(surface);
        if let Some(layer_clip) = &self.layer_clip {
            self.clip_open(
                rpass,
                &mut clip,
                &layer_clip.op,
                (&layer_clip.vertex_buffer, &layer_clip.index_buffer),
            );
        }
        if clip.skipping == 0 {
            rpass.set_pipeline(&self.composite_pipeline);
            rpass.set_bind_group(0, &self.composite_bind_group, &[]);
            rpass.set_vertex_buffer(0, self.composite_quad_vbuf.slice(..));
            rpass.set_stencil_reference(clip.depth);
            rpass.draw(0..6, 0..1);
        }
        self.clip_close_all(rpass, &mut clip);
    }
}

/// What closing a clip scope must undo.
enum Scope {
    /// Restore this scissor.
    Scissor([u32; 4]),
    /// Redraw this shape (a range of these buffers) with the decrementing
    /// pipeline.
    Stencil(std::ops::Range<u32>, wgpu::Buffer, wgpu::Buffer),
    /// The scope drew nothing (empty intersection, or nested inside one);
    /// only its closing needs tracking.
    Skipped,
    /// A clip that could not be applied but must stay balanced.
    Noop,
}

/// Deepest stencil nesting: the cleared value 0 up to the 8-bit maximum.
const MAX_CLIP_DEPTH: u32 = 255;

/// Clip state while recording one render pass.
struct ClipStack {
    scissor: [u32; 4],
    depth: u32,
    skipping: u32,
    scopes: Vec<Scope>,
}

impl ClipStack {
    fn new(surface: [u32; 4]) -> Self {
        Self {
            scissor: surface,
            depth: 0,
            skipping: 0,
            scopes: Vec::new(),
        }
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

/// A physical-pixel rectangle `[x, y, w, h]` from edge coordinates, clamped
/// to the target. Zero width or height means empty.
fn clamp_scissor(x0: f32, y0: f32, x1: f32, y1: f32, size: (u32, u32)) -> [u32; 4] {
    let (max_w, max_h) = (size.0 as f32, size.1 as f32);
    let x0 = x0.clamp(0.0, max_w);
    let y0 = y0.clamp(0.0, max_h);
    let x1 = x1.clamp(0.0, max_w);
    let y1 = y1.clamp(0.0, max_h);
    if x1 <= x0 || y1 <= y0 {
        return [0, 0, 0, 0];
    }
    [x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32]
}

fn intersect_scissor(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = (a[0] + a[2]).min(b[0] + b[2]);
    let y1 = (a[1] + a[3]).min(b[1] + b[3]);
    if x1 <= x0 || y1 <= y0 {
        return [0, 0, 0, 0];
    }
    [x0, y0, x1 - x0, y1 - y0]
}

/// The scissor equivalent of `shape`, when it is an unrounded rectangle
/// whose physical edges all fall on whole pixels. A fractional edge must
/// stay antialiased, which only the stencil path does.
fn scissor_for_clip(shape: &ClipShape, scale: f32, size: (u32, u32)) -> Option<[u32; 4]> {
    let ClipShape::Rectangle {
        top_left,
        size: extent,
        rounding,
    } = shape
    else {
        return None;
    };
    if *rounding != Rounding::NONE {
        return None;
    }
    let edges = [
        top_left.0 * scale,
        top_left.1 * scale,
        (top_left.0 + extent.0) * scale,
        (top_left.1 + extent.1) * scale,
    ];
    if edges.iter().any(|e| (e - e.round()).abs() > 1e-3) {
        return None;
    }
    Some(clamp_scissor(edges[0], edges[1], edges[2], edges[3], size))
}

/// How a [`Layer`] is kept between frames. See [`Layer::set_cache_policy`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CachePolicy {
    /// Drawn straight into the frame; offscreen only if it has an effect.
    #[default]
    Auto,
    /// Always rendered to its own texture, re-rendered only when changed.
    Always,
}

/// Format of the shared stencil attachment that clip scopes draw into.
pub(super) const STENCIL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Stencil8;

/// The shared multisampled targets every layer pass renders into: colour
/// (resolved into the layer's own texture) and the stencil clip scopes use.
pub(super) struct MsaaTarget {
    _color: wgpu::Texture,
    pub(super) color_view: wgpu::TextureView,
    _stencil: wgpu::Texture,
    pub(super) stencil_view: wgpu::TextureView,
    /// Allocation arithmetic: `(colour, stencil)` bytes.
    bytes: (u64, u64),
}

pub(super) fn create_msaa_target(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    sample_count: u32,
) -> MsaaTarget {
    let (color, color_view) = create_msaa_texture(device, format, size, sample_count);
    let (stencil, stencil_view) = create_msaa_texture(device, STENCIL_FORMAT, size, sample_count);
    let pixels = size.0.max(1) as u64 * size.1.max(1) as u64 * sample_count as u64;
    MsaaTarget {
        _color: color,
        color_view,
        _stencil: stencil,
        stencil_view,
        bytes: (pixels * 4, pixels),
    }
}

/// The stencil attachment for a pass: cleared to 0 (no clip active) and
/// never read afterwards.
fn stencil_attachment(view: &wgpu::TextureView) -> wgpu::RenderPassDepthStencilAttachment<'_> {
    wgpu::RenderPassDepthStencilAttachment {
        view,
        depth_ops: None,
        stencil_ops: Some(wgpu::Operations {
            load: wgpu::LoadOp::Clear(0),
            store: wgpu::StoreOp::Discard,
        }),
    }
}

/// Stencil state of content pipelines: draw only where the stencil equals
/// the pass's current reference (the clip depth), never write it.
pub(super) fn content_stencil_state() -> wgpu::DepthStencilState {
    let face = wgpu::StencilFaceState {
        compare: wgpu::CompareFunction::Equal,
        fail_op: wgpu::StencilOperation::Keep,
        depth_fail_op: wgpu::StencilOperation::Keep,
        pass_op: wgpu::StencilOperation::Keep,
    };
    wgpu::DepthStencilState {
        format: STENCIL_FORMAT,
        depth_write_enabled: Some(false),
        depth_compare: Some(wgpu::CompareFunction::Always),
        stencil: wgpu::StencilState {
            front: face,
            back: face,
            read_mask: 0xff,
            write_mask: 0xff,
        },
        bias: wgpu::DepthBiasState::default(),
    }
}

/// Stencil state that adds `op` wherever the stencil equals the reference:
/// `IncrementClamp` enters a clip, `DecrementClamp` leaves it.
fn clip_stencil_state(op: wgpu::StencilOperation) -> wgpu::DepthStencilState {
    let face = wgpu::StencilFaceState {
        compare: wgpu::CompareFunction::Equal,
        fail_op: wgpu::StencilOperation::Keep,
        depth_fail_op: wgpu::StencilOperation::Keep,
        pass_op: op,
    };
    wgpu::DepthStencilState {
        format: STENCIL_FORMAT,
        depth_write_enabled: Some(false),
        depth_compare: Some(wgpu::CompareFunction::Always),
        stencil: wgpu::StencilState {
            front: face,
            back: face,
            read_mask: 0xff,
            write_mask: 0xff,
        },
        bias: wgpu::DepthBiasState::default(),
    }
}

/// Build the offscreen multisampled colour texture every layer's
/// solid/image/text pipelines render into — resolved into that layer's own
/// (single-sample) texture at the end of its pass. One shared instance for
/// all layers, owned by the renderer (see `LayerInner`'s field comment on
/// why sharing is safe). Never sampled — only `RENDER_ATTACHMENT` usage.
pub(super) fn create_msaa_texture(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    (width, height): (u32, u32),
    sample_count: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("atomos-layer-msaa-texture"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count,
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

    ImageDraw {
        vertex_buffer,
        texture,
        bind_group,
    }
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

/// Lower a logical-units [`ClipShape`] to the physical-px [`DrawCommand`]
/// its mask is tessellated from — the same logical-to-physical scaling
/// every `Layer::draw_*` method applies at its call boundary, with a flat
/// white fill (the mask only ever reads coverage/alpha).
fn clip_shape_to_command(shape: &ClipShape, s: f32) -> DrawCommand {
    const WHITE: Color = Color::rgb(0xFF, 0xFF, 0xFF);
    match shape {
        ClipShape::Rectangle {
            top_left,
            size,
            rounding,
        } => DrawCommand::Rectangle {
            top_left: [top_left.0 * s, top_left.1 * s],
            size: [size.0 * s, size.1 * s],
            color: WHITE,
            rounding: rounding.scaled(s),
        },
        ClipShape::Circle { center, radius } => DrawCommand::Circle {
            center: [center.0 * s, center.1 * s],
            radius: radius * s,
            color: WHITE,
        },
        ClipShape::Polygon { vertices } => DrawCommand::Polygon {
            vertices: vertices.iter().map(|&(x, y)| [x * s, y * s]).collect(),
            color: WHITE,
        },
        ClipShape::Path { d, position } => DrawCommand::Path {
            d: d.clone(),
            position: [position.0 * s, position.1 * s],
            scale: s,
            paint: PathPaint::fill(WHITE),
        },
    }
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

/// Same rule as the `FillVertexConstructor` impl above, for stroke
/// tessellation's own (structurally identical, but distinctly-typed)
/// vertex — see [`tessellate_stroked`].
impl StrokeVertexConstructor<Vertex> for ColorAtPosition<'_> {
    fn new_vertex(&mut self, v: StrokeVertex) -> Vertex {
        let p = v.position();
        Vertex {
            position: [p.x, p.y],
            color: self.color.resolve([p.x, p.y], self.min, self.max),
        }
    }
}

fn tessellate_command(
    command: &DrawCommand,
    tessellator: &mut FillTessellator,
    stroke_tessellator: &mut StrokeTessellator,
) -> (Vec<Vertex>, Vec<u32>) {
    match command {
        DrawCommand::Rectangle {
            top_left,
            size,
            color,
            rounding,
        } => tessellate_rectangle(*top_left, *size, color, rounding, tessellator),
        DrawCommand::Polygon { vertices, color } => {
            tessellate_polygon(vertices, color, tessellator)
        }
        DrawCommand::Circle {
            center,
            radius,
            color,
        } => tessellate_circle(*center, *radius, color, tessellator),
        DrawCommand::Path {
            d,
            position,
            scale,
            paint,
        } => tessellate_svg_path(d, *position, *scale, paint, tessellator, stroke_tessellator),
        // Handled directly in `LayerInner::build_buffers`, which needs its
        // own texture per image rather than a shared vertex/index buffer
        // — filtered out before ever reaching here.
        DrawCommand::Image { .. } => (Vec::new(), Vec::new()),
        // Handled entirely in `LayerInner::rebuild_if_needed`/
        // `render_to_texture` via the shared `TextStack`, not the solid
        // vertex/index batch.
        DrawCommand::Text { .. }
        | DrawCommand::StyledText { .. }
        | DrawCommand::PushClip(_)
        | DrawCommand::PopClip => (Vec::new(), Vec::new()),
    }
}

/// `true` if any color reachable from `paint` is [`Color::PerVertex`] —
/// [`Layer::draw_path`] rejects that, same reasoning as
/// [`Layer::draw_circle`] (a path's vertices come from tessellating `d`,
/// not from the caller, so there's nothing to pin a per-vertex corner color
/// to).
fn paint_has_per_vertex_color(paint: &PathPaint) -> bool {
    let is_per_vertex = |c: &Color| matches!(c, Color::PerVertex(_));
    match paint {
        PathPaint::Fill { color, .. } => is_per_vertex(color),
        PathPaint::Stroke(stroke) => is_per_vertex(&stroke.color),
        PathPaint::FillAndStroke {
            fill_color, stroke, ..
        } => is_per_vertex(fill_color) || is_per_vertex(&stroke.color),
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
            path::Event::Cubic {
                from,
                ctrl1,
                ctrl2,
                to,
            } => {
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

/// Every control point of `path` multiplied by `scale`. Affine-exact for
/// Bézier curves (scaling every control point by `s` scales the curve it
/// defines by exactly `s`, no flattening error introduced) — used to bring
/// a `d` string's local-unit geometry to physical pixels *before*
/// tessellation, so the fixed tolerance tessellation uses below stays a
/// fixed *physical*-pixel tolerance at any DPI scale factor, rather than
/// silently getting coarser at high DPI.
fn scale_path(path: &Path, scale: f32) -> Path {
    let mut builder = Path::builder();
    let s = |p: lyon::math::Point| path::geom::point(p.x * scale, p.y * scale);
    for event in path.iter() {
        match event {
            path::Event::Begin { at } => {
                builder.begin(s(at));
            }
            path::Event::Line { to, .. } => {
                builder.line_to(s(to));
            }
            path::Event::Quadratic { ctrl, to, .. } => {
                builder.quadratic_bezier_to(s(ctrl), s(to));
            }
            path::Event::Cubic {
                ctrl1, ctrl2, to, ..
            } => {
                builder.cubic_bezier_to(s(ctrl1), s(ctrl2), s(to));
            }
            path::Event::End { close, .. } => {
                builder.end(close);
            }
        }
    }
    builder.build()
}

fn to_lyon_fill_rule(rule: FillRule) -> LyonFillRule {
    match rule {
        FillRule::NonZero => LyonFillRule::NonZero,
        FillRule::EvenOdd => LyonFillRule::EvenOdd,
    }
}

fn to_lyon_cap(cap: LineCap) -> LyonLineCap {
    match cap {
        LineCap::Butt => LyonLineCap::Butt,
        LineCap::Round => LyonLineCap::Round,
        LineCap::Square => LyonLineCap::Square,
    }
}

fn to_lyon_join(join: LineJoin) -> LyonLineJoin {
    match join {
        LineJoin::Miter => LyonLineJoin::Miter,
        LineJoin::Round => LyonLineJoin::Round,
        LineJoin::Bevel => LyonLineJoin::Bevel,
    }
}

/// Stroke-tessellate `path`, resolving color against `min`/`max` — the
/// stroke counterpart to [`tessellate_filled`], sharing the same
/// `ColorAtPosition` vertex constructor (it implements both
/// `FillVertexConstructor` and `StrokeVertexConstructor`). `scale` widens
/// `stroke.width` the same way `scale_path` already widened the path's own
/// geometry, so a caller-specified logical-pixel stroke width stays that
/// width in logical pixels at any DPI.
fn tessellate_stroked(
    path: &Path,
    stroke: &Stroke,
    scale: f32,
    min: [f32; 2],
    max: [f32; 2],
    tessellator: &mut StrokeTessellator,
) -> (Vec<Vertex>, Vec<u32>) {
    let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    let ctor = ColorAtPosition {
        color: &stroke.color,
        min,
        max,
    };
    let mut buf_builder = BuffersBuilder::new(&mut buffers, ctor);
    let options = StrokeOptions::DEFAULT
        .with_line_width(stroke.width * scale)
        .with_line_cap(to_lyon_cap(stroke.cap))
        .with_line_join(to_lyon_join(stroke.join))
        .with_miter_limit(stroke.miter_limit)
        .with_tolerance(0.1);
    tessellator
        .tessellate_path(path, &options, &mut buf_builder)
        .expect("stroke tessellation should never fail for a valid path");
    (buffers.vertices, buffers.indices)
}

/// Tessellate an SVG `d` string, re-parsed here (see [`DrawCommand::Path`]
/// for why): the parsed path is scaled to physical pixels first (see
/// [`scale_path`]), then filled and/or stroked per `paint` — each resolving
/// color against the path's own (scaled) local bounding box — and finally
/// translated by `position` (already physical-pixel scaled by the caller,
/// same as every other `draw_*` method — see `Layer::draw_path`).
fn tessellate_svg_path(
    d: &str,
    position: [f32; 2],
    scale: f32,
    paint: &PathPaint,
    tessellator: &mut FillTessellator,
    stroke_tessellator: &mut StrokeTessellator,
) -> (Vec<Vertex>, Vec<u32>) {
    let raw_path = parse_svg_path(d)
        .expect("Layer::draw_path already validated this string's syntax when it was queued");
    let path = scale_path(&raw_path, scale);
    let (min, max) = path_bounding_box(&path);

    let mut vertices: Vec<Vertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    match paint {
        PathPaint::Fill { color, rule } => {
            let (v, i) = tessellate_filled(
                &path,
                color,
                to_lyon_fill_rule(*rule),
                min,
                max,
                tessellator,
            );
            append_geometry(&mut vertices, &mut indices, v, i);
        }
        PathPaint::Stroke(stroke) => {
            let (v, i) = tessellate_stroked(&path, stroke, scale, min, max, stroke_tessellator);
            append_geometry(&mut vertices, &mut indices, v, i);
        }
        PathPaint::FillAndStroke {
            fill_color,
            fill_rule,
            stroke,
        } => {
            let (v, i) = tessellate_filled(
                &path,
                fill_color,
                to_lyon_fill_rule(*fill_rule),
                min,
                max,
                tessellator,
            );
            append_geometry(&mut vertices, &mut indices, v, i);
            let (v, i) = tessellate_stroked(&path, stroke, scale, min, max, stroke_tessellator);
            append_geometry(&mut vertices, &mut indices, v, i);
        }
    }

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
fn tessellate_filled(
    path: &Path,
    color: &Color,
    rule: LyonFillRule,
    min: [f32; 2],
    max: [f32; 2],
    tessellator: &mut FillTessellator,
) -> (Vec<Vertex>, Vec<u32>) {
    let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    let ctor = ColorAtPosition { color, min, max };
    let mut buf_builder = BuffersBuilder::new(&mut buffers, ctor);
    // Tolerance is in pixels here (unlike the old NDC-scale tessellation
    // this replaced), so a value much larger than a NDC-tuned `0.0005`
    // still looks smooth at UI scale.
    let fill_opts = FillOptions::DEFAULT
        .with_tolerance(0.1)
        .with_fill_rule(rule);
    tessellator
        .tessellate_path(path, &fill_opts, &mut buf_builder)
        .expect("fill tessellation should never fail for a valid path");
    (buffers.vertices, buffers.indices)
}

/// Append `new_vertices`/`new_indices` to `vertices`/`indices`, rebasing
/// the new indices past whatever's already there — the concatenation rule
/// every multi-batch tessellation (multiple `DrawCommand`s in one layer,
/// or fill+stroke from one `PathPaint::FillAndStroke`) shares.
fn append_geometry(
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    mut new_vertices: Vec<Vertex>,
    new_indices: Vec<u32>,
) {
    let base = vertices.len() as u32;
    vertices.append(&mut new_vertices);
    indices.extend(new_indices.into_iter().map(|i| i + base));
}

fn tessellate_rectangle(
    top_left: [f32; 2],
    size: [f32; 2],
    color: &Color,
    rounding: &Rounding,
    tessellator: &mut FillTessellator,
) -> (Vec<Vertex>, Vec<u32>) {
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
        LyonFillRule::EvenOdd,
        top_left,
        [top_left[0] + size[0], top_left[1] + size[1]],
        tessellator,
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

fn tessellate_polygon(
    vertices: &[[f32; 2]],
    color: &Color,
    tessellator: &mut FillTessellator,
) -> (Vec<Vertex>, Vec<u32>) {
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
    tessellate_filled(
        &builder.build(),
        color,
        LyonFillRule::EvenOdd,
        min,
        max,
        tessellator,
    )
}

fn tessellate_circle(
    center: [f32; 2],
    radius: f32,
    color: &Color,
    tessellator: &mut FillTessellator,
) -> (Vec<Vertex>, Vec<u32>) {
    let mut builder = Path::builder();
    builder.add_circle(
        path::geom::point(center[0], center[1]),
        radius,
        path::Winding::Positive,
    );

    let min = [center[0] - radius, center[1] - radius];
    let max = [center[0] + radius, center[1] + radius];
    tessellate_filled(
        &builder.build(),
        color,
        LyonFillRule::EvenOdd,
        min,
        max,
        tessellator,
    )
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
    // Same reasoning as `LayerInner::new`'s `#[allow]` just above.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipelines: &LayerPipelines,
        text_stack: Rc<RefCell<TextStack>>,
        format: wgpu::TextureFormat,
        size: (u32, u32),
        scale_factor: f32,
        invalidation: LayerInvalidation,
    ) -> Self {
        Self(Rc::new(RefCell::new(LayerInner::new(
            device,
            queue,
            pipelines,
            text_stack,
            format,
            size,
            scale_factor,
            invalidation,
        ))))
    }

    /// Current DPI scale factor — the multiplier every other `draw_*`
    /// method already applies internally. Exposed so callers can make
    /// their own scale-aware decisions (e.g. picking a higher-resolution
    /// image asset) without duplicating the renderer's own tracking of it.
    pub fn scale_factor(&self) -> f32 {
        self.0.borrow().scale_factor
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
    pub fn draw_rectangle(
        &self,
        top_left: (f32, f32),
        size: (f32, f32),
        color: Color,
        rounding: Rounding,
    ) {
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

    /// Draw a shape parsed from an SVG-style path-data string (the same
    /// syntax as an SVG `<path d="...">` attribute — `M`/`L`/`H`/`V`/`C`/
    /// `S`/`Q`/`T`/`A`/`Z`, relative or absolute, including elliptical
    /// arcs). Not a full SVG document parser — one path's `d` grammar, no
    /// `<svg>`/`<defs>`/`transform`/clip-path/etc.
    ///
    /// - `d`: the path-data string, authored in whatever local unit the
    ///   path was designed in (e.g. a 24x24 icon viewBox), treated as
    ///   logical pixels — author icons at their intended on-screen size.
    /// - `position`: a logical-pixel *offset* added to the path's own local
    ///   coordinates — not a scale, so the path's authored units become
    ///   pixels directly (DPI scaling aside).
    /// - `paint`: fill, stroke, or both — see [`PathPaint`]. Only
    ///   [`Color::Solid`]/[`Color::Gradient`] are accepted for any color
    ///   involved — same reasoning as [`Layer::draw_circle`].
    ///
    /// Returns an error immediately if `d` doesn't parse, rather than
    /// silently failing later when the layer happens to rebuild.
    pub fn draw_path(
        &self,
        d: &str,
        position: (f32, f32),
        paint: PathPaint,
    ) -> Result<(), lyon_extra::parser::ParseError> {
        self.push_path_command(d, [position.0, position.1], 1.0, paint)
    }

    /// Draw an SVG icon whose `d` was authored against `viewbox` (the
    /// SVG `viewBox="min_x min_y width height"` it came with) rather than
    /// [`Layer::draw_path`]'s "already at intended on-screen units, offset
    /// only" convention — real-world icon sets don't always use a plain
    /// `0 0 24 24` box (Google's Material Symbols, for one, ship
    /// `0 -960 960 960`), and hand-transforming every icon's path data to
    /// match `draw_path`'s convention before pasting it into source isn't
    /// something a caller should have to do by hand.
    ///
    /// `size`: the icon's on-screen (logical-pixel) width/height —
    /// `viewbox`'s width is used for the scale factor (icons are
    /// overwhelmingly square; a non-square `size`/`viewbox` combination
    /// stretches non-uniformly, same as [`Layer::draw_image`]'s `size`
    /// would, but isn't the expected case here).
    /// `position`: the icon's top-left corner, in logical pixels.
    pub fn draw_svg_icon(
        &self,
        d: &str,
        viewbox: (f32, f32, f32, f32),
        size: (f32, f32),
        position: (f32, f32),
        paint: PathPaint,
    ) -> Result<(), lyon_extra::parser::ParseError> {
        let (min_x, min_y, vb_width, _vb_height) = viewbox;
        let icon_scale = size.0 / vb_width;
        let position = [
            position.0 - min_x * icon_scale,
            position.1 - min_y * icon_scale,
        ];
        self.push_path_command(d, position, icon_scale, paint)
    }

    /// Shared by [`Layer::draw_path`]/[`Layer::draw_svg_icon`] — both just
    /// differ in how they compute `position`/`scale` (plain DPI scaling
    /// for the former, DPI scaling composed with a viewBox remap for the
    /// latter); parsing/validation/queueing is identical either way.
    fn push_path_command(
        &self,
        d: &str,
        position: [f32; 2],
        scale: f32,
        paint: PathPaint,
    ) -> Result<(), lyon_extra::parser::ParseError> {
        debug_assert!(
            !paint_has_per_vertex_color(&paint),
            "draw_path/draw_svg_icon doesn't support Color::PerVertex — use Solid or Gradient"
        );
        parse_svg_path(d)?;
        self.0.borrow_mut().push(DrawCommand::Path {
            d: d.to_string(),
            position,
            scale,
            paint,
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
        paint: PathPaint,
    ) -> Result<(), PathFileError> {
        let d = std::fs::read_to_string(path)?;
        self.draw_path(&d, position, paint)?;
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
        self.push_text(
            text,
            position,
            color,
            alignment,
            font,
            font_parameters,
            None,
        );
    }

    /// [`Layer::draw_text`], with lines wrapped to `max_width` (logical
    /// pixels): a line too long for it breaks at the last word boundary
    /// that fits (or, for a single word wider than `max_width`, between
    /// characters). `\n` still breaks unconditionally.
    ///
    /// `position`/`alignment` apply to the wrapped block's measured size
    /// ([`Layer::get_wrapped_text_size`]), which is at most `max_width`
    /// wide except for an unbreakable glyph wider than it. Lines are
    /// left-aligned within the block. Use [`Layer::layout_wrapped_text`]
    /// with the same arguments for carets, hit-testing and selections.
    ///
    /// Each distinct `max_width` is shaped (and cached) separately, so
    /// animating it re-shapes every frame; resize in steps if that shows up.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_wrapped_text(
        &self,
        text: &str,
        position: (f32, f32),
        max_width: f32,
        color: Color,
        alignment: Alignment,
        font: Font,
        font_parameters: FontParameters,
    ) {
        self.push_text(
            text,
            position,
            color,
            alignment,
            font,
            font_parameters,
            Some(max_width),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn push_text(
        &self,
        text: &str,
        position: (f32, f32),
        color: Color,
        alignment: Alignment,
        font: Font,
        font_parameters: FontParameters,
        wrap_width: Option<f32>,
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
            wrap_width,
        });
    }

    /// Measure the pixel size `text` would occupy shaped with `font`/
    /// `font_parameters`, before any [`Alignment`] offset is applied — the
    /// same measurement [`Layer::draw_text`] uses internally to resolve
    /// alignment. Exposed so callers can do their own layout (e.g. sizing
    /// a background box to fit a label) without duplicating text-shaping
    /// logic themselves.
    pub fn get_text_size(
        &self,
        text: &str,
        font: &Font,
        font_parameters: &FontParameters,
    ) -> (f32, f32) {
        self.text_size(text, font, font_parameters, None)
    }

    /// [`Layer::get_text_size`] for [`Layer::draw_wrapped_text`] with the
    /// same `max_width`.
    pub fn get_wrapped_text_size(
        &self,
        text: &str,
        max_width: f32,
        font: &Font,
        font_parameters: &FontParameters,
    ) -> (f32, f32) {
        self.text_size(text, font, font_parameters, Some(max_width))
    }

    fn text_size(
        &self,
        text: &str,
        font: &Font,
        font_parameters: &FontParameters,
        wrap_width: Option<f32>,
    ) -> (f32, f32) {
        let s = self.scale_factor();
        let scaled = font_parameters.scaled(s);
        let (width, height) = self.0.borrow().text_stack.borrow_mut().measure(
            text,
            font,
            &scaled,
            wrap_width.map(|w| w * s),
        );
        (width / s, height / s)
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
            spans
                .iter()
                .all(|span| matches!(span.color, Color::Solid(_))),
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
    /// Shape `text` exactly as [`Layer::draw_text`] would and return where
    /// everything lands, for editing and interaction: caret rectangles,
    /// click hit-testing and selection rectangles that stay correct across
    /// ligatures, combining marks and right-to-left runs. See
    /// [`TextLayout`]. Coordinates are logical pixels relative to the text's
    /// top-left; offset them by the position given to `draw_text` (after any
    /// [`Alignment`] shift computed from [`TextLayout::size`]).
    pub fn layout_text(
        &self,
        text: &str,
        font: &Font,
        font_parameters: &FontParameters,
    ) -> TextLayout {
        self.text_layout(text, font, font_parameters, None)
    }

    /// [`Layer::layout_text`] for [`Layer::draw_wrapped_text`] with the same
    /// `max_width`: one [`TextLine`](super::TextLine) per *wrapped* line, so
    /// carets, hit-testing and selection rectangles follow what is drawn. A
    /// byte offset at a soft wrap belongs to the line it starts, as the
    /// caret there is shown at the start of the next line.
    pub fn layout_wrapped_text(
        &self,
        text: &str,
        max_width: f32,
        font: &Font,
        font_parameters: &FontParameters,
    ) -> TextLayout {
        self.text_layout(text, font, font_parameters, Some(max_width))
    }

    fn text_layout(
        &self,
        text: &str,
        font: &Font,
        font_parameters: &FontParameters,
        wrap_width: Option<f32>,
    ) -> TextLayout {
        let s = self.scale_factor();
        let scaled = font_parameters.scaled(s);
        self.0
            .borrow()
            .text_stack
            .borrow_mut()
            .layout(text, font, &scaled, wrap_width.map(|w| w * s))
            .unscaled(s)
    }

    /// Clip everything `f` draws to `clip` (logical pixels). Scopes nest;
    /// content drawn after `f` returns is unclipped again.
    ///
    /// An unrounded rectangle on whole physical pixels is a free hardware
    /// scissor. Any other shape (rounded corners, circle, polygon, path, or
    /// a fractional edge) is drawn into a stencil buffer: no extra surface
    /// is allocated, and edges are antialiased by the same MSAA as the
    /// content. Text, images and shapes are all clipped, and a later shape
    /// can still paint over earlier clipped content.
    ///
    /// Errors, without calling `f`, only for an unparsable
    /// [`ClipShape::Path`].
    pub fn with_clip<R>(
        &self,
        clip: ClipShape,
        f: impl FnOnce(&Layer) -> R,
    ) -> Result<R, lyon_extra::parser::ParseError> {
        if let ClipShape::Path { d, .. } = &clip {
            parse_svg_path(d)?;
        }
        /// Closes the scope even if `f` panics, so the clip cannot leak
        /// into later commands.
        struct Close<'a>(&'a Layer);
        impl Drop for Close<'_> {
            fn drop(&mut self) {
                if let Ok(mut inner) = self.0.0.try_borrow_mut() {
                    inner.push(DrawCommand::PopClip);
                }
            }
        }
        self.0.borrow_mut().push(DrawCommand::PushClip(clip));
        let _close = Close(self);
        Ok(f(self))
    }

    pub fn get_styled_text_size(&self, spans: &[TextSpan]) -> (f32, f32) {
        let s = self.scale_factor();
        let scaled_spans: Vec<TextSpan> = spans
            .iter()
            .map(|span| TextSpan {
                font_parameters: span.font_parameters.scaled(s),
                ..span.clone()
            })
            .collect();
        let (width, height) = self
            .0
            .borrow()
            .text_stack
            .borrow_mut()
            .measure_spans(&scaled_spans);
        (width / s, height / s)
    }

    /// Set (or clear, with `None`) this layer's post-process
    /// [`ShaderEffect`], applied to its whole composited output — see
    /// `shader.rs`'s module doc for why "whole layer" is the unit shader
    /// effects apply at here (shade a specific shape/path/text run by
    /// giving it its own layer).
    ///
    /// Switching to a different effect kind rebuilds its GPU resources
    /// immediately, not deferred to the next frame. Changing only the
    /// parameters of the effect already in place (a blur radius, a colour
    /// matrix) just rewrites its uniforms, so animating them every frame
    /// allocates nothing. [`ShaderEffect::Custom`]'s WGSL is
    /// compiled and validated here too; a bad shader logs and leaves this
    /// layer without an effect rather than panicking.
    pub fn set_effect(&self, effect: Option<ShaderEffect>) {
        let mut inner = self.0.borrow_mut();
        if let Some(effect) = &effect
            && inner.update_effect_uniforms(effect)
        {
            inner.effect = Some(effect.clone());
            return;
        }
        inner.effect = effect;
        let wanted = if inner.is_group() { inner.size } else { (1, 1) };
        if inner.texture_size == wanted {
            inner.rebuild_effect_gpu();
        } else {
            // Gaining or losing the offscreen target also rebuilds the
            // effect against it.
            inner.sync_target();
            if inner.effect.is_none() {
                inner.rebuild_effect_gpu();
            }
        }
    }

    /// Clip the whole layer to `shape` (logical pixels; `None` clears): the
    /// outermost clip scope, applied after any effect, so a blur cannot
    /// bleed outside it. Anything [`Layer::with_clip`] accepts works here,
    /// with the same cost model (a pixel-aligned rectangle is a free
    /// scissor, every other shape uses the shared stencil, and no surface
    /// is allocated either way).
    ///
    /// [`ClipShape::Path`]'s `d` is validated here, same as
    /// [`Layer::draw_path`]; the other variants can't fail.
    pub fn set_clip(&self, shape: Option<ClipShape>) -> Result<(), lyon_extra::parser::ParseError> {
        if let Some(ClipShape::Path { d, .. }) = &shape {
            parse_svg_path(d)?;
        }
        let mut inner = self.0.borrow_mut();
        if inner.clip != shape {
            inner.clip = shape;
            inner.rebuild_layer_clip();
        }
        Ok(())
    }

    /// Choose whether this layer is drawn straight into the frame
    /// ([`CachePolicy::Auto`], the default) or kept as a texture that is
    /// only re-rendered when it changes ([`CachePolicy::Always`]).
    ///
    /// A direct layer costs no memory of its own and is replayed from its
    /// retained geometry each presented frame. A cached layer costs a
    /// surface-sized texture but a frame where it is unchanged is a single
    /// textured quad, which pays off for a large, mostly static layer
    /// (a full-screen document) under a frequently animating one (a caret).
    /// A layer with an effect is always cached.
    pub fn set_cache_policy(&self, policy: CachePolicy) {
        let mut inner = self.0.borrow_mut();
        let always = policy == CachePolicy::Always;
        if inner.cache_always != always {
            inner.cache_always = always;
            inner.sync_target();
        }
    }

    /// Discard every pending/previously-built draw command for this layer.
    /// Mainly meaningful for [`LayerInvalidation::Manual`] layers (call
    /// this before re-describing content that changed); harmless on
    /// [`LayerInvalidation::Automatic`] layers, which already clear their
    /// own pending content every rebuilt frame.
    // Not called by the layer spike (its one `Manual` layer draws once and
    // never redraws) — kept as the real API `Manual`-layer callers need.
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }
}

/// Record one frame onto `view` (the swap-chain view, or any single-sample
/// target): bring every live layer up to date, render the offscreen groups
/// (layers with an effect or `CachePolicy::Always`), then draw everything
/// bottom to top in a single multisampled pass cleared to `clear`.
///
/// `order` runs bottom (front) to top (back). Direct layers replay their
/// retained ops inside the pass; groups composite their texture. Every
/// text batch is prepared before anything is recorded: preparing a
/// renderer after its draw was recorded would replace the buffer the
/// recorded draw reads, and a prepare that resets the shared atlas would
/// leave earlier batches pointing into the discarded one.
pub(super) fn render_frame(
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    msaa: &MsaaTarget,
    order: &mut std::collections::VecDeque<std::rc::Weak<RefCell<LayerInner>>>,
    surface_size: (u32, u32),
    scale_factor: f32,
    clear: wgpu::Color,
) -> RenderStats {
    let mut stats = RenderStats::default();
    stats.bytes.msaa_color = msaa.bytes.0;
    stats.bytes.msaa_stencil = msaa.bytes.1;
    order.retain(|weak| weak.strong_count() > 0);
    let live: Vec<Rc<RefCell<LayerInner>>> =
        order.iter().filter_map(|weak| weak.upgrade()).collect();

    // Phase 1: CPU work and queue writes only.
    let generation_before = live
        .first()
        .map(|inner| inner.borrow().text_stack.borrow().atlas_generation());
    for inner in &live {
        let mut inner = inner.borrow_mut();
        inner.resize_if_needed(surface_size);
        inner.set_scale_factor(scale_factor);
        inner.sync_target();
        if inner.rebuild_if_needed() {
            stats.rebuilt_layers += 1;
        }
    }
    // A later layer's prepare may have reset the shared atlas, which
    // invalidates text prepared earlier in this phase: sweep once more (each
    // batch only re-prepares if its generation is stale).
    if let Some(before) = generation_before
        && live[0].borrow().text_stack.borrow().atlas_generation() != before
    {
        for inner in &live {
            inner.borrow_mut().prepare_text_batches();
        }
    }

    // Phase 2: offscreen groups.
    for inner in &live {
        let mut inner = inner.borrow_mut();
        if inner.is_group() {
            stats.group_content_passes += inner.render_to_texture_if_needed(encoder, msaa) as u32;
            stats.group_effect_passes += inner.apply_effect_if_needed(encoder) as u32;
        }
        inner.add_to_stats(&mut stats);
    }

    // Phase 3: the frame.
    let surface = [0, 0, surface_size.0, surface_size.1];
    let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("atomos-frame-pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &msaa.color_view,
            depth_slice: None,
            resolve_target: Some(view),
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(clear),
                store: wgpu::StoreOp::Discard,
            },
        })],
        depth_stencil_attachment: Some(stencil_attachment(&msaa.stencil_view)),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    for inner in &live {
        let inner = inner.borrow();
        if inner.is_group() {
            inner.record_group(&mut rpass, surface);
        } else {
            inner.record_direct(&mut rpass, surface);
        }
    }
    stats
}
