//! When to draw the next frame.
//!
//! The naive winit loop — `ControlFlow::Poll` plus an unconditional
//! `request_redraw()` — renders forever at display refresh rate, whether or
//! not anything changed. With vsync that isn't a runaway spin, but it is a
//! full CPU frame and GPU composite every vblank for a window showing a
//! static buffer. An editor is idle almost all the time; that is the wrong
//! default.
//!
//! [`FrameScheduler`] inverts it: **draw nothing until something asks.**
//! Input that changed state asks. A running animation asks, every frame, for
//! as long as it runs. Everything else costs zero frames.
//!
//! "Dirty" and "animating" are the same concept here — animating is just
//! asking again every frame. The only difference is pacing, which is why
//! this owns a target-FPS knob rather than a separate animation clock.
//!
//! Wiring, in an `ApplicationHandler`:
//!
//! ```ignore
//! fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
//!     // `should_draw_now`, not `wants_redraw` — see its doc. Requesting
//!     // early makes any FPS target silently do nothing.
//!     if self.scheduler.should_draw_now() {
//!         self.window.request_redraw();
//!     }
//!     event_loop.set_control_flow(self.scheduler.control_flow());
//! }
//!
//! // in WindowEvent::RedrawRequested, *before* per-frame logic:
//! self.scheduler.begin_frame();
//! let dt = FrameScheduler::animation_delta(renderer.get_frametime());
//! if animations.advance(dt) {
//!     self.scheduler.request_redraw();   // still animating -> one more frame
//! }
//! ```
//!
//! `begin_frame` clears the pending request at the *top* of the frame, so
//! anything the frame itself requests survives to the next `about_to_wait`.
//! Clearing at the end would swallow it and the animation would stall after
//! one frame.

use std::time::{Duration, Instant};

use winit::event_loop::ControlFlow;

/// Ceiling on the delta handed to animations, regardless of how long the
/// process actually sat idle.
///
/// Under `ControlFlow::Wait` the gap between two frames is unbounded — a
/// window left alone for a minute produces a 60-second frame delta on the
/// next keystroke. Stepping animations by that jumps every one of them
/// straight to its end state, which reads as "animations randomly don't
/// play". 100 ms is well past a dropped frame at any refresh rate and well
/// short of anything a user would perceive as a jump.
pub const MAX_ANIMATION_DELTA: Duration = Duration::from_millis(100);

/// Decides whether the event loop sleeps or draws. See the module doc.
#[derive(Debug)]
pub struct FrameScheduler {
    redraw_requested: bool,
    /// `None` means "as fast as the display allows" — with `PresentMode::Fifo`
    /// that is already vsync-paced, so no deadline is needed.
    target_fps: Option<f32>,
    last_frame_at: Option<Instant>,
}

impl Default for FrameScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)] // knobs the first animated feature will want; see `input::Input`
impl FrameScheduler {
    pub fn new() -> Self {
        Self {
            // Start pending: under `Wait` a window that never asks for a
            // frame never paints at all, so the first frame has to be owed
            // from the outset.
            redraw_requested: true,
            target_fps: None,
            last_frame_at: None,
        }
    }

    /// Ask for another frame. Idempotent within a frame — callers can poke
    /// this from anywhere without coordinating.
    pub fn request_redraw(&mut self) {
        self.redraw_requested = true;
    }

    /// Is a frame owed *at all*? This ignores pacing — for the
    /// `about_to_wait` decision use [`should_draw_now`](Self::should_draw_now).
    pub fn wants_redraw(&self) -> bool {
        self.redraw_requested
    }

    /// Should `window.request_redraw()` be called right now?
    ///
    /// A frame being owed is not enough: `request_redraw` queues a
    /// `RedrawRequested` event that winit services on the next loop
    /// iteration **whatever `ControlFlow` says**, so asking early makes the
    /// deadline below purely decorative and the target FPS a no-op. The
    /// request has to be withheld until the deadline actually passes.
    pub fn should_draw_now(&self) -> bool {
        self.redraw_requested && self.deadline().is_none_or(|at| Instant::now() >= at)
    }

    /// Earliest instant the next frame may start, or `None` when pacing is
    /// left to the display.
    fn deadline(&self) -> Option<Instant> {
        match (self.target_fps, self.last_frame_at) {
            (Some(fps), Some(last)) if fps > 0.0 => {
                Some(last + Duration::from_secs_f32(1.0 / fps))
            }
            _ => None,
        }
    }

    /// Call at the **top** of `RedrawRequested`, before per-frame logic:
    /// consumes the pending request and stamps the frame time, so anything
    /// this frame requests counts toward the *next* frame.
    pub fn begin_frame(&mut self) {
        self.redraw_requested = false;
        self.last_frame_at = Some(Instant::now());
    }

    /// What the event loop should do next.
    ///
    /// - Nothing owed → [`ControlFlow::Wait`]: sleep until an OS event.
    /// - Owed, no FPS target → [`ControlFlow::Poll`]: vsync does the pacing.
    /// - Owed, with a target → [`ControlFlow::WaitUntil`] the next deadline,
    ///   which is the only way to run *slower* than the display (battery
    ///   throttling, a deliberate 30 fps cap).
    pub fn control_flow(&self) -> ControlFlow {
        if !self.redraw_requested {
            return ControlFlow::Wait;
        }
        match self.deadline() {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Poll,
        }
    }

    /// Cap the frame rate while animating. `None` (the default) follows the
    /// display. A target *above* the refresh rate has no effect — vsync
    /// still gates presentation.
    pub fn set_target_fps(&mut self, fps: Option<f32>) {
        self.target_fps = fps;
    }

    pub fn target_fps(&self) -> Option<f32> {
        self.target_fps
    }

    /// Clamp a measured frame time (e.g. `Renderer::get_frametime`) to
    /// something safe to step animations by — see [`MAX_ANIMATION_DELTA`]
    /// for why this is not optional.
    pub fn animation_delta(frametime: Duration) -> Duration {
        frametime.min(MAX_ANIMATION_DELTA)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_is_owed_so_the_window_paints() {
        // Under `Wait`, a scheduler that started clean would never paint.
        let scheduler = FrameScheduler::new();
        assert!(scheduler.wants_redraw());
        assert_eq!(scheduler.control_flow(), ControlFlow::Poll);
    }

    #[test]
    fn idle_scheduler_sleeps() {
        let mut scheduler = FrameScheduler::new();
        scheduler.begin_frame();
        assert!(!scheduler.wants_redraw());
        assert_eq!(scheduler.control_flow(), ControlFlow::Wait);
    }

    #[test]
    fn a_request_during_a_frame_survives_to_the_next() {
        // The ordering bug this guards: clearing at the end of a frame
        // instead of the start swallows the "still animating" request and
        // the animation stalls after one frame.
        let mut scheduler = FrameScheduler::new();
        scheduler.begin_frame();
        scheduler.request_redraw();
        assert!(scheduler.wants_redraw());
        assert_ne!(scheduler.control_flow(), ControlFlow::Wait);
    }

    #[test]
    fn target_fps_produces_a_deadline_only_when_animating() {
        let mut scheduler = FrameScheduler::new();
        scheduler.set_target_fps(Some(30.0));
        scheduler.begin_frame();
        assert_eq!(scheduler.control_flow(), ControlFlow::Wait, "idle ignores the target");

        scheduler.request_redraw();
        let ControlFlow::WaitUntil(deadline) = scheduler.control_flow() else {
            panic!("expected a WaitUntil deadline, got {:?}", scheduler.control_flow());
        };
        let budget = deadline.duration_since(scheduler.last_frame_at.unwrap());
        assert!(
            (budget.as_secs_f32() - 1.0 / 30.0).abs() < 1e-4,
            "30 fps should budget ~33ms, got {budget:?}"
        );
    }

    #[test]
    fn a_target_actually_withholds_the_redraw_request() {
        // The bug this pins: `about_to_wait` gating on `wants_redraw` alone
        // queues a RedrawRequested that winit services regardless of
        // ControlFlow, so the deadline never throttles anything.
        let mut scheduler = FrameScheduler::new();
        scheduler.set_target_fps(Some(5.0));
        scheduler.begin_frame();
        scheduler.request_redraw();

        assert!(scheduler.wants_redraw(), "a frame is owed");
        assert!(!scheduler.should_draw_now(), "but not until 200ms have passed");

        // No target -> nothing to wait for, vsync does the pacing.
        scheduler.set_target_fps(None);
        assert!(scheduler.should_draw_now());
    }

    #[test]
    fn animation_delta_clamps_an_idle_gap() {
        assert_eq!(
            FrameScheduler::animation_delta(Duration::from_secs(60)),
            MAX_ANIMATION_DELTA,
            "a minute of idle must not teleport animations to their end"
        );
        let normal = Duration::from_millis(16);
        assert_eq!(FrameScheduler::animation_delta(normal), normal);
    }
}
