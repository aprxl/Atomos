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
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use wgpu::{CurrentSurfaceTexture, QuerySet};
use winit::{event::WindowEvent, window::Window};

mod alignment;
mod color;
mod draw_command;
mod font;
mod font_parameters;
mod glyph_effects;
mod layer;
mod path_paint;
mod pixels;
mod rounding;
mod shader;
mod text_span;
mod text_stack;

#[cfg(test)]
mod pixel_tests;
#[cfg(test)]
mod test_support;

// `GradientDirection`/`Rgba` are vocabulary for richer `Color` use
// (gradients/per-vertex), not yet exercised by every call site. This is a
// `bin` crate, so nothing outside can reach these `pub` items yet either,
// hence the blanket allow rather than one per item.
pub use alignment::{Alignment, HorizontalAlign, VerticalAlign};
pub use color::Rgba;
pub use color::{Color, GradientDirection, to_linear};
pub use font::Font;
pub use font_parameters::FontParameters;
pub use layer::{Layer, LayerInvalidation};
// Same story as `PathFileError` below: only bins that actually clip by
// shape touch it.
pub use layer::ClipShape;
// Only needed by callers that pattern-match the error instead of just
// `.expect()`/`?`-propagating it (like the current demo does).
pub use layer::PathFileError;
pub use path_paint::{FillRule, LineCap, LineJoin, PathPaint, Stroke};
pub use pixels::Pixels;
pub use rounding::Rounding;
pub use shader::{ColorMatrix, ShaderEffect};
pub use text_span::TextSpan;

/// MSAA sample count for every layer's own render pass (the solid, image,
/// and text pipelines all draw into a multisampled offscreen target,
/// resolved down to the layer's actual single-sample texture — the same
/// technique the old Phase-0 spike used for the swap chain directly).
/// Without this, only axis-aligned edges (rectangles) look clean —
/// anything with a diagonal or curved edge (`draw_path`, `draw_circle`,
/// glyph outlines) shows visible aliasing, and 4x specifically still looks
/// noticeably stair-stepped on longer diagonal edges (a straight line has
/// far fewer, more visually regular "steps" to hide behind than a
/// continuously-curving edge does, at the same sample count) — hence
/// preferring 8x when the adapter actually supports it, picked at runtime
/// by [`pick_msaa_sample_count`] rather than assumed. Shared by
/// `layer.rs` and `text_stack.rs`, which is why the *value* still lives
/// here (as a `Renderer` field, not a `const`) rather than in either one.
///
/// `MULTISAMPLE_X4` is guaranteed for every renderable format by the
/// WebGPU/wgpu spec, so this is always a safe floor; `X8` (and higher) is
/// adapter/format-dependent and must be queried, not assumed — asking for
/// an unsupported sample count is a validation error, and this renderer
/// needs to keep working on adapters that only offer 4x.
const MSAA_SAMPLE_COUNT_FALLBACK: u32 = 4;
/// Ceiling on the preferred sample count — doubling sample count doubles
/// MSAA texture memory/bandwidth for a visual return that flattens out
/// well before 16x on UI-sized geometry, so 8x is where this stops
/// climbing even if an adapter offers more.
const MSAA_SAMPLE_COUNT_MAX: u32 = 8;

/// Pick the highest MSAA sample count (up to [`MSAA_SAMPLE_COUNT_MAX`])
/// this adapter actually supports for `format`, including
/// `resolve_target` support at that count (every layer's render pass
/// resolves its multisampled target automatically, so a sample count the
/// format can't resolve wouldn't help even if multisampling itself were
/// supported at it). Falls back to [`MSAA_SAMPLE_COUNT_FALLBACK`], which
/// every renderable format is required to support.
fn pick_msaa_sample_count(adapter: &wgpu::Adapter, format: wgpu::TextureFormat) -> u32 {
    let flags = adapter.get_texture_format_features(format).flags;
    let can_resolve = flags.contains(wgpu::TextureFormatFeatureFlags::MULTISAMPLE_RESOLVE);
    if can_resolve && flags.sample_count_supported(MSAA_SAMPLE_COUNT_MAX) {
        MSAA_SAMPLE_COUNT_MAX
    } else {
        MSAA_SAMPLE_COUNT_FALLBACK
    }
}

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
    #[allow(dead_code)] // owns the GPU resource its bind group points at
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    // GPU resources shared by every `Layer` (its pipelines, bind group
    // layouts, the composite quad) — built once here rather than per layer.
    layer_pipelines: layer::LayerPipelines,
    // The one multisampled scratch texture every layer's render pass draws
    // into (each resolving into its own single-sample texture). Shared
    // because layer passes run sequentially and the target is transient
    // (cleared per pass, discarded after resolve) — per-layer copies would
    // cost a full-surface 4x texture each. Kept alive for `msaa_view`.
    msaa_texture: wgpu::Texture,
    msaa_view: wgpu::TextureView,
    msaa_size: (u32, u32),
    // Resolved once in `new` via `pick_msaa_sample_count` — every layer's
    // solid/image/text pipeline and the shared MSAA texture above all use
    // this same value, so it's threaded through their constructors rather
    // than assumed as a fixed constant.
    msaa_sample_count: u32,
    // The glyphon/cosmic-text stack every `Layer`'s `draw_text` shares —
    // see `text_stack.rs`'s module doc for why it's one shared instance,
    // not one per layer.
    text_stack: Rc<RefCell<text_stack::TextStack>>,
    // Composite order: front = bottom of the visual stack, back = top.
    // `Weak`, not `Rc` — a layer stops being composited once the caller
    // drops their last `Layer` handle.
    layers: VecDeque<std::rc::Weak<RefCell<layer::LayerInner>>>,
    // GPU frame-duration measurement via timestamp queries — `None` on
    // adapters without timestamp support, in which case
    // `Renderer::get_gpu_frametime` reports `None` and everything else
    // works normally. Deliberately *not* what `get_frametime` (the
    // animation-facing API) is built on — that one is wall-clock and works
    // everywhere.
    timestamps: Option<TimestampState>,
    // Wall-clock instant the previous `render()` finished — the anchor
    // `frame_delta` is measured from. `None` until the first frame.
    previous_frame_at: Option<Instant>,
    // Wall-clock time between the two most recent frames — what
    // `Renderer::get_frametime` returns. Includes vsync pacing, which is
    // exactly what animation stepping needs (GPU-busy time would step
    // animations far too slowly).
    frame_delta: Duration,
    // Wall-clock instant the renderer was created — `get_render_time`
    // reports elapsed time against this.
    start_time: Instant,
    // Current window scale factor (OS DPI setting; 1.0 = 96 DPI, 2.0 = a
    // typical "Retina"/HiDPI display). Every `Layer::draw_*` call takes
    // *logical* pixels and multiplies by this at the call boundary to get
    // the *physical* pixels `DrawCommand`s (and everything downstream —
    // tessellation, glyph rasterization) actually store and render in.
    // Physical-pixel internals means text/paths rasterize at native
    // display density (crisp, no blur from upscaling a logical-resolution
    // render) while callers never have to think about DPI themselves — the
    // same contract CSS px/SwiftUI points/Android dp give their callers.
    scale_factor: f64,
}

/// How many timestamp readback buffers are kept in flight. Each is tiny
/// (16 bytes); three is enough that a slot is essentially always free
/// under `desired_maximum_frame_latency: 2`.
const TIMESTAMP_RING_SLOTS: usize = 3;

/// 2 timestamps * 8 bytes (`QUERY_SIZE`) each.
const TIMESTAMP_BUFFER_SIZE: wgpu::BufferAddress = 16;

// `ReadbackSlot::status` values: the `map_async` callback (which may run on
// another thread) reports completion through an atomic rather than any
// blocking wait.
const SLOT_STATUS_PENDING: u8 = 0;
const SLOT_STATUS_MAPPED: u8 = 1;
const SLOT_STATUS_FAILED: u8 = 2;

/// One buffer of the timestamp readback ring plus its bookkeeping.
struct ReadbackSlot {
    buffer: wgpu::Buffer,
    /// Written by the `map_async` callback, read by
    /// [`TimestampState::collect`].
    status: Arc<AtomicU8>,
    /// A copy into this slot has been recorded and its map requested, but
    /// the result hasn't been consumed yet.
    in_flight: bool,
    /// Which frame's measurement this slot carries — newer wins when
    /// several complete in the same poll.
    serial: u64,
}

/// GPU frame-duration measurement: two timestamps written around each
/// frame's commands, resolved and copied into a small ring of `MAP_READ`
/// buffers that are read back *without ever blocking on the GPU* — the
/// naive map-then-`poll(Wait)` approach fully serializes CPU and GPU (the
/// CPU can't start frame N+1 until frame N finishes executing), silently
/// destroying the pipelining `desired_maximum_frame_latency` exists to
/// provide. Instead each frame requests an async map and a later frame
/// harvests whichever slots have completed (1–3 frames latent — fine for a
/// profiling metric).
struct TimestampState {
    query_set: QuerySet,
    /// Where `resolve_query_set` writes the raw `u64` ticks (reused every
    /// frame; GPU-side ordering makes that safe).
    resolve_buffer: wgpu::Buffer,
    slots: [ReadbackSlot; TIMESTAMP_RING_SLOTS],
    /// Nanoseconds per timestamp tick — device/driver-specific.
    period_ns: f64,
    next_serial: u64,
    /// Highest serial consumed so far — guards against an older in-flight
    /// result overwriting a newer one.
    consumed_serial: u64,
    /// GPU duration of the most recently measured frame, if any sample has
    /// completed yet.
    last_gpu_frametime: Option<Duration>,
}

impl TimestampState {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let query_set = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("atomos-timestamp-query"),
            ty: wgpu::QueryType::Timestamp,
            count: 2,
        });
        let resolve_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("atomos-timestamp-resolve-buffer"),
            size: TIMESTAMP_BUFFER_SIZE,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let slots = std::array::from_fn(|_| ReadbackSlot {
            buffer: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("atomos-timestamp-readback-buffer"),
                size: TIMESTAMP_BUFFER_SIZE,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            status: Arc::new(AtomicU8::new(SLOT_STATUS_PENDING)),
            in_flight: false,
            serial: 0,
        });
        Self {
            query_set,
            resolve_buffer,
            slots,
            period_ns: queue.get_timestamp_period() as f64,
            next_serial: 1,
            consumed_serial: 0,
            last_gpu_frametime: None,
        }
    }

    /// Record the end-of-frame timestamp, resolve both, and copy them into
    /// a free ring slot. Returns the slot to map after submit — `None` when
    /// every slot is still in flight (the GPU is badly behind; this frame
    /// simply goes unmeasured rather than blocking).
    fn record_frame_end(&mut self, encoder: &mut wgpu::CommandEncoder) -> Option<usize> {
        encoder.write_timestamp(&self.query_set, 1);
        encoder.resolve_query_set(&self.query_set, 0..2, &self.resolve_buffer, 0);
        let index = self.slots.iter().position(|slot| !slot.in_flight)?;
        encoder.copy_buffer_to_buffer(
            &self.resolve_buffer,
            0,
            &self.slots[index].buffer,
            0,
            TIMESTAMP_BUFFER_SIZE,
        );
        Some(index)
    }

    /// Request the async map of a slot [`TimestampState::record_frame_end`]
    /// picked. Called after `queue.submit` so the copy is already queued.
    fn begin_map(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.in_flight = true;
        slot.serial = self.next_serial;
        self.next_serial += 1;
        slot.status.store(SLOT_STATUS_PENDING, Ordering::Release);
        let status = slot.status.clone();
        slot.buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let outcome = if result.is_ok() {
                    SLOT_STATUS_MAPPED
                } else {
                    SLOT_STATUS_FAILED
                };
                status.store(outcome, Ordering::Release);
            });
    }

    /// Drive map callbacks with a *non-blocking* poll and consume every
    /// slot that has completed since last frame, keeping the newest
    /// measurement.
    fn collect(&mut self, device: &wgpu::Device) {
        let _ = device.poll(wgpu::PollType::Poll);
        for slot in &mut self.slots {
            if !slot.in_flight {
                continue;
            }
            match slot.status.load(Ordering::Acquire) {
                SLOT_STATUS_MAPPED => {
                    let ticks: [u64; 2] = {
                        let data = slot.buffer.slice(..).get_mapped_range();
                        let ticks: &[u64] = bytemuck::cast_slice(&data);
                        [ticks[0], ticks[1]]
                    };
                    slot.buffer.unmap();
                    slot.in_flight = false;
                    if slot.serial > self.consumed_serial {
                        self.consumed_serial = slot.serial;
                        let elapsed_ns = ticks[1].saturating_sub(ticks[0]) as f64 * self.period_ns;
                        self.last_gpu_frametime = Some(Duration::from_nanos(elapsed_ns as u64));
                    }
                }
                // Map failed (e.g. device loss) — the buffer never mapped,
                // so there's nothing to unmap; just free the slot.
                SLOT_STATUS_FAILED => slot.in_flight = false,
                _ => {}
            }
        }
    }
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
        let scale_factor = window.scale_factor();

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

        // `TIMESTAMP_QUERY_INSIDE_ENCODERS` is needed on top of plain
        // `TIMESTAMP_QUERY` because `render`'s two `write_timestamp` calls
        // happen directly on the `CommandEncoder`, not inside a
        // render/compute pass. Both are *optional*: on adapters without
        // them, the renderer still works fully — only `get_gpu_frametime`
        // degrades to `None` (the animation-facing `get_frametime` is
        // wall-clock and needs no GPU features). Requesting an unsupported
        // feature would fail device creation outright, hence the check.
        let timestamp_features =
            wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
        let timestamps_supported = adapter.features().contains(timestamp_features);

        // `TextureFormatFeatureFlags`/`get_texture_format_features` reports
        // what the adapter's hardware can *physically* do, but wgpu only
        // permits actually using sample counts beyond the WebGPU-guaranteed
        // baseline (4x, for a standard renderable format like this
        // srgb-encoded one) once this feature is explicitly requested and
        // granted — a real error hit while smoke-testing this: "Sample
        // count 8 is not supported by format Rgba8UnormSrgb on this
        // device... With the TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
        // feature your device supports [1, 2, 4, 8]." Same optional-
        // feature shape as `timestamp_features` above: request it only if
        // the adapter actually offers it, and gate any use of >4x MSAA on
        // whether it was actually granted, not just adapter-capable.
        let extended_msaa_supported = adapter
            .features()
            .contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES);

        let mut required_features = wgpu::Features::empty();
        if timestamps_supported {
            required_features |= timestamp_features;
        }
        if extended_msaa_supported {
            required_features |= wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES;
        }

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("atomos-device"),
                required_features,
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

        // Only consider going past the guaranteed 4x floor if the device
        // was actually granted `TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES`
        // above — querying `get_texture_format_features` without that
        // feature enabled would still report the adapter's raw hardware
        // capability, but *using* a sample count beyond 4x without the
        // feature granted is a validation error regardless of what the
        // hardware can physically do.
        let msaa_sample_count = if extended_msaa_supported {
            pick_msaa_sample_count(&adapter, surface_config.format)
        } else {
            MSAA_SAMPLE_COUNT_FALLBACK
        };

        let layer_pipelines =
            layer::LayerPipelines::new(&device, surface_config.format, msaa_sample_count);
        let msaa_size = (surface_config.width, surface_config.height);
        let (msaa_texture, msaa_view) = layer::create_msaa_texture(
            &device,
            surface_config.format,
            msaa_size,
            msaa_sample_count,
        );
        let text_stack = Rc::new(RefCell::new(text_stack::TextStack::new(
            &device,
            &queue,
            surface_config.format,
            (surface_config.width, surface_config.height),
            msaa_sample_count,
        )));
        let layers = VecDeque::new();

        let timestamps = timestamps_supported.then(|| TimestampState::new(&device, &queue));

        Self {
            window,
            device,
            queue,
            surface,
            surface_config,
            layer_pipelines,
            msaa_texture,
            msaa_view,
            msaa_size,
            msaa_sample_count,
            text_stack,
            layers,
            timestamps,
            previous_frame_at: None,
            frame_delta: Duration::ZERO,
            start_time: Instant::now(),
            scale_factor,
        }
    }

    /// Update the window scale factor — call this from
    /// `WindowEvent::ScaleFactorChanged`. Layers pick up the new value
    /// lazily (same pattern as [`Renderer::resize`]/surface size), so this
    /// is just a cheap field write; no GPU resources need rebuilding here
    /// (a `ScaleFactorChanged` is virtually always paired with a `Resized`
    /// carrying the new physical size, which `resize` already handles).
    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        self.scale_factor = scale_factor;
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
            self.scale_factor as f32,
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
            self.scale_factor as f32,
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
            // Still a valid, presentable frame — just not ideally sized.
            // Reconfiguring *here* would be the bug that produced "wgpu
            // error: SurfaceOutput must be dropped before a new Surface is
            // made": `frame` is already an acquired, unpresented
            // `SurfaceTexture`, and wgpu requires any such texture to be
            // presented/dropped *before* `configure` runs again — calling
            // it while still holding `frame` is exactly what's invalid. An
            // actual size change already reaches this renderer correctly
            // through `Renderer::resize` (called from
            // `WindowEvent::Resized`), which runs between frames, never
            // while one is held — so there's nothing to do here but use
            // the frame as given.
            CurrentSurfaceTexture::Suboptimal(frame) => frame,
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

        if let Some(timestamps) = &self.timestamps {
            encoder.write_timestamp(&timestamps.query_set, 0);
        }

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

        // Once per frame (not per layer — every layer shares this stack):
        // tick the shaped-text cache clock and evict stale entries.
        //
        // This clock counts *frames*, not seconds, and frames are demand-
        // driven (see `frame::FrameScheduler`) — so an idle editor stretches
        // the atlas-trim interval out over arbitrary wall-clock time. That's
        // deliberate rather than a bug waiting to happen: idle frames add no
        // new glyphs, so there is nothing accumulating that a time-based
        // trim would catch earlier. Revisit only if atlas growth ever shows
        // up in a profile.
        self.text_stack.borrow_mut().advance_frame();

        // Layers resize lazily against the current surface size (see
        // `layer::render_layers`), so the shared MSAA scratch target does
        // the same here rather than in `resize()`.
        let surface_size = (self.surface_config.width, self.surface_config.height);
        if self.msaa_size != surface_size {
            let (msaa_texture, msaa_view) = layer::create_msaa_texture(
                &self.device,
                self.surface_config.format,
                surface_size,
                self.msaa_sample_count,
            );
            self.msaa_texture = msaa_texture;
            self.msaa_view = msaa_view;
            self.msaa_size = surface_size;
        }

        layer::render_layers(
            &mut encoder,
            &view,
            &self.msaa_view,
            &mut self.layers,
            surface_size,
            self.scale_factor as f32,
        );

        let map_slot = self
            .timestamps
            .as_mut()
            .and_then(|timestamps| timestamps.record_frame_end(&mut encoder));

        self.queue.submit(std::iter::once(encoder.finish()));
        // In wgpu 29, `present` is a method on `SurfaceTexture`; in wgpu 30
        // it moved to `Queue`.
        frame.present();

        if let Some(timestamps) = &mut self.timestamps {
            if let Some(index) = map_slot {
                timestamps.begin_map(index);
            }
            timestamps.collect(&self.device);
        }

        // Wall-clock frame delta, the anchor for `get_frametime`. Measured
        // at the same point every frame so the delta is stable under vsync.
        let now = Instant::now();
        if let Some(previous) = self.previous_frame_at {
            self.frame_delta = now.duration_since(previous);
        }
        self.previous_frame_at = Some(now);

        Ok(())
    }

    /// Wall-clock time between the two most recent frames — the value to
    /// step animations by. Always available on every machine (no GPU
    /// features involved) and includes vsync pacing, so an animation
    /// advanced by this per frame moves in real time. Returns
    /// [`Duration::ZERO`] until two frames have been rendered.
    ///
    /// Not a measure of how hard the GPU is working — see
    /// [`Renderer::get_gpu_frametime`] for that.
    pub fn get_frametime(&self) -> Duration {
        self.frame_delta
    }

    /// GPU execution time of this renderer's own commands for the most
    /// recently *measured* frame — a profiling metric, measured with GPU
    /// timestamp queries and read back asynchronously, so the value is 1–3
    /// frames old. `None` when the adapter doesn't support timestamp
    /// queries, or before the first measurement completes. Use
    /// [`Renderer::get_frametime`] for animation stepping, not this.
    pub fn get_gpu_frametime(&self) -> Option<Duration> {
        self.timestamps
            .as_ref()
            .and_then(|timestamps| timestamps.last_gpu_frametime)
    }

    /// Wall-clock time elapsed since this `Renderer` was created.
    pub fn get_render_time(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Current window scale factor — see [`Renderer::set_scale_factor`].
    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }

    /// Reserved for future per-window state. Currently a no-op.
    pub fn handle_event(&mut self, _event: &WindowEvent) {}
}
