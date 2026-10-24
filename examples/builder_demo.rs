//! Declarative builder prototype: the whole UI is one `view` function that
//! is re-run on every frame. Unchanged parts cost comparisons only, so the
//! solver and the painter stay idle. `Space` adds a sidebar item at the top,
//! `Backspace` removes the top one (items carry ids, so the rest keep their
//! nodes). Resize to watch the readout change.
//!
//! Run with `cargo run --example builder_demo`. `Esc` quits.

use std::sync::Arc;

use atomos::frame::FrameScheduler;
use atomos::input::Input;
use atomos::layout::{Build, Layout, Overflow, Rect, Style, TextStyle, px};
use atomos::renderer::{Color, Font, FontParameters, Layer, LayerInvalidation, Renderer};
use atomos::ui;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::KeyCode,
    window::{Window, WindowId},
};

const BG: Color = Color::rgb(0x1E, 0x21, 0x27);
const PANEL: Color = Color::rgb(0x28, 0x2C, 0x34);
const TEXT: Color = Color::rgb(0xAB, 0xB2, 0xBF);
const BLUE: Color = Color::rgb(0x61, 0xAF, 0xEF);

struct Theme {
    label: TextStyle,
    small: TextStyle,
    title: TextStyle,
}

struct State {
    items: Vec<(u32, String)>,
    next: u32,
    window: (f32, f32),
}

fn theme() -> Theme {
    let style = |size: f32| {
        TextStyle::new(Font::Named("monospace".into()), FontParameters::new(size), TEXT)
    };
    Theme {
        label: style(13.0).no_wrap(),
        small: style(12.0),
        title: style(16.0),
    }
}

/// The entire UI. Runs every frame; only differences touch the tree.
fn view(b: &mut Build, t: &Theme, s: &State) {
    ui!(b;
        div(Style::new().padding(12.0).gap(12.0).bg(BG)) {
            div(Style::row().height(px(48.0)).items_center().padding_x(14.0).gap(12.0)
                    .bg(PANEL).radius(8.0)) {
                el(Style::new().size(28.0, 28.0).shrink(0.0).bg(BLUE).radius(14.0));
                el(Style::new().flex(1.0).max_width(px(340.0)).height(px(30.0))
                    .bg(BG).radius(6.0));
                with_text(&t.label) {
                    for name in ["Share", "Focus"] => {
                        div(Style::row().height(px(30.0)).padding_x(12.0).items_center().shrink(0.0)
                                .bg(BG).radius(6.0)) {
                            text(Style::new(), name);
                        }
                    }
                }
            }
            div(Style::row().flex(1.0).gap(12.0)) {
                div(Style::new().fixed_width(180.0).padding(12.0).gap(8.0).bg(PANEL).radius(8.0)) {
                    with_text(&t.label) {
                        for (id, name) in &s.items => {
                            text(Style::new().id(*id as u64), name);
                        }
                    }
                }
                div(Style::new().flex(1.0).padding(16.0).gap(10.0).overflow(Overflow::Hidden)
                        .bg(PANEL).radius(8.0)) {
                    text_with(Style::new(), &t.title, "Declarative builder");
                    with_text(&t.small) {
                        text(Style::new(),
                            "This whole view is a single function, re-run every frame.                              Unchanged elements are compared and skipped, so nothing is                              allocated, solved or painted until something really changes.");
                        text(Style::new().margin_top(8.0),
                            "window {:.0} x {:.0}, {} items", s.window.0, s.window.1, s.items.len());
                    }
                }
            }
        }
    );
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    layer: Option<Layer>,
    input: Input,
    scheduler: FrameScheduler,
    layout: Layout,
    theme: Theme,
    state: State,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Atomos — builder demo")
                        .with_inner_size(winit::dpi::LogicalSize::new(900.0, 560.0)),
                )
                .expect("failed to create window"),
        );
        let mut renderer = pollster::block_on(Renderer::new(window.clone()));
        self.layer = Some(renderer.new_layer_top(LayerInvalidation::Manual));
        self.input = Input::new(window.scale_factor());
        self.scheduler.sync_display(&window);
        self.window = Some(window);
        self.renderer = Some(renderer);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if self.input.handle_event(&event) {
            self.scheduler.request_redraw();
        }
        self.scheduler.handle_event(&event);
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
                let s = &mut self.state;
                if self.input.is_key_pressed(KeyCode::Space) {
                    s.next += 1;
                    s.items.insert(0, (s.next, format!("Item {}", s.next)));
                }
                if self.input.is_key_pressed(KeyCode::Backspace) && !s.items.is_empty() {
                    s.items.remove(0);
                }
                if let (Some(w), Some(r), Some(layer)) =
                    (&self.window, &mut self.renderer, &self.layer)
                {
                    let size = w.inner_size().to_logical::<f32>(w.scale_factor());
                    s.window = (size.width, size.height);
                    let (theme, state) = (&self.theme, &self.state);
                    self.layout.build(|b| view(b, theme, state));
                    let view = Rect::new(0.0, 0.0, size.width, size.height);
                    let solved = self.layout.compute(view, layer);
                    if solved | self.layout.take_repaint() {
                        layer.clear();
                        self.layout.paint(layer);
                    }
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
    event_loop.run_app(&mut App {
        window: None,
        renderer: None,
        layer: None,
        input: Input::new(1.0),
        scheduler: FrameScheduler::new(),
        layout: Layout::new(),
        theme: theme(),
        state: State {
            items: ["Inbox", "Drafts", "Archive"]
                .iter()
                .enumerate()
                .map(|(i, n)| (i as u32 + 1, n.to_string()))
                .collect(),
            next: 3,
            window: (0.0, 0.0),
        },
    })
}
