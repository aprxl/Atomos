//! Input-system proof of concept.
//!
//! Run with `cargo run --bin input_demo`. Exercises every part of
//! `atomos::input::Input` against the renderer:
//!
//! - three key squares — a plain key (Space) and two chords (Ctrl+S,
//!   Shift+A) — lit while held, with a press counter proving edge-triggering
//!   fires once per press and not once per frame,
//! - a hover dot that reacts to cursor proximity and to mouse buttons,
//! - a cursor pad whose circle follows the pointer and resizes with the
//!   scroll wheel,
//! - a live readout of raw state (position, deltas, scroll, modifiers,
//!   buttons, held keys, focus, typed text),
//! - two animation bars, one caller-owned and one driven through the keyed
//!   registry, plus a frame counter.
//!
//! Focus-loss safety is visible here: alt-tab away while holding Space and
//! the square unlights immediately rather than staying stuck on.
//!
//! The frame counter is the proof that frames are demand-driven: stop
//! touching the mouse with animations paused (`T`) and it freezes. Nothing
//! is being drawn at all until something asks.

use std::sync::Arc;
use std::time::Duration;

use atomos::animation::{Animation, Animations, Easing};
use atomos::frame::FrameScheduler;
use atomos::input::Input;
use atomos::renderer::{
    Alignment, Color, Font, FontParameters, Layer, LayerInvalidation, Renderer, Rounding,
};
use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, DeviceId, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::{KeyCode, ModifiersState},
    window::{Window, WindowId},
};

const WIDTH: f32 = 1000.0;
const HEIGHT: f32 = 800.0;

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

/// Cursor-pad circle size limits, in logical pixels.
const RADIUS_MIN: f32 = 8.0;
const RADIUS_MAX: f32 = 90.0;
/// Logical pixels of radius per line of scroll.
const RADIUS_PER_SCROLL_LINE: f32 = 4.0;

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

/// One of the three key squares: a chord to watch, and how many times it has
/// fired since launch.
struct KeySquare {
    name: &'static str,
    modifiers: ModifiersState,
    key: KeyCode,
    color: Color,
    presses: u32,
}

impl KeySquare {
    fn held(&self, input: &Input) -> bool {
        input.is_shortcut_down(self.modifiers, self.key)
    }
}

/// Key that toggles both animations. Deliberately not one of the key-square
/// chords, so the two panels stay independently demonstrable.
const TOGGLE_ANIMATION: KeyCode = KeyCode::KeyT;
/// Registry key for the second bar — the whole point of the keyed registry
/// is that this string is the only thing a caller needs to drive it.
const PULSE: &str = "demo.pulse";

struct Demo {
    squares: [KeySquare; 3],
    /// Cursor-pad circle radius, driven by the scroll wheel.
    radius: f32,
    /// Rolling buffer of `Input::text`, to show layout-resolved typing.
    typed: String,
    /// Caller-owned animation — the default pattern: it lives in the struct
    /// of the thing it animates, and nothing else can see it.
    eased: Animation,
    /// The opt-in registry, driving the second bar by name.
    animations: Animations,
    /// Frames actually drawn. Freezes when the scheduler idles, which is the
    /// entire point of the exercise.
    frames: u64,
}

impl Demo {
    fn new() -> Self {
        let mut animations = Animations::new();
        // Linear + hard loop, to contrast against the eased ping-pong below:
        // the snap back to zero is very visible next to a smooth reversal.
        animations.play(
            PULSE,
            Animation::new(Duration::from_millis(900), Easing::Linear).looping(),
        );

        Self {
            eased: Animation::new(Duration::from_millis(1200), Easing::EaseInOut).ping_pong(),
            animations,
            frames: 0,
            squares: [
                KeySquare {
                    name: "Space",
                    modifiers: ModifiersState::empty(),
                    key: KeyCode::Space,
                    color: BLUE,
                    presses: 0,
                },
                KeySquare {
                    name: "Ctrl+S",
                    modifiers: ModifiersState::CONTROL,
                    key: KeyCode::KeyS,
                    color: GREEN,
                    presses: 0,
                },
                KeySquare {
                    name: "Shift+A",
                    modifiers: ModifiersState::SHIFT,
                    key: KeyCode::KeyA,
                    color: PURPLE,
                    presses: 0,
                },
            ],
            radius: 28.0,
            typed: String::new(),
        }
    }

    /// Fold this frame's input into demo state. Split from `draw` so the
    /// draw pass is a pure function of state — the same split real editor
    /// code will want.
    ///
    /// Returns whether anything still wants more frames, which is what the
    /// caller feeds back to [`FrameScheduler::request_redraw`].
    fn update(&mut self, input: &Input, dt: Duration) -> bool {
        self.frames += 1;

        if input.is_key_pressed(TOGGLE_ANIMATION) && input.modifiers().is_empty() {
            let resume = !self.eased.is_playing();
            for target in [Some(&mut self.eased), self.animations.get_mut(PULSE)]
                .into_iter()
                .flatten()
            {
                if resume {
                    target.resume();
                } else {
                    target.pause();
                }
            }
        }

        // `|=`, not `||=`: both must be advanced every frame regardless of
        // which one answers first.
        let mut animating = self.eased.advance(dt);
        animating |= self.animations.advance(dt);

        for square in &mut self.squares {
            if input.is_shortcut_pressed(square.modifiers, square.key) {
                square.presses += 1;
            }
        }

        if input.is_key_pressed(KeyCode::KeyR) && input.modifiers().is_empty() {
            for square in &mut self.squares {
                square.presses = 0;
            }
            self.typed.clear();
            self.radius = 28.0;
        }

        self.radius = (self.radius + input.scroll_delta().1 * RADIUS_PER_SCROLL_LINE)
            .clamp(RADIUS_MIN, RADIUS_MAX);

        self.typed.push_str(input.text());
        // Keep only the tail that fits the readout line.
        while self.typed.chars().count() > 24 {
            self.typed.remove(0);
        }

        animating
    }

    fn draw(&self, layer: &Layer, input: &Input, animating: bool) {
        layer.draw_rectangle((0.0, 0.0), (WIDTH, HEIGHT), BG, Rounding::NONE);

        label(layer, "Atomos — input system", (24.0, 22.0), 20.0, TEXT);
        label(
            layer,
            "R resets  ·  T toggles animation  ·  Esc quits",
            (24.0, 48.0),
            12.0,
            DIM,
        );

        self.draw_keys(layer, input);
        self.draw_hover(layer, input);
        self.draw_cursor_pad(layer, input);
        self.draw_readout(layer, input, animating);
        self.draw_animation(layer);
    }

    fn draw_animation(&self, layer: &Layer) {
        panel(
            layer,
            (24.0, 676.0),
            (952.0, 100.0),
            "ANIMATION — T toggles  ·  weights drive everything below",
        );
        self.draw_bar(layer, 714.0, "ping-pong", self.eased.weight(), BLUE);
        self.draw_bar(layer, 742.0, "loop", self.animations.weight(PULSE), GREEN);
    }

    /// One weight rendered as a track plus a fill. The whole animation
    /// system's output is this single 0..1 number.
    fn draw_bar(&self, layer: &Layer, y: f32, name: &str, weight: f32, color: Color) {
        const TRACK_X: f32 = 150.0;
        const TRACK_W: f32 = 740.0;
        const BAR_H: f32 = 14.0;

        label(layer, name, (48.0, y + 1.0), 12.0, DIM);
        layer.draw_rectangle(
            (TRACK_X, y),
            (TRACK_W, BAR_H),
            SLOT,
            Rounding::uniform(BAR_H / 2.0),
        );

        let filled = TRACK_W * weight.clamp(0.0, 1.0);
        // Below a pixel there is nothing to tessellate, and a rounding radius
        // larger than half the width would fold the shape inside out.
        if filled >= 1.0 {
            layer.draw_rectangle(
                (TRACK_X, y),
                (filled, BAR_H),
                color,
                Rounding::uniform((filled / 2.0).min(BAR_H / 2.0)),
            );
        }

        label(layer, &format!("{weight:.2}"), (906.0, y + 1.0), 12.0, TEXT);
    }

    fn draw_keys(&self, layer: &Layer, input: &Input) {
        panel(
            layer,
            (24.0, 76.0),
            (560.0, 250.0),
            "KEYBOARD — held state (level) + press count (edge)",
        );

        for (i, square) in self.squares.iter().enumerate() {
            let x = 48.0 + i as f32 * 152.0;
            let held = square.held(input);
            let fill = if held { square.color.clone() } else { SLOT };
            layer.draw_rectangle((x, 124.0), (112.0, 112.0), fill, Rounding::uniform(10.0));

            let name_color = if held { square.color.clone() } else { TEXT };
            label(layer, square.name, (x, 248.0), 13.0, name_color);
            label(
                layer,
                &format!("{} presses", square.presses),
                (x, 268.0),
                11.0,
                DIM,
            );
        }
    }

    fn draw_hover(&self, layer: &Layer, input: &Input) {
        panel(layer, (600.0, 76.0), (376.0, 250.0), "HOVER + CLICK");

        let center = (788.0, 176.0);
        let radius = 44.0;
        let (mx, my) = input.mouse_position();
        let hovered = input.is_cursor_in_window()
            && (mx - center.0).powi(2) + (my - center.1).powi(2) <= radius * radius;
        let clicked = hovered && input.is_mouse_down(MouseButton::Left);

        let color = match (hovered, clicked) {
            (_, true) => RED,
            (true, _) => YELLOW,
            _ => SLOT,
        };
        layer.draw_circle(center, radius, color);

        let caption = match (hovered, clicked) {
            (_, true) => "pressed",
            (true, _) => "hovered",
            _ => "hover me",
        };
        label(
            layer,
            caption,
            (center.0 - 40.0, center.1 + 68.0),
            13.0,
            if hovered { TEXT } else { DIM },
        );
    }

    fn draw_cursor_pad(&self, layer: &Layer, input: &Input) {
        panel(
            layer,
            (24.0, 346.0),
            (560.0, 310.0),
            "CURSOR PAD — follows the pointer, scroll to resize",
        );

        let (pad_x, pad_y) = (48.0, 386.0);
        let (pad_w, pad_h) = (512.0, 212.0);
        layer.draw_rectangle((pad_x, pad_y), (pad_w, pad_h), SLOT, Rounding::uniform(6.0));

        // Clamp to the pad so the circle stays fully inside its own area,
        // and keep drawing at the last known position when the cursor is
        // outside the window (Input keeps the position; only hover-ness is
        // false).
        let (mx, my) = input.mouse_position();
        let r = self.radius;
        let cx = mx.clamp(pad_x + r, pad_x + pad_w - r);
        let cy = my.clamp(pad_y + r, pad_y + pad_h - r);
        let inside = mx >= pad_x && mx <= pad_x + pad_w && my >= pad_y && my <= pad_y + pad_h;
        layer.draw_circle((cx, cy), r, if inside { BLUE } else { DIM });

        label(
            layer,
            &format!("radius {:.0}px", self.radius),
            (pad_x, pad_y + pad_h + 14.0),
            12.0,
            DIM,
        );
    }

    fn draw_readout(&self, layer: &Layer, input: &Input, animating: bool) {
        panel(layer, (600.0, 346.0), (376.0, 310.0), "LIVE STATE");

        let x = 624.0;
        let mut y = 384.0;
        let mut line = |key: &str, value: &str, color: Color| {
            label(layer, key, (x, y), 12.0, DIM);
            label(layer, value, (x + 78.0, y), 12.0, color);
            y += 24.0;
        };

        let (mx, my) = input.mouse_position();
        let (dx, dy) = input.mouse_delta();
        let (rdx, rdy) = input.raw_mouse_delta();
        let (sx, sy) = input.scroll_delta();

        line("mouse", &format!("{mx:.0}, {my:.0}"), TEXT);
        line("delta", &format!("{dx:+.1}, {dy:+.1}"), TEXT);
        line("raw", &format!("{rdx:+.1}, {rdy:+.1}"), TEXT);
        line("scroll", &format!("{sx:+.2}, {sy:+.2}"), TEXT);

        let mods = [
            ("CTRL", input.ctrl()),
            ("SHIFT", input.shift()),
            ("ALT", input.alt()),
            ("SUPER", input.super_key()),
        ];
        let active_mods: Vec<&str> = mods.iter().filter(|(_, on)| *on).map(|(n, _)| *n).collect();
        line(
            "mods",
            if active_mods.is_empty() {
                "—".into()
            } else {
                active_mods.join(" ")
            }
            .as_str(),
            YELLOW,
        );

        let buttons: Vec<&str> = [
            ("L", MouseButton::Left),
            ("M", MouseButton::Middle),
            ("R", MouseButton::Right),
        ]
        .iter()
        .filter(|(_, b)| input.is_mouse_down(*b))
        .map(|(n, _)| *n)
        .collect();
        line(
            "buttons",
            if buttons.is_empty() {
                "—".into()
            } else {
                buttons.join(" ")
            }
            .as_str(),
            GREEN,
        );

        let held = input.keys_down().count();
        line("keys", &format!("{held} held"), TEXT);
        line(
            "window",
            &format!(
                "{} · cursor {}",
                if input.is_focused() {
                    "focused"
                } else {
                    "blurred"
                },
                if input.is_cursor_in_window() {
                    "in"
                } else {
                    "out"
                },
            ),
            if input.is_focused() { TEXT } else { RED },
        );
        line(
            "typed",
            if self.typed.is_empty() {
                "—"
            } else {
                &self.typed
            },
            PURPLE,
        );
        // The optimization made visible: with animations paused, this stops
        // climbing the moment you stop moving the mouse.
        line(
            "frames",
            &format!(
                "{} · {}",
                self.frames,
                if animating { "animating" } else { "on demand" }
            ),
            if animating { YELLOW } else { GREEN },
        );
    }
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    layer: Option<Layer>,
    input: Input,
    scheduler: FrameScheduler,
    demo: Demo,
}

impl App {
    fn new() -> Self {
        let mut scheduler = FrameScheduler::new();
        // 30 fps while animating: slow enough to see the pacing work, fast
        // enough not to look broken. Input is never throttled by this.
        scheduler.set_target_fps(Some(30.0));

        Self {
            window: None,
            renderer: None,
            layer: None,
            // Replaced with the window's real factor in `resumed`.
            input: Input::new(1.0),
            scheduler,
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
                        .with_title("Atomos — input demo")
                        .with_inner_size(winit::dpi::LogicalSize::new(WIDTH, HEIGHT)),
                )
                .expect("failed to create window"),
        );
        let mut renderer = pollster::block_on(Renderer::new(window.clone()));
        // Immediate-mode: the whole scene is re-described every frame and
        // the layer skips the GPU rebuild when nothing actually changed.
        let layer = renderer.new_layer_top(LayerInvalidation::Automatic);

        self.input = Input::new(window.scale_factor());
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.layer = Some(layer);
    }

    fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
        self.input.handle_device_event(&event);
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

                // Clamped, because the first frame after an idle stretch
                // reports however long the window sat untouched.
                let dt = FrameScheduler::animation_delta(
                    self.renderer
                        .as_ref()
                        .map_or(Duration::ZERO, Renderer::get_frametime),
                );
                let animating = self.demo.update(&self.input, dt);
                if animating {
                    self.scheduler.request_redraw();
                }

                if let (Some(r), Some(layer)) = (&mut self.renderer, &self.layer) {
                    self.demo.draw(layer, &self.input, animating);
                    match r.render() {
                        Ok(outcome) => self.scheduler.finish_frame(outcome),
                        Err(e) => eprintln!("render error: {e}"),
                    }
                }

                if let Some(w) = &self.window {
                    self.input.sync_ime(w);
                }

                // Exactly once per frame, after everything has read state.
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
    // Control flow is owned by `FrameScheduler` from `about_to_wait` onward.
    event_loop.run_app(&mut App::new())
}
