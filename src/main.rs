//! Atomos — Phase 0 entry point.
//!
//! Owns the winit event loop and delegates all rendering to
//! [`renderer::Renderer`]. Window event types the renderer needs to see are
//! forwarded through `Renderer::handle_event`; window resize, close request,
//! and redraw are handled here so the renderer stays platform-agnostic.

mod renderer;

use std::sync::Arc;

use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};

use renderer::{
    Alignment, Color, Font, FontParameters, GradientDirection, Layer, LayerInvalidation, Pixels,
    Rounding,
};

// Demo shapes proving `Layer` draw calls work from application code — not
// renderer internals — and don't overlap on screen.
const TOP_LAYER_RECT_POS: (f32, f32) = (400.0, 560.0);
const TOP_LAYER_RECT_SIZE: (f32, f32) = (120.0, 80.0);
const BOTTOM_LAYER_RECT_POS: (f32, f32) = (550.0, 560.0);
const BOTTOM_LAYER_RECT_SIZE: (f32, f32) = (120.0, 80.0);
const TOP_LAYER_TRIANGLE: [(f32, f32); 3] = [(700.0, 640.0), (760.0, 560.0), (820.0, 640.0)];
const TOP_LAYER_CIRCLE_CENTER: (f32, f32) = (900.0, 600.0);
const TOP_LAYER_CIRCLE_RADIUS: f32 = 40.0;
// A filled triangle authored in a 24x24 icon viewBox — standard
// icon-authoring convention, good enough to prove an SVG-path-sourced
// shape renders through `draw_path`.
const TOP_LAYER_SVG_PATH: &str = "M12 2 L22 20 L2 20 Z";
const TOP_LAYER_SVG_PATH_POS: (f32, f32) = (960.0, 560.0);
// Same path data, on disk, to prove `draw_path_from_file`.
const BOTTOM_LAYER_SVG_PATH_FILE: &str = "assets/demo-triangle.path";
const BOTTOM_LAYER_SVG_PATH_POS: (f32, f32) = (400.0, 650.0);
const BOTTOM_LAYER_IMAGE_POS: (f32, f32) = (500.0, 650.0);
const BOTTOM_LAYER_IMAGE_SIZE: (f32, f32) = (64.0, 64.0);

// Color::Gradient / Color::PerVertex showcase — a separate row, well clear
// of everything above.
const SHOWCASE_GRADIENT_H_RECT_POS: (f32, f32) = (50.0, 60.0);
const SHOWCASE_GRADIENT_H_RECT_SIZE: (f32, f32) = (160.0, 100.0);
const SHOWCASE_GRADIENT_V_RECT_POS: (f32, f32) = (240.0, 60.0);
const SHOWCASE_GRADIENT_V_RECT_SIZE: (f32, f32) = (160.0, 100.0);
const SHOWCASE_PERVERTEX_RECT_POS: (f32, f32) = (430.0, 60.0);
const SHOWCASE_PERVERTEX_RECT_SIZE: (f32, f32) = (160.0, 100.0);
const SHOWCASE_GRADIENT_CIRCLE_CENTER: (f32, f32) = (700.0, 110.0);
const SHOWCASE_GRADIENT_CIRCLE_RADIUS: f32 = 55.0;
const SHOWCASE_GRADIENT_TRIANGLE: [(f32, f32); 3] = [(820.0, 160.0), (900.0, 60.0), (980.0, 160.0)];

// draw_text showcase — a system font, a bundled font with faux-bold and
// faux-condense, and a `get_text_size`-sized background box demonstrating
// `Alignment::CENTER`. Own row, clear of everything above/below.
const TX02_FONT_BYTES: &[u8] = include_bytes!("../assets/TX-02-Book.ttf");
const SHOWCASE_TEXT_SYSTEM_POS: (f32, f32) = (50.0, 300.0);
const SHOWCASE_TEXT_WEIGHT_POS: (f32, f32) = (50.0, 340.0);
const SHOWCASE_TEXT_CONDENSE_POS: (f32, f32) = (50.0, 380.0);
const SHOWCASE_TEXT_BOXED_ANCHOR: (f32, f32) = (600.0, 475.0);

/// A small procedural RGBA8 checkerboard, so the `draw_image` demo doesn't
/// need a bundled image asset.
fn checkerboard_rgba(size: u32, cell: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let on = ((x / cell) + (y / cell)).is_multiple_of(2);
            let (r, g, b) = if on { (0xE0, 0xC0, 0x40) } else { (0x30, 0x30, 0x38) };
            pixels.extend_from_slice(&[r, g, b, 0xFF]);
        }
    }
    pixels
}

/// Application state. The window and renderer are `None` until `resumed` runs.
struct App {
    window: Option<Arc<Window>>,
    renderer: Option<renderer::Renderer>,
    // `Automatic`: redrawn every frame in `window_event` below, proving an
    // unchanged frame's worth of `draw_*` calls gets diffed away internally.
    top_layer: Option<Layer>,
    // `Manual`: drawn once in `resumed`, never touched again. Still held
    // here (rather than dropped) — dropping it would remove the layer from
    // the renderer's registry and it would vanish from the screen.
    #[allow(dead_code)]
    bottom_layer: Option<Layer>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            window: None,
            renderer: None,
            top_layer: None,
            bottom_layer: None,
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Atomos — Phase 0")
                        .with_inner_size(winit::dpi::LogicalSize::new(1024, 720)),
                )
                .expect("failed to create window"),
        );
        let mut renderer = pollster::block_on(renderer::Renderer::new(window.clone()));

        let top_layer = renderer.new_layer_top(LayerInvalidation::Automatic);
        let bottom_layer = renderer.new_layer_bottom(LayerInvalidation::Manual);
        // `Manual` layers persist their content until `clear()`, so this
        // never needs to run again.
        bottom_layer.draw_rectangle(
            BOTTOM_LAYER_RECT_POS,
            BOTTOM_LAYER_RECT_SIZE,
            Color::rgb(0xE0, 0x78, 0x3C),
            Rounding::NONE,
        );
        bottom_layer
            .draw_path_from_file(
                std::path::Path::new(BOTTOM_LAYER_SVG_PATH_FILE),
                BOTTOM_LAYER_SVG_PATH_POS,
                Color::rgb(0x80, 0xA0, 0xC0),
            )
            .expect("the bundled demo path file should exist and parse");
        bottom_layer.draw_image(
            Pixels::new(32, 32, checkerboard_rgba(32, 8)),
            BOTTOM_LAYER_IMAGE_POS,
            BOTTOM_LAYER_IMAGE_SIZE,
            Color::rgb(0xFF, 0xFF, 0xFF), // untinted
        );

        self.window = Some(window);
        self.renderer = Some(renderer);
        self.top_layer = Some(top_layer);
        self.bottom_layer = Some(bottom_layer);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        // Forward to the renderer first so it can keep any per-window state
        // in sync (currently a no-op, but the seam is in place).
        if let Some(r) = &mut self.renderer {
            r.handle_event(&event);
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                // `Automatic` layers expect their full content re-described
                // every frame — this call is identical every time, so
                // `Layer` should hash it to the same value each frame and
                // skip rebuilding after the first.
                if let Some(layer) = &self.top_layer {
                    layer.draw_rectangle(
                        TOP_LAYER_RECT_POS,
                        TOP_LAYER_RECT_SIZE,
                        Color::rgba(0x64, 0x64, 0xC8, 0xFF),
                        Rounding::uniform(12.0),
                    );
                    layer.draw_polygon(&TOP_LAYER_TRIANGLE, Color::rgb(0x50, 0xB0, 0x70));
                    layer.draw_circle(
                        TOP_LAYER_CIRCLE_CENTER,
                        TOP_LAYER_CIRCLE_RADIUS,
                        Color::rgb(0xD0, 0xB0, 0x40),
                    );
                    layer
                        .draw_path(
                            TOP_LAYER_SVG_PATH,
                            TOP_LAYER_SVG_PATH_POS,
                            Color::rgb(0xC0, 0x80, 0xF0),
                        )
                        .expect("the hardcoded demo SVG path is valid path syntax");

                    // Color::Gradient / Color::PerVertex showcase.
                    layer.draw_rectangle(
                        SHOWCASE_GRADIENT_H_RECT_POS,
                        SHOWCASE_GRADIENT_H_RECT_SIZE,
                        Color::Gradient {
                            stops: [[0xE0, 0x40, 0x40, 0xFF], [0x40, 0x40, 0xE0, 0xFF]],
                            direction: GradientDirection::Horizontal,
                        },
                        Rounding::uniform(10.0),
                    );
                    layer.draw_rectangle(
                        SHOWCASE_GRADIENT_V_RECT_POS,
                        SHOWCASE_GRADIENT_V_RECT_SIZE,
                        Color::Gradient {
                            stops: [[0x40, 0xE0, 0x40, 0xFF], [0xE0, 0xE0, 0x40, 0xFF]],
                            direction: GradientDirection::Vertical,
                        },
                        Rounding::uniform(10.0),
                    );
                    layer.draw_rectangle(
                        SHOWCASE_PERVERTEX_RECT_POS,
                        SHOWCASE_PERVERTEX_RECT_SIZE,
                        Color::PerVertex([
                            [0xE0, 0x40, 0x40, 0xFF], // top-left: red
                            [0x40, 0xE0, 0x40, 0xFF], // top-right: green
                            [0x40, 0x40, 0xE0, 0xFF], // bottom-right: blue
                            [0xE0, 0xE0, 0x40, 0xFF], // bottom-left: yellow
                        ]),
                        Rounding::NONE,
                    );
                    layer.draw_circle(
                        SHOWCASE_GRADIENT_CIRCLE_CENTER,
                        SHOWCASE_GRADIENT_CIRCLE_RADIUS,
                        Color::Gradient {
                            stops: [[0xF0, 0x80, 0xC0, 0xFF], [0x40, 0xC0, 0xF0, 0xFF]],
                            direction: GradientDirection::Vertical,
                        },
                    );
                    layer.draw_polygon(
                        &SHOWCASE_GRADIENT_TRIANGLE,
                        Color::Gradient {
                            stops: [[0xFF, 0xA0, 0x40, 0xFF], [0x40, 0x80, 0xFF, 0xFF]],
                            direction: GradientDirection::Horizontal,
                        },
                    );

                    // draw_text showcase.
                    layer.draw_text(
                        "System font (sans-serif)",
                        SHOWCASE_TEXT_SYSTEM_POS,
                        Color::rgb(0xE6, 0xE6, 0xE6),
                        Alignment::TOP_LEFT,
                        Font::Named("sans-serif".to_string()),
                        FontParameters::new(28.0),
                    );
                    layer.draw_text(
                        "TX-02, faux-bold + tracking",
                        SHOWCASE_TEXT_WEIGHT_POS,
                        Color::rgb(0xE6, 0xE6, 0xE6),
                        Alignment::TOP_LEFT,
                        Font::Bytes(TX02_FONT_BYTES),
                        FontParameters { size: 28.0, weight: 1.4, width: 1.0, tracking: 0.2 },
                    );
                    layer.draw_text(
                        "TX-02, \u{2208}",
                        SHOWCASE_TEXT_CONDENSE_POS,
                        Color::rgb(0xE6, 0xE6, 0xE6),
                        Alignment::TOP_LEFT,
                        Font::Bytes(TX02_FONT_BYTES),
                        FontParameters { size: 28.0, weight: 0.0, width: 0.72, tracking: -0.1 },
                    );

                    // `get_text_size` showcase: size a background box to
                    // fit a centered label, using the same measurement
                    // `draw_text` resolves `Alignment::CENTER` from.
                    let boxed_text = "Centered, boxed via get_text_size";
                    let boxed_font = Font::Named("sans-serif".to_string());
                    let mut boxed_params = FontParameters::new(22.0);
                    boxed_params.weight = 0.8;
                    boxed_params.tracking = 0.4;
                    let (text_w, text_h) = layer.get_text_size(boxed_text, &boxed_font, &boxed_params);
                    const BOX_PADDING: f32 = 14.0;
                    layer.draw_rectangle(
                        (
                            SHOWCASE_TEXT_BOXED_ANCHOR.0 - text_w / 2.0 - BOX_PADDING,
                            SHOWCASE_TEXT_BOXED_ANCHOR.1 - text_h / 2.0 - BOX_PADDING,
                        ),
                        (text_w + BOX_PADDING * 2.0, text_h + BOX_PADDING * 2.0),
                        Color::rgba(0x20, 0x20, 0x28, 0xD0),
                        Rounding::uniform(8.0),
                    );
                    layer.draw_text(
                        boxed_text,
                        SHOWCASE_TEXT_BOXED_ANCHOR,
                        Color::rgb(0xFF, 0xFF, 0xFF),
                        Alignment::CENTER,
                        boxed_font,
                        boxed_params,
                    );
                }
                if let Some(r) = &mut self.renderer {
                    if let Err(e) = r.render() {
                        eprintln!("render error: {e}");
                    }
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }
}

fn main() -> Result<(), winit::error::EventLoopError> {
    let event_loop = EventLoop::new().expect("failed to create event loop");
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
    let mut app = App::default();
    event_loop.run_app(&mut app)
}
