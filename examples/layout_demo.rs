//! Flexbox layout demo: a toolbar, a sidebar and a wrapping paragraph,
//! declared like divs and positioned automatically. Resize the window to
//! watch the layout re-solve; the layer is `Manual`, so frames where
//! nothing moved issue no draw calls at all.
//!
//! Run with `cargo run --example layout_demo`. `Esc` quits.

use std::sync::Arc;

use atomos::frame::FrameScheduler;
use atomos::input::Input;
use atomos::layout::{Layout, NodeId, Overflow, Paint, Rect, Style, Text, px};
use atomos::renderer::{Color, Font, FontParameters, Layer, LayerInvalidation, Renderer};
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

struct Ui {
    layout: Layout,
    status: NodeId,
}

fn label(s: &str, size: f32) -> Text {
    Text::new(
        s,
        Font::Named("monospace".into()),
        FontParameters::new(size),
        TEXT,
    )
}

impl Ui {
    fn new() -> Self {
        let mut ui = Layout::new();
        let root = ui.root();
        ui.set_style(root, |s| *s = Style::new().padding(12.0).gap(12.0));
        ui.set_paint(root, Some(Paint::background(BG)));

        // Toolbar: logo, a search box that grows up to 340px, two buttons.
        let bar = ui.add(
            root,
            Style::row()
                .height(px(48.0))
                .items_center()
                .padding_x(14.0)
                .gap(12.0),
        );
        ui.set_paint(bar, Some(Paint::background(PANEL).radius(8.0)));
        let logo = ui.add(bar, Style::new().size(28.0, 28.0).shrink(0.0));
        ui.set_paint(logo, Some(Paint::background(BLUE).radius(14.0)));
        let search = ui.add(
            bar,
            Style::new().grow(1.0).max_width(px(340.0)).height(px(30.0)),
        );
        ui.set_paint(search, Some(Paint::background(BG).radius(6.0)));
        for name in ["Share", "Focus"] {
            let button = ui.add(
                bar,
                Style::row()
                    .height(px(30.0))
                    .padding_x(12.0)
                    .items_center()
                    .shrink(0.0),
            );
            ui.set_paint(button, Some(Paint::background(BG).radius(6.0)));
            ui.add_text(button, Style::new(), label(name, 13.0).no_wrap());
        }

        // Body: fixed sidebar and a main column whose text wraps.
        let body = ui.add(root, Style::row().grow(1.0).gap(12.0));
        let side = ui.add(body, Style::new().fixed(180.0).padding(12.0).gap(8.0));
        ui.set_paint(side, Some(Paint::background(PANEL).radius(8.0)));
        for item in ["Inbox", "Drafts", "Archive"] {
            ui.add_text(side, Style::new(), label(item, 13.0).no_wrap());
        }
        let main = ui.add(
            body,
            Style::new()
                .grow(1.0)
                .padding(16.0)
                .gap(10.0)
                .overflow(Overflow::Hidden),
        );
        ui.set_paint(main, Some(Paint::background(PANEL).radius(8.0)));
        ui.add_text(main, Style::new(), label("Wrapping text", 16.0));
        ui.add_text(
            main,
            Style::new(),
            label(
                "This paragraph is a flex item. It wraps to whatever width the column \
                 is given, and its height feeds back into the layout, so everything \
                 below it moves with it. Drag the window edge to see.",
                13.0,
            ),
        );
        let status = ui.add_text(main, Style::new().margin_top(8.0), label("", 12.0));
        Self { layout: ui, status }
    }
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    layer: Option<Layer>,
    input: Input,
    scheduler: FrameScheduler,
    ui: Ui,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Atomos — layout demo")
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
                if let (Some(w), Some(r), Some(layer)) =
                    (&self.window, &mut self.renderer, &self.layer)
                {
                    let size = w.inner_size().to_logical::<f32>(w.scale_factor());
                    let ui = &mut self.ui;
                    let text = format!(
                        "{} solves, {} rects moved",
                        ui.layout.solves(),
                        ui.layout.moved_count()
                    );
                    ui.layout.set_text(ui.status, &text);
                    // Only a real change re-solves, and only that redraws.
                    if ui
                        .layout
                        .compute(Rect::new(0.0, 0.0, size.width, size.height), layer)
                    {
                        layer.clear();
                        ui.layout.paint(layer);
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
        ui: Ui::new(),
    })
}
