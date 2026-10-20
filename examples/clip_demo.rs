//! Clipping-API proof of concept.
//!
//! Run with `cargo run --bin clip_demo`. Exercises both clip paths on
//! `atomos::renderer::Layer`:
//!
//! - **Scissor** (a pixel-aligned rectangle, the cheap everyday one): a viewport
//!   panel whose scrolling content — text and bars alike — is cut off
//!   hard at the panel edge by the hardware scissor. The scroll runs
//!   forever; nothing outside the rect is ever shaded.
//! - **Mask** (`set_clip` with any shape, via the stencil): a panel of
//!   gradient + text content clipped by a shape cycled with `M` —
//!   rounded rect, circle, star polygon, SVG heart path. The mask
//!   rebuilds only on the keypress; a static shape costs one small pass
//!   total, then one extra texture sample per composited frame.
//! - **Animated mask**: a spotlight circle following the mouse over a
//!   hidden panel, its radius breathing on a ping-pong animation and
//!   resizable with the scroll wheel. The shape is re-set every frame —
//!   the worst case for the mask path, visibly still smooth.
//!
//! `Esc` quits.

use std::sync::Arc;
use std::time::Duration;

use atomos::animation::{Animation, Easing};
use atomos::frame::FrameScheduler;
use atomos::input::Input;
use atomos::renderer::{
    Alignment, ClipShape, Color, Font, FontParameters, GradientDirection, Layer, LayerInvalidation,
    Renderer, Rounding,
};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::KeyCode,
    window::{Window, WindowId},
};

const WIDTH: f32 = 1000.0;
const HEIGHT: f32 = 760.0;

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

/// Scissor panel geometry (logical px). The viewport is the panel interior
/// the scissor rect is set to — content is drawn well past it on purpose.
const SCISSOR_PANEL: ((f32, f32), (f32, f32)) = ((24.0, 76.0), (460.0, 300.0));
const SCISSOR_VIEW: ((f32, f32), (f32, f32)) = ((40.0, 112.0), (428.0, 248.0));

/// Mask panel geometry. The clip shapes are all authored inside this box.
const MASK_PANEL: ((f32, f32), (f32, f32)) = ((516.0, 76.0), (460.0, 300.0));

/// Spotlight panel geometry.
const SPOT_PANEL: ((f32, f32), (f32, f32)) = ((24.0, 400.0), (952.0, 330.0));

/// Spotlight radius limits and scroll sensitivity, logical px.
const SPOT_RADIUS_MIN: f32 = 30.0;
const SPOT_RADIUS_MAX: f32 = 160.0;
const SPOT_RADIUS_PER_LINE: f32 = 6.0;
/// How far the breathing animation swings the radius.
const SPOT_BREATHE: f32 = 10.0;

/// One scrolling row's height in the scissor viewport.
const ROW_H: f32 = 34.0;
const ROW_COUNT: usize = 12;

fn font() -> Font {
    Font::Named("monospace".into())
}

fn label(layer: &Layer, s: &str, position: (f32, f32), size: f32, color: Color) {
    layer.draw_text(
        s,
        position,
        color,
        Alignment::TOP_LEFT,
        font(),
        FontParameters::new(size),
    );
}

fn panel(layer: &Layer, top_left: (f32, f32), size: (f32, f32), title: &str) {
    layer.draw_rectangle(top_left, size, PANEL, Rounding::uniform(8.0));
    label(
        layer,
        title,
        (top_left.0 + 16.0, top_left.1 + 14.0),
        12.0,
        DIM,
    );
}

/// The shapes `M` cycles through on the mask panel. Everything is authored
/// in the panel's own coordinates so the switch is a pure `ClipShape` swap.
#[derive(Clone, Copy, PartialEq)]
enum MaskKind {
    RoundedRect,
    Circle,
    Star,
    Heart,
}

impl MaskKind {
    const ALL: [MaskKind; 4] = [
        MaskKind::RoundedRect,
        MaskKind::Circle,
        MaskKind::Star,
        MaskKind::Heart,
    ];

    fn name(self) -> &'static str {
        match self {
            MaskKind::RoundedRect => "rounded rect",
            MaskKind::Circle => "circle",
            MaskKind::Star => "star polygon",
            MaskKind::Heart => "SVG heart path",
        }
    }

    fn shape(self) -> ClipShape {
        let ((px, py), (pw, ph)) = MASK_PANEL;
        let center = (px + pw / 2.0, py + ph / 2.0 + 14.0);
        match self {
            MaskKind::RoundedRect => ClipShape::Rectangle {
                top_left: (px + 24.0, py + 40.0),
                size: (pw - 48.0, ph - 64.0),
                rounding: Rounding::uniform(36.0),
            },
            MaskKind::Circle => ClipShape::Circle {
                center,
                radius: 118.0,
            },
            MaskKind::Star => {
                // Five-pointed star: alternate outer/inner radius vertices.
                let (outer, inner) = (128.0, 52.0);
                let vertices = (0..10)
                    .map(|i| {
                        let r = if i % 2 == 0 { outer } else { inner };
                        let a =
                            std::f32::consts::TAU * i as f32 / 10.0 - std::f32::consts::FRAC_PI_2;
                        (center.0 + r * a.cos(), center.1 + r * a.sin())
                    })
                    .collect();
                ClipShape::Polygon { vertices }
            }
            MaskKind::Heart => ClipShape::Path {
                // Authored in a ~256x232 box; positioned to sit centered in
                // the panel.
                d: "M128 226 C 20 150 0 96 0 64 C 0 24 30 0 64 0 C 92 0 116 18 128 44 \
                    C 140 18 164 0 196 0 C 230 0 256 24 256 64 C 256 96 236 150 128 226 Z"
                    .into(),
                position: (center.0 - 128.0, center.1 - 122.0),
            },
        }
    }
}

struct Demo {
    /// Endless scroll driving the scissor viewport, 0..1 per cycle.
    scroll: Animation,
    /// Spotlight radius breathing.
    breathe: Animation,
    /// Current mask-panel shape.
    mask: MaskKind,
    /// Spotlight base radius, scroll-wheel adjustable.
    spot_radius: f32,
}

impl Demo {
    fn new() -> Self {
        Self {
            scroll: Animation::new(Duration::from_secs(6), Easing::Linear).looping(),
            breathe: Animation::new(Duration::from_millis(1400), Easing::EaseInOut).ping_pong(),
            mask: MaskKind::RoundedRect,
            spot_radius: 90.0,
        }
    }

    /// Fold input + time into state. Returns whether anything still wants
    /// more frames (here: always, both animations loop forever).
    fn update(&mut self, input: &Input, dt: Duration) -> bool {
        let mut animating = self.scroll.advance(dt);
        animating |= self.breathe.advance(dt);

        if input.is_key_pressed(KeyCode::KeyM) {
            let i = MaskKind::ALL
                .iter()
                .position(|k| *k == self.mask)
                .unwrap_or(0);
            self.mask = MaskKind::ALL[(i + 1) % MaskKind::ALL.len()];
        }

        self.spot_radius = (self.spot_radius + input.scroll_delta().1 * SPOT_RADIUS_PER_LINE)
            .clamp(SPOT_RADIUS_MIN, SPOT_RADIUS_MAX);

        animating
    }

    /// Chrome + captions — everything that must NOT be clipped lives here.
    fn draw_background(&self, layer: &Layer) {
        layer.draw_rectangle((0.0, 0.0), (WIDTH, HEIGHT), BG, Rounding::NONE);
        label(layer, "Atomos — clipping", (24.0, 22.0), 20.0, TEXT);
        label(
            layer,
            "M cycles the mask shape  ·  scroll resizes the spotlight  ·  Esc quits",
            (24.0, 48.0),
            12.0,
            DIM,
        );

        panel(
            layer,
            SCISSOR_PANEL.0,
            SCISSOR_PANEL.1,
            "SCISSOR — aligned rectangle, hardware, free",
        );
        // The viewport slot the scissored content scrolls inside.
        layer.draw_rectangle(SCISSOR_VIEW.0, SCISSOR_VIEW.1, SLOT, Rounding::NONE);

        panel(
            layer,
            MASK_PANEL.0,
            MASK_PANEL.1,
            "STENCIL — set_clip, any geometry",
        );
        let ((mx, my), (_, mh)) = MASK_PANEL;
        label(
            layer,
            self.mask.name(),
            (mx + 16.0, my + mh - 26.0),
            12.0,
            YELLOW,
        );

        panel(
            layer,
            SPOT_PANEL.0,
            SPOT_PANEL.1,
            "ANIMATED MASK — spotlight follows the mouse, shape re-set every frame",
        );
        let ((sx, sy), (sw, sh)) = SPOT_PANEL;
        layer.draw_text(
            "move the mouse here",
            (sx + sw / 2.0, sy + sh / 2.0),
            DIM,
            Alignment::CENTER,
            font(),
            FontParameters::new(13.0),
        );
    }

    /// Content for the scissor viewport — deliberately drawn far outside
    /// the clip rect so the hard cut is visible on every edge.
    fn draw_scissor_content(&self, layer: &Layer) {
        let ((vx, vy), (_vw, vh)) = SCISSOR_VIEW;
        let cycle = ROW_COUNT as f32 * ROW_H;
        let offset = self.scroll.weight() * cycle;

        // Two copies back to back make the wraparound seamless; rows above
        // and below the viewport are drawn and scissored away.
        let colors = [BLUE, GREEN, PURPLE, YELLOW, RED];
        for pass in 0..2 {
            for i in 0..ROW_COUNT {
                let y = vy + i as f32 * ROW_H - offset + pass as f32 * cycle;
                if y < vy - ROW_H || y > vy + vh {
                    continue;
                }
                let color = colors[i % colors.len()].clone();
                layer.draw_rectangle(
                    (vx + 8.0, y + 8.0),
                    (10.0, ROW_H - 16.0),
                    color.clone(),
                    Rounding::uniform(3.0),
                );
                label(
                    layer,
                    &format!("row {:02} — clipped mid-glyph at the edge", i),
                    (vx + 30.0, y + 9.0),
                    13.0,
                    if i % 2 == 0 { TEXT } else { color },
                );
            }
        }
    }

    /// Content for the mask panel — a gradient wash + grid + text, so the
    /// shape's antialiased edge is visible against varied content.
    fn draw_mask_content(&self, layer: &Layer) {
        let ((px, py), (pw, ph)) = MASK_PANEL;
        layer.draw_rectangle(
            (px, py),
            (pw, ph),
            Color::gradient(
                [0x61, 0xAF, 0xEF, 0xFF],
                [0xC6, 0x78, 0xDD, 0xFF],
                GradientDirection::Horizontal,
            ),
            Rounding::NONE,
        );
        // Checker grid over the gradient.
        let cell: f32 = 20.0;
        let mut row = 0;
        let mut y = py;
        while y < py + ph {
            let mut col = 0;
            let mut x = px;
            while x < px + pw {
                if (row + col) % 2 == 0 {
                    layer.draw_rectangle(
                        (x, y),
                        (cell.min(px + pw - x), cell.min(py + ph - y)),
                        Color::rgba(0xFF, 0xFF, 0xFF, 0x14),
                        Rounding::NONE,
                    );
                }
                x += cell;
                col += 1;
            }
            y += cell;
            row += 1;
        }
        layer.draw_text(
            "any shape, antialiased",
            (px + pw / 2.0, py + ph / 2.0 + 14.0),
            Color::rgb(0xFF, 0xFF, 0xFF),
            Alignment::CENTER,
            font(),
            FontParameters::new(16.0),
        );
    }

    /// Content for the spotlight layer — bright "hidden" content the circle
    /// mask reveals.
    fn draw_spotlight_content(&self, layer: &Layer) {
        let ((sx, sy), (sw, sh)) = SPOT_PANEL;
        layer.draw_rectangle(
            (sx, sy),
            (sw, sh),
            Color::gradient(
                [0x98, 0xC3, 0x79, 0xFF],
                [0xE5, 0xC0, 0x7B, 0xFF],
                GradientDirection::Vertical,
            ),
            Rounding::uniform(8.0),
        );
        let colors = [BG, SLOT, PANEL];
        for i in 0..6 {
            for j in 0..14 {
                let x = sx + 36.0 + j as f32 * 66.0;
                let y = sy + 40.0 + i as f32 * 48.0;
                layer.draw_circle((x, y), 12.0, colors[(i + j) % colors.len()].clone());
            }
        }
        layer.draw_text(
            "you found the hidden layer",
            (sx + sw / 2.0, sy + sh / 2.0),
            BG,
            Alignment::CENTER,
            font(),
            FontParameters::new(18.0),
        );
    }

    /// The spotlight's clip circle for this frame, or `None` when the
    /// cursor is outside the panel (which hides the layer entirely).
    fn spotlight_shape(&self, input: &Input) -> Option<ClipShape> {
        let ((sx, sy), (sw, sh)) = SPOT_PANEL;
        let (mx, my) = input.mouse_position();
        let inside =
            input.is_cursor_in_window() && mx >= sx && mx <= sx + sw && my >= sy && my <= sy + sh;
        inside.then(|| ClipShape::Circle {
            center: (mx, my),
            radius: self.spot_radius + self.breathe.value(-SPOT_BREATHE, SPOT_BREATHE),
        })
    }
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    /// Bottom-to-top: chrome, scissored viewport, masked panel, spotlight.
    background: Option<Layer>,
    scissor_layer: Option<Layer>,
    mask_layer: Option<Layer>,
    spot_layer: Option<Layer>,
    input: Input,
    scheduler: FrameScheduler,
    demo: Demo,
    /// Last mask-panel shape actually applied, so the mask only rebuilds
    /// when `M` changes it — the whole point of the static-mask cost model.
    applied_mask: Option<MaskKind>,
}

impl App {
    fn new() -> Self {
        Self {
            window: None,
            renderer: None,
            background: None,
            scissor_layer: None,
            mask_layer: None,
            spot_layer: None,
            // Replaced with the window's real factor in `resumed`.
            input: Input::new(1.0),
            scheduler: FrameScheduler::new(),
            demo: Demo::new(),
            applied_mask: None,
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Atomos — clip demo")
                        .with_inner_size(winit::dpi::LogicalSize::new(WIDTH, HEIGHT)),
                )
                .expect("failed to create window"),
        );
        let mut renderer = pollster::block_on(Renderer::new(window.clone()));

        let background = renderer.new_layer_top(LayerInvalidation::Automatic);
        let scissor_layer = renderer.new_layer_top(LayerInvalidation::Automatic);
        let mask_layer = renderer.new_layer_top(LayerInvalidation::Automatic);
        let spot_layer = renderer.new_layer_top(LayerInvalidation::Automatic);

        // The scissor rect is panel-fixed: set once, clips forever.
        scissor_layer
            .set_clip(Some(ClipShape::Rectangle {
                top_left: SCISSOR_VIEW.0,
                size: SCISSOR_VIEW.1,
                rounding: Rounding::NONE,
            }))
            .expect("rectangles cannot fail to parse");

        self.input = Input::new(window.scale_factor());
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.background = Some(background);
        self.scissor_layer = Some(scissor_layer);
        self.mask_layer = Some(mask_layer);
        self.spot_layer = Some(spot_layer);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if self.input.handle_event(&event) {
            self.scheduler.request_redraw();
        }
        self.scheduler.handle_event(&event);
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

                let dt = FrameScheduler::animation_delta(
                    self.renderer
                        .as_ref()
                        .map_or(Duration::ZERO, Renderer::get_frametime),
                );
                let animating = self.demo.update(&self.input, dt);
                if animating {
                    self.scheduler.request_redraw();
                }

                if let (Some(r), Some(background), Some(scissor), Some(mask), Some(spot)) = (
                    &mut self.renderer,
                    &self.background,
                    &self.scissor_layer,
                    &self.mask_layer,
                    &self.spot_layer,
                ) {
                    self.demo.draw_background(background);
                    self.demo.draw_scissor_content(scissor);
                    self.demo.draw_mask_content(mask);
                    self.demo.draw_spotlight_content(spot);

                    // Static mask: only rebuilt when `M` actually changed it.
                    if self.applied_mask != Some(self.demo.mask) {
                        mask.set_clip(Some(self.demo.mask.shape()))
                            .expect("mask shapes are compile-time constants");
                        self.applied_mask = Some(self.demo.mask);
                    }
                    // Animated mask: re-set every frame — the stress case.
                    spot.set_clip(self.demo.spotlight_shape(&self.input))
                        .expect("circle shapes cannot fail to parse");

                    match r.render() {
                        Ok(outcome) => self.scheduler.finish_frame(outcome),
                        Err(e) => eprintln!("render error: {e}"),
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
    event_loop.run_app(&mut App::new())
}
