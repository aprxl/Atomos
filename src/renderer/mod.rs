//! Renderer — GPU layer abstraction.
//!
//! The renderer's public surface is [`Renderer`] (owns the wgpu device,
//! surface, and the shared GPU resources every [`Layer`] draws through) and
//! [`Layer`] itself (a retained, independently-cached drawing surface with
//! `draw_*` methods — see `layer.rs`'s module doc for the full design).
//! `Layer` handles are ordinary values: create one with
//! [`Renderer::new_layer_top`]/[`Renderer::new_layer_bottom`] and call its
//! `draw_*` methods from anywhere that holds the handle — a `Layer` needs
//! no reference back to the `Renderer` to be drawn into.
//!
//! All wgpu types are kept private to this module (and its submodules) so
//! the rest of the codebase only sees [`Renderer`], [`Layer`], and the
//! plain-data vocabulary types (`Color`, `Rounding`, `Alignment`, `Font`,
//! `FontParameters`, `Pixels`) each `draw_*` method takes.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use wgpu::{CurrentSurfaceTexture, QuerySet};
use winit::{event::WindowEvent, window::Window};

mod alignment;
mod color;
mod draw_command;
mod font;
mod font_parameters;
mod glyph_effects;
mod layer;
mod pixels;
mod rounding;
mod text_stack;

// `GradientDirection`/`Rgba` are vocabulary for richer `Color` use
// (gradients/per-vertex), not yet exercised by every call site. This is a
// `bin` crate, so nothing outside can reach these `pub` items yet either,
// hence the blanket allow rather than one per item.
pub use alignment::{Alignment, HorizontalAlign, VerticalAlign};
pub use color::{Color, GradientDirection};
#[allow(unused_imports)]
pub use color::Rgba;
pub use font::Font;
pub use font_parameters::FontParameters;
pub use layer::{Layer, LayerInvalidation};
// Only needed by callers that pattern-match the error instead of just
// `.expect()`/`?`-propagating it (like the current demo does).
#[allow(unused_imports)]
pub use layer::PathFileError;
pub use pixels::Pixels;
pub use rounding::Rounding;

/// MSAA sample count for every layer's own render pass (the solid, image,
/// and text pipelines all draw into a multisampled offscreen target,
/// resolved down to the layer's actual single-sample texture — the same
/// technique the old Phase-0 spike used for the swap chain directly). 4x
/// is standard and near-universally supported on desktop Vulkan/Metal/DX12
/// for a render-attachment-only colour texture. Without this, only
/// axis-aligned edges (rectangles) look clean — anything with a diagonal
/// or curved edge (`draw_polygon`, `draw_circle`, glyph outlines) shows
/// visible aliasing. Shared by `layer.rs` and `text_stack.rs`, which is why
/// it lives here rather than in either one.
const MSAA_SAMPLE_COUNT: u32 = 4;

/// Errors that can escape [`Renderer::render`]. These are the cases the
/// renderer could not recover from internally; recoverable per-frame surface
/// states (timeout, occlusion, suboptimal swap chain) are handled inside
/// `render` and reported as `Ok(())`.
#[derive(Debug)]
pub enum RenderError {
    /// The window's swap chain is gone (the surface itself was lost).
    /// Recovering from this in-place is non-trivial; the practical response
    /// is to tear the renderer down and rebuild it.
    SurfaceLost,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SurfaceLost => write!(f, "wgpu surface was lost"),
        }
    }
}

impl std::error::Error for RenderError {}

/// The GPU renderer. Owns the wgpu device, queue, and swap chain, plus the
/// GPU resources every [`Layer`] shares (its pipelines, bind group layouts)
/// and the registry of currently-live layers.
pub struct Renderer {
    /// Kept alive so the `Surface<'static>` stays valid. Not read directly
    /// by any method today, but the field is required for soundness — the
    /// surface is built from this Arc and would dangle if it were dropped.
    #[allow(dead_code)]
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    // GPU resources shared by every `Layer` (its pipelines, bind group
    // layouts, the composite quad) — built once here rather than per layer.
    layer_pipelines: layer::LayerPipelines,
    // The glyphon/cosmic-text stack every `Layer`'s `draw_text` shares —
    // see `text_stack.rs`'s module doc for why it's one shared instance,
    // not one per layer.
    text_stack: Rc<RefCell<text_stack::TextStack>>,
    // Composite order: front = bottom of the visual stack, back = top.
    // `Weak`, not `Rc` — a layer stops being composited once the caller
    // drops their last `Layer` handle.
    layers: VecDeque<std::rc::Weak<RefCell<layer::LayerInner>>>,
    // GPU timestamp queries around each frame's commands, so frame time is
    // measured in GPU time rather than wall-clock CPU time (which would
    // include time spent waiting on `Fifo` vsync and doesn't reflect actual
    // GPU work). `timestamp_query_set` holds the two raw timestamps (frame
    // start/end); `timestamp_resolve_buffer` is where the GPU writes them as
    // plain `u64`s; `timestamp_readback_buffer` is a `MAP_READ` copy of that
    // the CPU can actually read from.
    timestamp_query_set: QuerySet,
    timestamp_resolve_buffer: wgpu::Buffer,
    timestamp_readback_buffer: wgpu::Buffer,
    // Nanoseconds per timestamp tick — device/driver-specific, queried once.
    timestamp_period_ns: f64,
    // GPU duration of the most recently completed frame.
    last_frametime: std::time::Duration,
    // Wall-clock instant the renderer was created — `get_render_time` reports
    // elapsed time against this. Summing per-frame GPU durations instead
    // (as `last_frametime` does) would undercount badly: GPU work is only a
    // sliver of each frame, the rest is spent idle waiting on `Fifo` vsync,
    // so that sum stays far below true elapsed time.
    start_time: std::time::Instant,
}

/// Pixel-space vertex format shared by every [`Layer`]'s internal solid
/// pipeline: a position (interpreted in pixels, converted to clip space via
/// that layer's `screen_size` uniform) plus a color. Carrying color
/// per-vertex means one pipeline draws any flat-filled shape a layer's
/// `draw_*` methods produce.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
}

impl Vertex {
    const ATTRS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

// --- Implementation ---------------------------------------------------------

impl Renderer {
    /// Build a [`Renderer`] for the given window. Async because wgpu device
    /// and adapter requests are async; callers should drive this to
    /// completion with `pollster::block_on` or a runtime.
    pub async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());

        let surface = instance
            .create_surface(window.clone())
            .expect("failed to create wgpu surface for window");

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .expect("no compatible wgpu adapter found");

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("atomos-device"),
                // `TIMESTAMP_QUERY_INSIDE_ENCODERS` is needed on top of plain
                // `TIMESTAMP_QUERY` because `render`'s two `write_timestamp`
                // calls happen directly on the `CommandEncoder`, not inside a
                // render/compute pass.
                required_features: wgpu::Features::TIMESTAMP_QUERY
                    | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS,
                required_limits: wgpu::Limits::default(),
                experimental_features: wgpu::ExperimentalFeatures::default(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::default(),
            })
            .await
            .expect("failed to request wgpu device");

        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            // Fifo = vsync: present blocks until the next vblank, capping the
            // render loop to the display refresh rate. `main.rs` drives
            // rendering with `ControlFlow::Poll` and unconditionally requests
            // another redraw every frame, so without this the loop would
            // render as fast as the GPU allows. Fifo is required to be
            // supported by every wgpu surface, unlike `caps.present_modes[0]`
            // (whatever the platform happens to list first).
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let layer_pipelines = layer::LayerPipelines::new(&device, surface_config.format);
        let text_stack = Rc::new(RefCell::new(text_stack::TextStack::new(
            &device,
            &queue,
            surface_config.format,
            (surface_config.width, surface_config.height),
        )));
        let layers = VecDeque::new();

        // Query set for timestamping frame times. This is used instead of `Instant::now` because
        // CPU time is not accurate enough for GPU asynchronous operations. We only need two timestamps, one
        // for the beginning and another for the end.
        let timestamp_query_set = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("atomos-timestamp-query"),
            ty: wgpu::QueryType::Timestamp,
            count: 2
        });
        // 2 timestamps * 8 bytes (`QUERY_SIZE`) each.
        const TIMESTAMP_BUFFER_SIZE: wgpu::BufferAddress = 16;
        let timestamp_resolve_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("atomos-timestamp-resolve-buffer"),
            size: TIMESTAMP_BUFFER_SIZE,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let timestamp_readback_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("atomos-timestamp-readback-buffer"),
            size: TIMESTAMP_BUFFER_SIZE,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let timestamp_period_ns = queue.get_timestamp_period() as f64;

        Self {
            window,
            device,
            queue,
            surface,
            surface_config,
            layer_pipelines,
            text_stack,
            layers,
            timestamp_query_set,
            timestamp_resolve_buffer,
            timestamp_readback_buffer,
            timestamp_period_ns,
            last_frametime: std::time::Duration::ZERO,
            start_time: std::time::Instant::now(),
        }
    }

    /// Reconfigure the swap chain for a new window size. No-op on zero-sized
    /// (minimised) windows, which would otherwise produce invalid surfaces.
    /// Live layers pick up the new size lazily, the next time they render
    /// (see `layer::render_layers`).
    pub fn resize(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.surface_config.width = width;
            self.surface_config.height = height;
            self.surface.configure(&self.device, &self.surface_config);
        }
    }

    /// Create a new [`Layer`] on top of every existing layer (drawn last,
    /// so it appears above everything else). `invalidation` picks how the
    /// layer decides its content is stale — see [`LayerInvalidation`].
    pub fn new_layer_top(&mut self, invalidation: LayerInvalidation) -> Layer {
        let layer = Layer::new(
            &self.device,
            &self.queue,
            &self.layer_pipelines,
            self.text_stack.clone(),
            self.surface_config.format,
            (self.surface_config.width, self.surface_config.height),
            invalidation,
        );
        self.layers.push_back(Rc::downgrade(&layer.0));
        layer
    }

    /// Create a new [`Layer`] below every existing layer (drawn first, so
    /// everything else appears above it). `invalidation` picks how the
    /// layer decides its content is stale — see [`LayerInvalidation`].
    pub fn new_layer_bottom(&mut self, invalidation: LayerInvalidation) -> Layer {
        let layer = Layer::new(
            &self.device,
            &self.queue,
            &self.layer_pipelines,
            self.text_stack.clone(),
            self.surface_config.format,
            (self.surface_config.width, self.surface_config.height),
            invalidation,
        );
        self.layers.push_front(Rc::downgrade(&layer.0));
        layer
    }

    /// Render one frame: clear the swap chain, then composite every live
    /// layer onto it in bottom-to-top order (each into its own offscreen
    /// texture first, skipped entirely for layers whose content hasn't
    /// changed — see `layer::render_layers`).
    ///
    /// Returns `Ok(())` for every per-frame state that is handled in place
    /// (timeout, occlusion, suboptimal/outdated swap chain). The `Result`
    /// only carries genuinely unrecoverable surface errors.
    pub fn render(&mut self) -> Result<(), RenderError> {
        let frame = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(frame) => frame,
            CurrentSurfaceTexture::Suboptimal(frame) => {
                // Format/size no longer matches the surface; reconfigure and
                // use the frame anyway.
                self.surface.configure(&self.device, &self.surface_config);
                frame
            }
            CurrentSurfaceTexture::Outdated => {
                // The surface has changed; reconfigure and skip this frame.
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(());
            }
            CurrentSurfaceTexture::Timeout
            | CurrentSurfaceTexture::Occluded
            | CurrentSurfaceTexture::Validation => {
                // Nothing usable this frame; try again next time.
                return Ok(());
            }
            CurrentSurfaceTexture::Lost => {
                // The surface itself is gone; the only honest signal we can
                // give the caller is that the surface is invalid.
                return Err(RenderError::SurfaceLost);
            }
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("atomos-frame-encoder"),
            });

        encoder.write_timestamp(&self.timestamp_query_set, 0);

        {
            // Nothing draws directly into the swap chain any more — every
            // visible pixel comes from a layer's composite blit — so this
            // pass only needs to clear it.
            let _clear_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("atomos-clear-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.05,
                            b: 0.08,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }

        layer::render_layers(
            &mut encoder,
            &view,
            &mut self.layers,
            (self.surface_config.width, self.surface_config.height),
        );

        encoder.write_timestamp(&self.timestamp_query_set, 1);
        encoder.resolve_query_set(&self.timestamp_query_set, 0..2, &self.timestamp_resolve_buffer, 0);
        encoder.copy_buffer_to_buffer(
            &self.timestamp_resolve_buffer,
            0,
            &self.timestamp_readback_buffer,
            0,
            self.timestamp_readback_buffer.size(),
        );

        self.queue.submit(std::iter::once(encoder.finish()));
        // In wgpu 29, `present` is a method on `SurfaceTexture`; in wgpu 30
        // it moved to `Queue`.
        frame.present();

        self.read_back_frametime();

        Ok(())
    }

    /// Block until this frame's GPU commands finish, then read the two
    /// timestamps written around them and fold the result into
    /// `last_frametime`.
    ///
    /// This does wait on the GPU (via `device.poll`), which the rest of
    /// `render` otherwise avoids — acceptable for now since `Fifo` present
    /// mode already blocks the CPU on vsync every frame anyway; revisit if
    /// that stops being true (e.g. an `Immediate`/uncapped mode is added).
    fn read_back_frametime(&mut self) {
        let slice = self.timestamp_readback_buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("polling the device for the timestamp readback failed");

        let ticks: [u64; 2] = {
            let data = slice.get_mapped_range();
            let ticks: &[u64] = bytemuck::cast_slice(&data);
            [ticks[0], ticks[1]]
        };
        self.timestamp_readback_buffer.unmap();

        let elapsed_ticks = ticks[1].saturating_sub(ticks[0]);
        let elapsed_ns = elapsed_ticks as f64 * self.timestamp_period_ns;
        self.last_frametime = std::time::Duration::from_nanos(elapsed_ns as u64);
    }

    /// GPU duration of the most recently completed frame (the time spent
    /// executing this renderer's own commands, not wall-clock frame pacing —
    /// see `read_back_frametime`).
    pub fn get_frametime(&self) -> std::time::Duration {
        self.last_frametime
    }

    /// Wall-clock time elapsed since this `Renderer` was created.
    pub fn get_render_time(&self) -> std::time::Duration {
        self.start_time.elapsed()
    }

    /// Reserved for future per-window state. Currently a no-op.
    pub fn handle_event(&mut self, _event: &WindowEvent) {}
}
