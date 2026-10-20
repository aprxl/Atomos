//! Shader-system proof of concept.
//!
//! Run with `cargo run --bin shader_demo`.
//!
//! A 4x4 matrix: every **surface type** the renderer can draw (geometry,
//! SVG paths, images, text) crossed with every **effect kind** the shader
//! system offers (none, `Blur`, `ColorMatrix`, `Custom` WGSL).
//!
//! The thing this demo is really showing is the architectural unit:
//! **effects apply to a whole layer, not to a draw call.** That is why
//! there are sixteen layers here and not one — each cell needs its own
//! layer to be shaded independently. The slot backgrounds and every label
//! live on a separate unshaded chrome layer underneath, which is why they
//! stay crisp while the content on top of them is blurred or inverted.
//!
//! Controls:
//!
//! - `M` / `Shift+M` — cycle the `ColorMatrix` column forward/backward
//! - `B` — pause/resume the animated blur radius
//! - `C` — toggle the `Custom` WGSL column between the RGB-split shader and
//!   a deliberately broken one, to show that a bad shader logs and drops
//!   the effect instead of taking the renderer down
//! - `Escape` — quit
//!
//! The footer reports CPU and GPU frametime. On this machine the twelve
//! active effects add roughly 340 us -> 510-770 us of GPU time on top of
//! the 17-layer composite, and cost nothing measurable on the CPU even with
//! the blur radius animating (which re-runs `set_effect` every frame). See
//! `Demo::apply_effects`.

use std::sync::Arc;
use std::time::Duration;

use atomos::animation::{Animation, Easing};
use atomos::frame::FrameScheduler;
use atomos::input::Input;
use atomos::renderer::{
    Alignment, Color, ColorMatrix, Font, FontParameters, GradientDirection, Layer,
    LayerInvalidation, PathPaint, Pixels, Renderer, Rounding, ShaderEffect, Stroke,
};
use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, DeviceId, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::KeyCode,
    window::{Window, WindowId},
};

// ---------------------------------------------------------------- layout

const WIDTH: f32 = 1180.0;
const HEIGHT: f32 = 840.0;

const LABEL_W: f32 = 130.0;
const GRID_X: f32 = LABEL_W + 8.0;
const GRID_Y: f32 = 98.0;
const CELL_W: f32 = 252.0;
const CELL_H: f32 = 168.0;
const CELL_GAP: f32 = 4.0;
/// Content inset inside a cell — the slot background is drawn at the full
/// cell rect on the chrome layer, the surface at the inset rect on its own.
const INSET: f32 = 10.0;

const ROWS: usize = 4;
const COLS: usize = 4;

const ROW_LABELS: [&str; ROWS] = ["GEOMETRY", "SVG PATH", "IMAGE", "TEXT"];
const COL_LABELS: [&str; COLS] = ["no effect", "Blur", "ColorMatrix", "Custom WGSL"];

const BG: Color = Color::rgb(0x1E, 0x21, 0x27);
const PANEL: Color = Color::rgb(0x28, 0x2C, 0x34);
const SLOT: Color = Color::rgb(0x21, 0x25, 0x2B);
const TEXT: Color = Color::rgb(0xAB, 0xB2, 0xBF);
const DIM: Color = Color::rgb(0x5C, 0x63, 0x70);
const BLUE: Color = Color::rgb(0x61, 0xAF, 0xEF);
const GREEN: Color = Color::rgb(0x98, 0xC3, 0x79);
const PURPLE: Color = Color::rgb(0xC6, 0x78, 0xDD);
const YELLOW: Color = Color::rgb(0xE5, 0xC0, 0x7B);
const RED: Color = Color::rgb(0xE0, 0x6C, 0x75);

/// Top-left of the cell at `(row, col)`.
fn cell_origin(row: usize, col: usize) -> (f32, f32) {
    (
        GRID_X + col as f32 * (CELL_W + CELL_GAP),
        GRID_Y + row as f32 * (CELL_H + CELL_GAP),
    )
}

// ---------------------------------------------------------------- assets

/// A five-pointed star in a 24x24 viewBox, for `draw_svg_icon`.
const STAR: &str = "M12 2 L14.47 8.60 L21.51 8.91 L15.99 13.30 L17.88 20.09 \
                    L12 16.2 L6.12 20.09 L8.01 13.30 L2.49 8.91 L9.53 8.60 Z";

/// The `Custom` WGSL contract from `ShaderEffect::Custom`'s doc, with only
/// `fs_main`'s body changed: an RGB channel split plus scanlines. Chosen
/// because it deforms *coverage*, so its effect on a solid rectangle, an
/// antialiased path edge, a photo, and glyph stems all look different.
const RGB_SPLIT: &str = r#"
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
    let shift = vec2<f32>(0.0035, 0.0);
    let r = textureSampleLevel(tex, samp, in.uv + shift, 0.0);
    let g = textureSampleLevel(tex, samp, in.uv, 0.0);
    let b = textureSampleLevel(tex, samp, in.uv - shift, 0.0);
    let scan = 0.78 + 0.22 * sin(in.clip_pos.y * 1.7);
    // Straight (non-premultiplied) alpha — the composite pass blends with
    // wgpu's ALPHA_BLENDING, so the widest of the three taps keeps the
    // split visible past the original silhouette instead of clipping to it.
    let a = max(r.a, max(g.a, b.a));
    return vec4<f32>(vec3<f32>(r.r, g.g, b.b) * scan, a);
}
"#;

/// Deliberately invalid WGSL, for the `C` toggle: `set_effect` compiles
/// custom shaders inside a wgpu error scope, so this logs and leaves the
/// layer unshaded rather than panicking.
const BROKEN: &str = "@fragment fn fs_main() -> f32 { return not_a_thing(); }";

/// The `ColorMatrix` column's cycle. Every named constructor the shader
/// module ships, plus one hand-written 4x5 to show the raw escape hatch.
fn matrices() -> Vec<(&'static str, ColorMatrix)> {
    vec![
        ("grayscale()", ColorMatrix::grayscale()),
        ("invert()", ColorMatrix::invert()),
        ("sepia()", ColorMatrix::sepia()),
        ("saturate(2.5)", ColorMatrix::saturate(2.5)),
        ("saturate(0.4)", ColorMatrix::saturate(0.4)),
        ("brightness(1.5)", ColorMatrix::brightness(1.5)),
        ("contrast(1.9)", ColorMatrix::contrast(1.9)),
        ("tint(purple, 0.55)", ColorMatrix::tint(PURPLE, 0.55)),
        (
            "raw 4x5 — channel rotate",
            ColorMatrix([
                0.0, 0.0, 1.0, 0.0, 0.0, //
                1.0, 0.0, 0.0, 0.0, 0.0, //
                0.0, 1.0, 0.0, 0.0, 0.0, //
                0.0, 0.0, 0.0, 1.0, 0.0,
            ]),
        ),
    ]
}

/// A checkerboard under a hue sweep, generated rather than loaded so the
/// demo needs no bundled asset. Fine detail on purpose — it is the clearest
/// readout of what a blur radius is actually doing.
fn test_image() -> Pixels {
    const W: u32 = 120;
    const H: u32 = 120;
    const CHECK: u32 = 10;

    let mut data = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            let dark = ((x / CHECK) + (y / CHECK)).is_multiple_of(2);
            let u = x as f32 / W as f32;
            let v = y as f32 / H as f32;
            // Cheap three-phase sweep — no color-space math needed, the
            // point is just saturated variation across both axes.
            let base = [
                (255.0 * (0.5 + 0.5 * (u * 6.0).sin())) as u8,
                (255.0 * (0.5 + 0.5 * (v * 6.0 + 2.0).sin())) as u8,
                (255.0 * (0.5 + 0.5 * ((u + v) * 4.0 + 4.0).sin())) as u8,
            ];
            let scale = if dark { 0.45 } else { 1.0 };
            data.extend_from_slice(&[
                (base[0] as f32 * scale) as u8,
                (base[1] as f32 * scale) as u8,
                (base[2] as f32 * scale) as u8,
                0xFF,
            ]);
        }
    }
    Pixels::new(W, H, data)
}

// ------------------------------------------------------------------ demo

struct Demo {
    image: Pixels,
    matrices: Vec<(&'static str, ColorMatrix)>,
    matrix: usize,
    blur: Animation,
    custom_broken: bool,
    /// Last effect handed to each column, so `set_effect` is only called on
    /// an actual change — see `apply_effects`.
    applied: [Option<ShaderEffect>; COLS],
    frames: u64,
}

impl Demo {
    fn new() -> Self {
        Self {
            image: test_image(),
            matrices: matrices(),
            matrix: 0,
            blur: Animation::new(Duration::from_millis(1800), Easing::EaseInOut).ping_pong(),
            custom_broken: false,
            applied: [const { None }; COLS],
            frames: 0,
        }
    }

    /// Fold input and time into demo state. Returns whether anything is
    /// still animating, which is what keeps the scheduler asking for frames.
    fn update(&mut self, input: &Input, dt: Duration) -> bool {
        self.frames += 1;

        if input.is_key_pressed(KeyCode::KeyM) {
            let len = self.matrices.len();
            self.matrix = if input.shift() {
                (self.matrix + len - 1) % len
            } else {
                (self.matrix + 1) % len
            };
        }
        if input.is_key_pressed(KeyCode::KeyB) {
            if self.blur.is_playing() {
                self.blur.pause();
            } else {
                self.blur.resume();
            }
        }
        if input.is_key_pressed(KeyCode::KeyC) {
            self.custom_broken = !self.custom_broken;
        }

        self.blur.advance(dt)
    }

    fn blur_radius(&self) -> f32 {
        self.blur.value(0.0, 16.0)
    }

    /// Push each column's effect onto all four of its layers, skipping the
    /// call when the value is unchanged.
    ///
    /// `Layer::set_effect` rebuilds that layer's effect GPU resources
    /// eagerly (textures, buffers, bind groups — and for `Custom`, a full
    /// WGSL compile), and its doc frames that as a rare caller-driven
    /// operation. An animated blur radius is the one case here that hits it
    /// every frame, on four layers at once — so it is worth knowing that
    /// measurement did *not* find a cost: CPU frametime is identical with
    /// the radius animating and with it pinned. The skip stays because it
    /// is free, not because it was rescuing anything.
    ///
    /// The real shape of the cost is in GPU time (footer): ~340 us for
    /// these 17 layers with no effects at all, ~510-770 us with all twelve
    /// active, the spread tracking the blur radius. Compositing the layers
    /// is the floor; the effects are the cheap part.
    fn apply_effects(&mut self, cells: &[Layer]) {
        let wanted: [Option<ShaderEffect>; COLS] = [
            None,
            Some(ShaderEffect::Blur {
                radius: self.blur_radius(),
            }),
            Some(ShaderEffect::ColorMatrix(self.matrices[self.matrix].1)),
            Some(ShaderEffect::Custom(if self.custom_broken {
                BROKEN
            } else {
                RGB_SPLIT
            })),
        ];

        for (col, effect) in wanted.into_iter().enumerate() {
            if self.applied[col] == effect {
                continue;
            }
            for row in 0..ROWS {
                cells[row * COLS + col].set_effect(effect.clone());
            }
            self.applied[col] = effect;
        }
    }

    /// Everything that is *not* shaded: background, slot rects, labels,
    /// header, footer. One layer, no effect — the crisp reference the
    /// shaded cells sit on top of.
    fn draw_chrome(&self, layer: &Layer, frametime: Duration, gpu: Option<Duration>) {
        layer.draw_rectangle((0.0, 0.0), (WIDTH, HEIGHT), BG, Rounding::NONE);
        layer.draw_rectangle((0.0, 0.0), (WIDTH, 66.0), PANEL, Rounding::NONE);

        let label = FontParameters::new(13.0);
        let mono = Font::Named("monospace".into());

        layer.draw_text(
            "Atomos — shader system",
            (24.0, 16.0),
            TEXT,
            Alignment::TOP_LEFT,
            mono.clone(),
            FontParameters {
                weight: 1.2,
                ..FontParameters::new(19.0)
            },
        );
        layer.draw_text(
            "effects are per-LAYER, not per-draw-call — 16 cells means 16 layers",
            (24.0, 42.0),
            DIM,
            Alignment::TOP_LEFT,
            mono.clone(),
            FontParameters::new(12.0),
        );

        for (col, name) in COL_LABELS.iter().enumerate() {
            let (x, _) = cell_origin(0, col);
            layer.draw_text(
                name,
                (x + CELL_W / 2.0, 84.0),
                if col == 0 { DIM } else { BLUE },
                Alignment::CENTER,
                mono.clone(),
                label,
            );
        }

        for (row, name) in ROW_LABELS.iter().enumerate() {
            let (_, y) = cell_origin(row, 0);
            layer.draw_text(
                name,
                (LABEL_W - 12.0, y + CELL_H / 2.0),
                TEXT,
                Alignment {
                    horizontal: atomos::renderer::HorizontalAlign::Right,
                    vertical: atomos::renderer::VerticalAlign::Center,
                },
                mono.clone(),
                label,
            );
            for col in 0..COLS {
                let origin = cell_origin(row, col);
                layer.draw_rectangle(origin, (CELL_W, CELL_H), SLOT, Rounding::uniform(8.0));
            }
        }

        // Footer.
        let foot_y = GRID_Y + ROWS as f32 * (CELL_H + CELL_GAP) + 8.0;
        layer.draw_rectangle(
            (0.0, foot_y),
            (WIDTH, HEIGHT - foot_y),
            PANEL,
            Rounding::NONE,
        );

        let animating = self.blur.is_playing();
        layer.draw_text(
            &format!(
                "Blur {{ radius: {:>5.2} }}   [{}]",
                self.blur_radius(),
                if animating {
                    "running — B pauses"
                } else {
                    "paused — B resumes"
                }
            ),
            (24.0, foot_y + 12.0),
            if animating { YELLOW } else { GREEN },
            Alignment::TOP_LEFT,
            mono.clone(),
            FontParameters::new(13.0),
        );
        layer.draw_text(
            &format!(
                "ColorMatrix::{}   ({}/{}, M cycles)",
                self.matrices[self.matrix].0,
                self.matrix + 1,
                self.matrices.len()
            ),
            (24.0, foot_y + 32.0),
            PURPLE,
            Alignment::TOP_LEFT,
            mono.clone(),
            FontParameters::new(13.0),
        );
        layer.draw_text(
            &format!(
                "Custom: {}   |   frame {}   |   cpu {:.2} ms   |   gpu {}",
                if self.custom_broken {
                    "BROKEN WGSL (C)"
                } else {
                    "rgb split (C breaks it)"
                },
                self.frames,
                frametime.as_secs_f64() * 1000.0,
                match gpu {
                    Some(g) => format!("{:.2} ms", g.as_secs_f64() * 1000.0),
                    None => "n/a".to_string(),
                }
            ),
            (WIDTH - 24.0, foot_y + 22.0),
            if self.custom_broken { RED } else { DIM },
            Alignment {
                horizontal: atomos::renderer::HorizontalAlign::Right,
                vertical: atomos::renderer::VerticalAlign::Center,
            },
            mono,
            FontParameters::new(13.0),
        );
    }

    /// Draw one cell's surface. `row` picks which surface type; the effect
    /// is already on the layer, so this code is entirely effect-unaware —
    /// which is the point of shading at the layer level.
    fn draw_surface(&self, layer: &Layer, row: usize, col: usize) {
        let (cx, cy) = cell_origin(row, col);
        let (x, y) = (cx + INSET, cy + INSET);

        match row {
            0 => {
                layer.draw_rectangle(
                    (x + 2.0, y + 6.0),
                    (104.0, 62.0),
                    Color::gradient(
                        [0x61, 0xAF, 0xEF, 0xFF],
                        [0xC6, 0x78, 0xDD, 0xFF],
                        GradientDirection::Horizontal,
                    ),
                    Rounding::uniform(14.0),
                );
                layer.draw_circle((x + 172.0, y + 37.0), 31.0, GREEN);
                // Bilinear corner blend — the one draw call that takes
                // `Color::PerVertex`.
                layer.draw_rectangle(
                    (x + 2.0, y + 82.0),
                    (208.0, 54.0),
                    Color::PerVertex([
                        [0xE5, 0xC0, 0x7B, 0xFF],
                        [0xE0, 0x6C, 0x75, 0xFF],
                        [0x61, 0xAF, 0xEF, 0xFF],
                        [0x98, 0xC3, 0x79, 0xFF],
                    ]),
                    Rounding::uniform(8.0),
                );
            }
            1 => {
                let _ = layer.draw_svg_icon(
                    STAR,
                    (0.0, 0.0, 24.0, 24.0),
                    (96.0, 96.0),
                    (x + 2.0, y + 24.0),
                    PathPaint::fill(Color::gradient(
                        [0xE5, 0xC0, 0x7B, 0xFF],
                        [0xE0, 0x6C, 0x75, 0xFF],
                        GradientDirection::Vertical,
                    )),
                );
                let _ = layer.draw_svg_icon(
                    STAR,
                    (0.0, 0.0, 24.0, 24.0),
                    (96.0, 96.0),
                    (x + 118.0, y + 24.0),
                    PathPaint::Stroke(Stroke::new(BLUE, 2.5)),
                );
                layer.draw_text(
                    "fill / stroke",
                    (x + 4.0, y + 2.0),
                    DIM,
                    Alignment::TOP_LEFT,
                    Font::Named("monospace".into()),
                    FontParameters::new(11.0),
                );
            }
            2 => {
                // `draw_image` takes `Pixels` by value, so each call clones
                // the buffer — 120x120 here, cheap enough to ignore.
                layer.draw_image(
                    self.image.clone(),
                    (x + 2.0, y + 12.0),
                    (120.0, 120.0),
                    Color::rgb(0xFF, 0xFF, 0xFF),
                );
                layer.draw_image(
                    self.image.clone(),
                    (x + 130.0, y + 12.0),
                    (82.0, 120.0),
                    BLUE,
                );
            }
            _ => {
                let mono = Font::Named("monospace".into());
                layer.draw_text(
                    "Atomos",
                    (x + 4.0, y + 4.0),
                    TEXT,
                    Alignment::TOP_LEFT,
                    mono.clone(),
                    FontParameters {
                        weight: 1.6,
                        ..FontParameters::new(34.0)
                    },
                );
                layer.draw_text(
                    "shaders hit glyph coverage",
                    (x + 4.0, y + 50.0),
                    BLUE,
                    Alignment::TOP_LEFT,
                    mono.clone(),
                    FontParameters::new(14.0),
                );
                layer.draw_text(
                    "0123456789 {}[]<>",
                    (x + 4.0, y + 76.0),
                    GREEN,
                    Alignment::TOP_LEFT,
                    mono.clone(),
                    FontParameters {
                        tracking: 0.08,
                        ..FontParameters::new(15.0)
                    },
                );
                layer.draw_text(
                    "condensed, 9px, the hard case",
                    (x + 4.0, y + 106.0),
                    DIM,
                    Alignment::TOP_LEFT,
                    mono,
                    FontParameters {
                        width: 0.85,
                        ..FontParameters::new(11.0)
                    },
                );
            }
        }
    }
}

// ------------------------------------------------------------------- app

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    chrome: Option<Layer>,
    /// Row-major, `ROWS * COLS`. One layer per cell is not extravagance —
    /// it is the minimum the shader system's granularity allows.
    cells: Vec<Layer>,
    input: Input,
    scheduler: FrameScheduler,
    demo: Demo,
}

impl App {
    fn new() -> Self {
        Self {
            window: None,
            renderer: None,
            chrome: None,
            cells: Vec::new(),
            input: Input::new(1.0),
            scheduler: FrameScheduler::new(),
            demo: Demo::new(),
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Atomos — shader demo")
                        .with_inner_size(winit::dpi::LogicalSize::new(WIDTH, HEIGHT)),
                )
                .expect("failed to create window"),
        );
        let mut renderer = pollster::block_on(Renderer::new(window.clone()));

        // Chrome first so it sits at the bottom of the stack; every cell
        // layer is created above it.
        let chrome = renderer.new_layer_top(LayerInvalidation::Automatic);
        let cells = (0..ROWS * COLS)
            .map(|_| renderer.new_layer_top(LayerInvalidation::Automatic))
            .collect();

        self.input = Input::new(window.scale_factor());
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.chrome = Some(chrome);
        self.cells = cells;
    }

    fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
        self.input.handle_device_event(&event);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if self.input.handle_event(&event) {
            self.scheduler.request_redraw();
        }
        if let Some(r) = &mut self.renderer {
            r.handle_event(&event);
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
                self.scheduler.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(r) = &mut self.renderer {
                    r.set_scale_factor(scale_factor);
                }
                self.scheduler.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                if self.input.is_key_pressed(KeyCode::Escape) {
                    event_loop.exit();
                    return;
                }
                self.scheduler.begin_frame();

                let (frametime, gpu) = self.renderer.as_ref().map_or((Duration::ZERO, None), |r| {
                    (r.get_frametime(), r.get_gpu_frametime())
                });
                if self
                    .demo
                    .update(&self.input, FrameScheduler::animation_delta(frametime))
                {
                    self.scheduler.request_redraw();
                }

                if let (Some(r), Some(chrome)) = (&mut self.renderer, &self.chrome) {
                    self.demo.apply_effects(&self.cells);
                    self.demo.draw_chrome(chrome, frametime, gpu);
                    for row in 0..ROWS {
                        for col in 0..COLS {
                            self.demo
                                .draw_surface(&self.cells[row * COLS + col], row, col);
                        }
                    }
                    if let Err(e) = r.render() {
                        eprintln!("render error: {e}");
                    }
                }

                self.input.end_frame();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.scheduler.should_draw_now()
            && let Some(w) = &self.window
        {
            w.request_redraw();
        }
        event_loop.set_control_flow(self.scheduler.control_flow());
    }
}

fn main() -> Result<(), winit::error::EventLoopError> {
    let event_loop = EventLoop::new().expect("failed to create event loop");
    let mut app = App::new();
    event_loop.run_app(&mut app)
}
