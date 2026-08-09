//! Atomos — Phase 0 entry point.
//!
//! Owns the winit event loop and delegates all rendering to
//! [`renderer::Renderer`]. Window event types the renderer needs to see are
//! forwarded through `Renderer::handle_event`; window resize, close request,
//! scale-factor change, and redraw are handled here so the renderer stays
//! platform-agnostic.
//!
//! Frames are demand-driven, not continuous: [`frame::FrameScheduler`] keeps
//! the loop asleep until input changes state or something asks for another
//! frame. See its module doc for the wiring contract.

mod animation;
mod frame;
mod input;
mod renderer;

use std::sync::Arc;

use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, DeviceId, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};

/// Application state. The window and renderer are `None` until `resumed` runs.
struct App {
    window: Option<Arc<Window>>,
    renderer: Option<renderer::Renderer>,
    input: input::Input,
    scheduler: frame::FrameScheduler,
}

impl Default for App {
    fn default() -> Self {
        Self {
            window: None,
            renderer: None,
            // Replaced with the window's real scale factor in `resumed`.
            input: input::Input::new(1.0),
            scheduler: frame::FrameScheduler::new(),
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
        let renderer = pollster::block_on(renderer::Renderer::new(window.clone()));

        self.input = input::Input::new(window.scale_factor());
        self.window = Some(window);
        self.renderer = Some(renderer);
        // The first frame is already owed — `FrameScheduler::new` starts
        // pending precisely because a `Wait` loop that is never asked would
        // leave the window blank forever.
    }

    /// Raw (unaccelerated, unclipped) pointer motion, which only arrives as
    /// a device event — see `Input::raw_mouse_delta`.
    fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
        self.input.handle_device_event(&event);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        // Input state is folded in before anything reads it this frame; it
        // is cleared of per-frame edges/deltas in `RedrawRequested` below.
        // Only events that genuinely changed input state wake the renderer.
        if self.input.handle_event(&event) {
            self.scheduler.request_redraw();
        }

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
                // Neither resize nor rescale flows through `Input`, so the
                // redraw they obviously need has to be asked for here.
                self.scheduler.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(r) = &mut self.renderer {
                    r.set_scale_factor(scale_factor);
                }
                self.scheduler.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                // At the *top* of the frame: consumes the pending request so
                // that anything this frame asks for survives to the next
                // `about_to_wait` instead of being cleared out from under it.
                self.scheduler.begin_frame();

                if let Some(r) = &mut self.renderer {
                    if let Err(e) = r.render() {
                        eprintln!("render error: {e}");
                    }
                    if let Some(w) = &self.window {
                        w.set_title(&format!(
                            "Atomos — Phase 0 — frame {:.2}ms",
                            r.get_frametime().as_secs_f64() * 1000.0
                        ));
                    }
                }

                // Exactly once per frame, after everything has read state.
                self.input.end_frame();
            }
            _ => {}
        }
    }

    /// The scheduling seam: decide whether to draw another frame and how
    /// long to sleep before considering it again.
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
    // Control flow is owned by `FrameScheduler` from `about_to_wait` onward;
    // winit's own default (`Wait`) is the right starting point.
    let mut app = App::default();
    event_loop.run_app(&mut app)
}
