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
    Rounding, TextSpan,
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

// draw_styled_text showcase — a fake syntax-highlighted line (one
// `TextSpan` per token: keyword/name/punctuation/type/number, each with
// its own color, and faux-bold/condense on top of that) shaped as a
// single call, plus a `get_styled_text_size`-boxed label proving the
// spans counterpart to `get_text_size` measures correctly too. Sits in
// the gap between the gradient row and the plain-text row above.
const SHOWCASE_STYLED_TEXT_POS: (f32, f32) = (50.0, 200.0);
const SHOWCASE_STYLED_TEXT_BOXED_ANCHOR: (f32, f32) = (700.0, 215.0);

/// A small procedural RGBA8 checkerboard, so the `draw_image` demo doesn't
/// need a bundled image asset.
fn checkerboard_rgba(size: u32, cell: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let on = ((x / cell) + (y / cell)).is_multiple_of(2);
            let (r, g, b) = if on {
                (0xE0, 0xC0, 0x40)
            } else {
                (0x30, 0x30, 0x38)
            };
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

        // `draw_styled_text` showcase: one call, one shape operation,
        // every token independently colored — plus faux-bold on the
        // keyword/number and faux-condense on punctuation/the arrow, to
        // prove `FontParameters` still varies per span even though
        // cosmic-text has no native per-span concept for it (see
        // `glyph_effects.rs`/`text_stack.rs`'s `shape_spans`).
        const CODE_SIZE: f32 = 24.0;
        const CONDENSED: FontParameters = FontParameters {
            size: CODE_SIZE,
            weight: 0.0,
            width: 0.75,
            tracking: 0.0,
        };
        bottom_layer.draw_styled_text(
            &[
                TextSpan::new(
                    "fn ",
                    Font::Bytes(TX02_FONT_BYTES),
                    FontParameters {
                        size: CODE_SIZE,
                        weight: 1.3,
                        width: 1.0,
                        tracking: 0.0,
                    },
                    Color::rgb(0xC5, 0x8C, 0xFF), // keyword: violet, bold
                ),
                TextSpan::new(
                    "main",
                    Font::Bytes(TX02_FONT_BYTES),
                    FontParameters::new(CODE_SIZE),
                    Color::rgb(0xFF, 0xD8, 0x66), // fn name: gold
                ),
                TextSpan::new(
                    "() ",
                    Font::Bytes(TX02_FONT_BYTES),
                    CONDENSED,
                    Color::rgb(0xA0, 0xA0, 0xA8),
                ),
                TextSpan::new(
                    "-> ",
                    Font::Bytes(TX02_FONT_BYTES),
                    CONDENSED,
                    Color::rgb(0x80, 0xA0, 0xC0), // operator: blue
                ),
                TextSpan::new(
                    "i32 ",
                    Font::Bytes(TX02_FONT_BYTES),
                    FontParameters::new(CODE_SIZE),
                    Color::rgb(0x6E, 0xE7, 0xB7), // type: green
                ),
                TextSpan::new(
                    "{ ",
                    Font::Bytes(TX02_FONT_BYTES),
                    CONDENSED,
                    Color::rgb(0xA0, 0xA0, 0xA8),
                ),
                TextSpan::new(
                    "42",
                    Font::Bytes(TX02_FONT_BYTES),
                    FontParameters {
                        size: CODE_SIZE,
                        weight: 0.9,
                        width: 1.0,
                        tracking: 0.0,
                    },
                    Color::rgb(0xF0, 0x90, 0x50), // number: orange, bold
                ),
                TextSpan::new(
                    " }",
                    Font::Bytes(TX02_FONT_BYTES),
                    CONDENSED,
                    Color::rgb(0xA0, 0xA0, 0xA8),
                ),
            ],
            SHOWCASE_STYLED_TEXT_POS,
            Alignment::TOP_LEFT,
        );

        // `get_styled_text_size` showcase: same idea as the single-color
        // boxed label above, but sizing the box from a multi-span,
        // multi-color, multi-weight/width call — proving the spans
        // counterpart to `get_text_size` measures the combined bounding
        // box correctly too.
        let styled_boxed_spans = [
            TextSpan::new(
                "3  2 ",
                Font::Bytes(TX02_FONT_BYTES),
                FontParameters {
                    size: 20.0,
                    weight: 1.1,
                    width: 1.0,
                    tracking: 0.0,
                },
                Color::rgb(0xF0, 0x90, 0x50),
            ),
            TextSpan::new(
                "styled ",
                Font::Bytes(TX02_FONT_BYTES),
                FontParameters {
                    size: 20.0,
                    weight: 0.0,
                    width: 0.8,
                    tracking: 0.3,
                },
                Color::rgb(0x80, 0xA0, 0xF0),
            ),
            TextSpan::new(
                "spans",
                Font::Bytes(TX02_FONT_BYTES),
                FontParameters::new(20.0),
                Color::rgb(0x6E, 0xE7, 0xB7),
            ),
        ];
        let (styled_w, styled_h) = bottom_layer.get_styled_text_size(&styled_boxed_spans);
        const STYLED_BOX_PADDING: f32 = 12.0;
        bottom_layer.draw_rectangle(
            (
                SHOWCASE_STYLED_TEXT_BOXED_ANCHOR.0 - styled_w / 2.0 - STYLED_BOX_PADDING,
                SHOWCASE_STYLED_TEXT_BOXED_ANCHOR.1 - styled_h / 2.0 - STYLED_BOX_PADDING,
            ),
            (
                styled_w + STYLED_BOX_PADDING * 2.0,
                styled_h + STYLED_BOX_PADDING * 2.0,
            ),
            Color::rgba(0x20, 0x20, 0x28, 0xD0),
            Rounding::uniform(8.0),
        );
        bottom_layer.draw_styled_text(
            &styled_boxed_spans,
            SHOWCASE_STYLED_TEXT_BOXED_ANCHOR,
            Alignment::CENTER,
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
                if let Some(r) = &mut self.renderer {
                    if let Err(e) = r.render() {
                        eprintln!("render error: {e}");
                    }
                    if let Some(w) = &self.window {
                        // `get_gpu_frametime` is `None` on adapters without
                        // timestamp queries — just omit it from the title.
                        let gpu = r
                            .get_gpu_frametime()
                            .map(|d| format!(", gpu {:.2}ms", d.as_secs_f64() * 1000.0))
                            .unwrap_or_default();
                        w.set_title(&format!(
                            "Atomos — Phase 0 — frame {:.2}ms{gpu}, total render {:.2}s",
                            r.get_frametime().as_secs_f64() * 1000.0,
                            r.get_render_time().as_secs_f64(),
                        ));
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
