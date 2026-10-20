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
//!
//! That same ordering means a frame that showed nothing has already spent
//! its request. Report what `Renderer::render` returned with
//! [`FrameScheduler::finish_frame`] and forward window events to
//! [`FrameScheduler::handle_event`]: a [`FrameOutcome::Retry`] is asked
//! for again, with backoff so a persistent failure cannot spin, and a
//! [`FrameOutcome::Occluded`] frame stays owed but is not polled for until
//! the window is visible again.

use std::time::{Duration, Instant};

use winit::{event::WindowEvent, event_loop::ControlFlow};

use crate::renderer::FrameOutcome;

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

/// Consecutive [`FrameOutcome::Retry`] results retried at full speed before
/// backing off. A resize waiting on the GPU queue clears in a few frames; a
/// failure that lasts longer should not burn a core.
const RETRIES_BEFORE_BACKOFF: u32 = 4;

/// Longest wait between retries once backing off.
const MAX_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// Decides whether the event loop sleeps or draws. See the module doc.
#[derive(Debug)]
pub struct FrameScheduler {
    redraw_requested: bool,
    /// A frame owed at a future instant (a caret blink's next step). Kept
    /// as the earliest of everything asked for; it becomes an ordinary owed
    /// frame once due.
    wake_at: Option<Instant>,
    /// The window is not visible: a frame may be owed, but drawing it would
    /// show nothing. Cleared when the OS reports the window visible again.
    occluded: bool,
    /// Consecutive frames that came back [`FrameOutcome::Retry`].
    retries: u32,
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

impl FrameScheduler {
    pub fn new() -> Self {
        Self {
            // Start pending: under `Wait` a window that never asks for a
            // frame never paints at all, so the first frame has to be owed
            // from the outset.
            redraw_requested: true,
            wake_at: None,
            occluded: false,
            retries: 0,
            target_fps: None,
            last_frame_at: None,
        }
    }

    /// Ask for another frame. Idempotent within a frame — callers can poke
    /// this from anywhere without coordinating.
    pub fn request_redraw(&mut self) {
        self.redraw_requested = true;
    }

    /// Ask for a frame once `delay` has passed, without waking the loop
    /// before then. For state that changes on a schedule rather than in
    /// response to input, such as [`crate::timing::Stepped::wake_in`]: the
    /// loop sleeps until the deadline instead of redrawing identical frames.
    /// The earliest of several requests wins.
    pub fn request_redraw_in(&mut self, delay: Duration) {
        self.request_redraw_at(Instant::now() + delay);
    }

    /// [`request_redraw_in`](Self::request_redraw_in) with an absolute
    /// instant.
    pub fn request_redraw_at(&mut self, at: Instant) {
        self.wake_at = Some(self.wake_at.map_or(at, |existing| existing.min(at)));
    }

    fn wake_due(&self) -> bool {
        self.wake_at.is_some_and(|at| Instant::now() >= at)
    }

    /// Is a frame owed *at all*? This ignores pacing — for the
    /// `about_to_wait` decision use [`should_draw_now`](Self::should_draw_now).
    pub fn wants_redraw(&self) -> bool {
        self.redraw_requested || self.wake_due()
    }

    /// Should `window.request_redraw()` be called right now?
    ///
    /// A frame being owed is not enough: `request_redraw` queues a
    /// `RedrawRequested` event that winit services on the next loop
    /// iteration **whatever `ControlFlow` says**, so asking early makes the
    /// deadline below purely decorative and the target FPS a no-op. The
    /// request has to be withheld until the deadline actually passes.
    pub fn should_draw_now(&self) -> bool {
        self.wants_redraw()
            && !self.occluded
            && self.deadline().is_none_or(|at| Instant::now() >= at)
    }

    /// Earliest instant the next frame may start, or `None` when pacing is
    /// left to the display.
    fn deadline(&self) -> Option<Instant> {
        let paced = match (self.target_fps, self.last_frame_at) {
            (Some(fps), Some(last)) if fps.is_finite() && fps > 0.0 => {
                Some(last + Duration::from_secs_f32(1.0 / fps).max(Duration::from_micros(100)))
            }
            _ => None,
        };
        let backoff = self
            .last_frame_at
            .zip(self.retry_backoff())
            .map(|(last, wait)| last + wait);
        paced.max(backoff)
    }

    /// How long to wait before the next retry, or `None` while retries are
    /// still cheap enough to run at full speed.
    fn retry_backoff(&self) -> Option<Duration> {
        let over = self.retries.checked_sub(RETRIES_BEFORE_BACKOFF + 1)?;
        Some((Duration::from_millis(16) * 2u32.saturating_pow(over)).min(MAX_RETRY_BACKOFF))
    }

    /// Report what `Renderer::render` returned for the frame that just
    /// ran. Call once per frame, after `begin_frame`.
    ///
    /// - [`FrameOutcome::Presented`] resets retry tracking.
    /// - [`FrameOutcome::Retry`] asks for another frame, backing off after
    ///   a few consecutive failures.
    /// - [`FrameOutcome::Occluded`] keeps the frame owed but stops polling
    ///   until [`handle_event`](Self::handle_event) sees the window return.
    pub fn finish_frame(&mut self, outcome: FrameOutcome) {
        match outcome {
            FrameOutcome::Presented => self.retries = 0,
            FrameOutcome::Retry => {
                self.retries = self.retries.saturating_add(1);
                self.redraw_requested = true;
            }
            FrameOutcome::Occluded => {
                self.occluded = true;
                self.redraw_requested = true;
            }
        }
    }

    /// Feed window events through so the scheduler can wake from an
    /// occluded sleep. Returns whether a redraw was requested.
    pub fn handle_event(&mut self, event: &WindowEvent) -> bool {
        if let WindowEvent::Occluded(occluded) = event {
            self.occluded = *occluded;
            if !occluded {
                self.retries = 0;
                self.redraw_requested = true;
                return true;
            }
        }
        false
    }

    /// Call at the **top** of `RedrawRequested`, before per-frame logic:
    /// consumes the pending request and stamps the frame time, so anything
    /// this frame requests counts toward the *next* frame.
    pub fn begin_frame(&mut self) {
        self.redraw_requested = false;
        if self.wake_due() {
            self.wake_at = None;
        }
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
        if self.occluded {
            return ControlFlow::Wait;
        }
        if !self.wants_redraw() {
            return self
                .wake_at
                .map_or(ControlFlow::Wait, ControlFlow::WaitUntil);
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
        assert_eq!(
            scheduler.control_flow(),
            ControlFlow::Wait,
            "idle ignores the target"
        );

        scheduler.request_redraw();
        let ControlFlow::WaitUntil(deadline) = scheduler.control_flow() else {
            panic!(
                "expected a WaitUntil deadline, got {:?}",
                scheduler.control_flow()
            );
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
        assert!(
            !scheduler.should_draw_now(),
            "but not until 200ms have passed"
        );

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

    #[test]
    fn a_retry_is_asked_for_again() {
        let mut scheduler = FrameScheduler::new();
        scheduler.begin_frame();
        scheduler.finish_frame(FrameOutcome::Retry);
        assert!(scheduler.wants_redraw());
    }

    #[test]
    fn persistent_retries_back_off_instead_of_spinning() {
        let mut scheduler = FrameScheduler::new();
        for _ in 0..RETRIES_BEFORE_BACKOFF {
            scheduler.begin_frame();
            scheduler.finish_frame(FrameOutcome::Retry);
            assert_eq!(scheduler.control_flow(), ControlFlow::Poll);
        }
        scheduler.begin_frame();
        scheduler.finish_frame(FrameOutcome::Retry);
        assert!(matches!(
            scheduler.control_flow(),
            ControlFlow::WaitUntil(_)
        ));
        assert!(!scheduler.should_draw_now());

        scheduler.begin_frame();
        scheduler.finish_frame(FrameOutcome::Presented);
        assert!(!scheduler.wants_redraw());
    }

    #[test]
    fn an_occluded_frame_stays_owed_but_is_not_polled() {
        let mut scheduler = FrameScheduler::new();
        scheduler.begin_frame();
        scheduler.finish_frame(FrameOutcome::Occluded);
        assert!(scheduler.wants_redraw(), "the update is still unseen");
        assert!(!scheduler.should_draw_now());
        assert_eq!(scheduler.control_flow(), ControlFlow::Wait);

        assert!(scheduler.handle_event(&WindowEvent::Occluded(false)));
        assert!(scheduler.should_draw_now());
    }

    #[test]
    fn a_nonfinite_target_fps_is_ignored() {
        for fps in [f32::NAN, f32::INFINITY, 0.0, -5.0] {
            let mut scheduler = FrameScheduler::new();
            scheduler.set_target_fps(Some(fps));
            scheduler.begin_frame();
            scheduler.request_redraw();
            assert_eq!(scheduler.control_flow(), ControlFlow::Poll, "fps {fps}");
        }
    }

    #[test]
    fn a_scheduled_wake_sleeps_until_due_then_owes_a_frame() {
        let mut scheduler = FrameScheduler::new();
        scheduler.begin_frame();
        scheduler.request_redraw_in(Duration::from_secs(60));
        assert!(!scheduler.wants_redraw());
        assert!(matches!(
            scheduler.control_flow(),
            ControlFlow::WaitUntil(_)
        ));

        scheduler.request_redraw_at(Instant::now() - Duration::from_millis(1));
        assert!(scheduler.wants_redraw(), "a due wake is an owed frame");
        assert!(scheduler.should_draw_now());

        scheduler.begin_frame();
        assert!(!scheduler.wants_redraw(), "consumed by the frame it woke");
        assert_eq!(scheduler.control_flow(), ControlFlow::Wait);
    }

    #[test]
    fn the_earliest_scheduled_wake_wins() {
        let mut scheduler = FrameScheduler::new();
        scheduler.begin_frame();
        let soon = Instant::now() + Duration::from_secs(1);
        scheduler.request_redraw_at(soon + Duration::from_secs(10));
        scheduler.request_redraw_at(soon);
        scheduler.request_redraw_at(soon + Duration::from_secs(5));
        assert_eq!(scheduler.control_flow(), ControlFlow::WaitUntil(soon));
    }
}
