//! Typewritter's title bar built with `atomos::layout` instead of hand-
//! written offsets. Resize the window (minimum 620 px wide): the search box
//! stays centred and shrinks, the buttons stay glued to their sides, and no
//! coordinate appears anywhere below. Hover a control, click Focus to toggle
//! it. The geometry is checked against Typewritter's original arithmetic in
//! `src/layout/title_bar_port.rs`.
//!
//! Run with `cargo run --example title_bar_demo`. `Esc` quits.

use std::sync::Arc;

use atomos::frame::FrameScheduler;
use atomos::input::Input;
use atomos::layout::{Justify, Layout, NodeId, Paint, Rect, Style, Text, px};
use atomos::renderer::{Color, Font, FontParameters, Layer, LayerInvalidation, Renderer};
use winit::{
    application::ApplicationHandler,
    event::{MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::KeyCode,
    window::{Window, WindowId},
};

const PAGE: Color = Color::rgb(0x16, 0x18, 0x1D);
const CHROME: Color = Color::rgb(0x1E, 0x21, 0x27);
const FIELD: Color = Color::rgb(0x14, 0x16, 0x1A);
const HOVER: Color = Color::rgb(0x2A, 0x2E, 0x36);
const ACTIVE: Color = Color::rgb(0x2C, 0x3A, 0x52);
const RULE: Color = Color::rgb(0x2E, 0x32, 0x3A);
const INK: Color = Color::rgb(0xE6, 0xE8, 0xEC);
const DIM: Color = Color::rgb(0x8A, 0x91, 0x9E);
const BLUE: Color = Color::rgb(0x61, 0xAF, 0xEF);
const GREEN: Color = Color::rgb(0x98, 0xC3, 0x79);

const SIDEBAR: u32 = 1;
const SEARCH: u32 = 2;
const FOCUS: u32 = 3;
const SWITCH: u32 = 4;

fn text(s: &str, size: f32, color: Color) -> Text {
    Text::new(s, Font::Named("sans-serif".into()), FontParameters::new(size), color).no_wrap()
}

struct Bar {
    ui: Layout,
    controls: [(u32, NodeId, Paint); 4],
    readout: NodeId,
    hover: u32,
    focus_mode: bool,
}

impl Bar {
    fn new() -> Self {
        let mut ui = Layout::new();
        let root = ui.root();
        ui.set_style(root, |_| Style::new());
        ui.set_paint(root, Some(Paint::background(PAGE)));

        let bar = ui.add(root, Style::row().height(px(59.0)));
        ui.set_paint(bar, Some(Paint::background(CHROME)));
        let rule = ui.add(root, Style::new().height(px(1.0)));
        ui.set_paint(rule, Some(Paint::background(RULE)));

        // Left half: brand (fixed 188) then the sidebar button.
        let left = ui.add(bar, Style::row().grow(1.0));
        let brand = ui.add(
            left,
            Style::row().fixed(188.0).items_center().padding_left(20.0).gap(11.0),
        );
        let logo = ui.add(brand, Style::new().size(30.0, 30.0).shrink(0.0));
        ui.set_paint(logo, Some(Paint::background(BLUE).radius(15.0)));
        ui.add_text(brand, Style::new(), text("Typewritter", 16.0, INK));
        let sidebar = ui.add(
            left,
            Style::new().size(34.0, 34.0).margin_top(13.0).shrink(0.0),
        );
        ui.set_tag(sidebar, SIDEBAR);
        let glyph = ui.add(sidebar, Style::new().size(18.0, 18.0).margin(8.0));
        ui.set_paint(glyph, Some(Paint::background(DIM).radius(4.0)));

        // Middle: the search box, 340 wide at most, shrinking when it must.
        let search = ui.add(
            bar,
            Style::row()
                .width(px(340.0))
                .min_width(px(0.0))
                .height(px(36.0))
                .margin_top(12.0)
                .margin_x(24.0)
                .padding_x(12.0)
                .gap(10.0)
                .items_center(),
        );
        ui.set_tag(search, SEARCH);
        let lens = ui.add(search, Style::new().size(14.0, 14.0).shrink(0.0));
        ui.set_paint(lens, Some(Paint::background(DIM).radius(7.0)));
        ui.add_text(search, Style::new().shrink(1.0), text("Search notes", 12.5, DIM));

        // Right half: Focus, then the theme switch, pinned to the edge.
        let right = ui.add(bar, Style::row().grow(1.0).justify(Justify::End));
        let focus = ui.add(
            right,
            Style::row()
                .size(80.0, 34.0)
                .margin_top(13.0)
                .margin_right(14.0)
                .padding_left(8.0)
                .gap(8.0)
                .items_center()
                .shrink(0.0),
        );
        ui.set_tag(focus, FOCUS);
        let eye = ui.add(focus, Style::new().size(14.0, 14.0).shrink(0.0));
        ui.set_paint(eye, Some(Paint::background(DIM).radius(3.0)));
        ui.add_text(focus, Style::new(), text("Focus", 12.0, DIM));
        let switch = ui.add(
            right,
            Style::new()
                .size(30.0, 30.0)
                .margin_top(14.0)
                .margin_right(18.0)
                .shrink(0.0),
        );
        ui.set_tag(switch, SWITCH);

        let readout = ui.add_text(
            root,
            Style::new().margin(24.0),
            Text::new(
                "",
                Font::Named("monospace".into()),
                FontParameters::new(12.0),
                DIM,
            ),
        );

        let controls = [
            (SIDEBAR, sidebar, Paint::background(HOVER).radius(8.0)),
            (SEARCH, search, Paint::background(FIELD).radius(9.0).border(1.0, RULE)),
            (FOCUS, focus, Paint::background(HOVER).radius(8.0)),
            (SWITCH, switch, Paint::background(GREEN).radius(15.0)),
        ];
        let mut bar = Self {
            ui,
            controls,
            readout,
            hover: 0,
            focus_mode: false,
        };
        bar.restyle();
        bar
    }

    /// Hover and the Focus toggle are just paints on nodes; the geometry
    /// never changes.
    fn restyle(&mut self) {
        for (tag, node, base) in &self.controls {
            let hot = self.hover == *tag;
            let paint = match *tag {
                SEARCH => Paint {
                    background: Some(if hot { HOVER } else { FIELD }),
                    ..base.clone()
                },
                SWITCH => base.clone(),
                FOCUS if self.focus_mode => Paint::background(ACTIVE).radius(8.0),
                _ => {
                    if hot {
                        base.clone()
                    } else {
                        Paint {
                            background: None,
                            ..base.clone()
                        }
                    }
                }
            };
            self.ui.set_paint(*node, Some(paint));
        }
    }
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    layer: Option<Layer>,
    input: Input,
    scheduler: FrameScheduler,
    bar: Bar,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Atomos — title bar built with layout")
                        .with_inner_size(winit::dpi::LogicalSize::new(1000.0, 360.0))
                        .with_min_inner_size(winit::dpi::LogicalSize::new(620.0, 200.0)),
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
                    let bar = &mut self.bar;
                    let point = self.input.mouse_position();
                    let over = if self.input.is_cursor_in_window() {
                        bar.ui.hit_tag(point).unwrap_or(0)
                    } else {
                        0
                    };
                    let mut changed = over != bar.hover;
                    bar.hover = over;
                    if self.input.is_mouse_pressed(MouseButton::Left) && over == FOCUS {
                        bar.focus_mode = !bar.focus_mode;
                        changed = true;
                    }
                    if changed {
                        bar.restyle();
                    }
                    let view = Rect::new(0.0, 0.0, size.width, size.height);
                    let mut solved = bar.ui.compute(view, layer);
                    if solved {
                        // Rewritten only after a real solve, so the readout
                        // changes (and re-solves once more) only on resize.
                        let search = bar.ui.rect(bar.controls[1].1);
                        let readout = format!(
                            "window {:.0} px   search box x={:.1} w={:.0}",
                            size.width, search.x, search.width
                        );
                        bar.ui.set_text(bar.readout, &readout);
                        bar.ui.compute(view, layer);
                    }
                    solved |= changed;
                    if solved {
                        layer.clear();
                        bar.ui.paint(layer);
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
        bar: Bar::new(),
    })
}
