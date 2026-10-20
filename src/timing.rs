//! Timing primitives for demand-driven loops.
//!
//! [`crate::animation::Animation`] answers "is this still running", which is
//! the right question for a transition where every frame shows different
//! pixels. Two common cases are not that, and each needs its own clock:
//!
//! - [`Stepped`]: a value with a handful of visible states (a caret blink).
//!   It reports a *step change* instead of "running", and says when the next
//!   step is due so the loop can sleep until then
//!   ([`crate::frame::FrameScheduler::request_redraw_in`]).
//! - [`Glide`]: a number that eases toward a target that may move while it
//!   travels (a scroll offset), re-aiming from wherever it is now.
//!
//! Both keep their own wall clock instead of taking the frame delta. Under a
//! sleeping scheduler the first frame after an idle window carries the whole
//! idle gap, and [`crate::frame::MAX_ANIMATION_DELTA`] clamps it to 100 ms,
//! which is still most of a short travel: a target aimed *during* that frame
//! would be charged time in which it did not yet exist. Each method has an
//! `*_at(now)` form taking the instant explicitly, which is what the plain
//! form calls with `Instant::now()`; tests and replay use it directly.

use std::time::{Duration, Instant};

use crate::animation::{Animation, Easing};

/// An [`Animation`] sampled at `steps` evenly spaced points.
///
/// Steps are cut on raw progress, not on the eased weight: progress is
/// linear in time, which makes [`Stepped::wake_in`] a subtraction rather
/// than an easing inversion. [`Stepped::weight`] still reports the eased
/// value, so the animation keeps its shape and only takes fewer distinct
/// values.
///
/// A stepped animation that is advanced but never scheduled freezes, because
/// nothing else wakes a sleeping loop; that is why `advance` and `wake_in`
/// live on the same type.
#[derive(Debug)]
pub struct Stepped {
    animation: Animation,
    /// One full sweep of `progress`, 0 to 1.
    sweep: Duration,
    steps: u32,
    step: u32,
    /// Wall clock of the last advance.
    last: Instant,
}

impl Stepped {
    /// `sweep` is how long progress takes to travel 0 to 1; `steps` is how
    /// many distinct values the result may take. Two steps is a blink.
    pub fn new(sweep: Duration, easing: Easing, steps: u32) -> Self {
        Self::new_at(Instant::now(), sweep, easing, steps)
    }

    pub fn new_at(now: Instant, sweep: Duration, easing: Easing, steps: u32) -> Self {
        let animation = Animation::new(sweep, easing).looping();
        let steps = steps.max(1);
        let step = Self::step_of(&animation, steps);
        Self {
            animation,
            sweep,
            steps,
            step,
            last: now,
        }
    }

    fn step_of(animation: &Animation, steps: u32) -> u32 {
        ((animation.progress() * steps as f32) as u32).min(steps - 1)
    }

    /// Step the clock; returns whether the **visible state changed**, which
    /// is what to feed the frame scheduler in place of
    /// [`Animation::advance`]'s "still running".
    pub fn advance(&mut self) -> bool {
        self.advance_at(Instant::now())
    }

    pub fn advance_at(&mut self, now: Instant) -> bool {
        self.animation
            .advance(now.saturating_duration_since(self.last));
        self.last = now;
        let step = Self::step_of(&self.animation, self.steps);
        std::mem::replace(&mut self.step, step) != step
    }

    /// The eased weight, quantised to the current step.
    pub fn weight(&self) -> f32 {
        self.animation
            .easing()
            .apply(self.step as f32 / self.steps as f32)
    }

    /// True through the first half of the cycle: a blink's on/off, for a
    /// two-step animation.
    pub fn is_on(&self) -> bool {
        self.weight() < 0.5
    }

    /// Restart from the top of the cycle. For a caret this is "the user
    /// just typed": the blink must resume lit, not mid-phase.
    pub fn restart(&mut self) {
        self.restart_at(Instant::now());
    }

    pub fn restart_at(&mut self, now: Instant) {
        self.animation.restart();
        self.last = now;
        self.step = Self::step_of(&self.animation, self.steps);
    }

    /// How long until the next step is due: the deadline the event loop
    /// sleeps to. Never zero (a deadline in the past is a spin). Rounds
    /// toward early, since waking a step early costs one redundant frame
    /// while waking late shows a stalled caret.
    pub fn wake_in(&self) -> Duration {
        let per_step = self.sweep / self.steps;
        let into = self.animation.progress() * self.steps as f32 - self.step as f32;
        let remaining = per_step.mul_f32((1.0 - into).clamp(0.0, 1.0)).min(per_step);
        remaining.max(Duration::from_millis(1))
    }
}

/// A number that eases toward a target which may move while it travels.
///
/// - **Re-aiming picks up from wherever the value is now**, so a second
///   wheel notch during the first one's travel reads as one continuous push.
/// - **Aiming where it is already going does nothing**, so a target
///   recomputed and re-aimed every frame does not hold the glide at its
///   start forever.
///
/// [`Glide::advance`] reports the frame it *lands* on as a change, not just
/// the frames before it: a caller that stopped asking for frames a moment
/// early would leave the last pixel of travel undrawn.
#[derive(Debug)]
pub struct Glide {
    animation: Animation,
    from: f32,
    to: f32,
    duration: Duration,
    easing: Easing,
    /// Wall clock of the last aim or advance.
    last: Instant,
}

impl Glide {
    /// A glide resting at zero. `duration` is one full travel, whatever the
    /// distance: a scroll takes the same time to settle moving a line or a
    /// screen.
    pub fn new(duration: Duration, easing: Easing) -> Self {
        Self::new_at(Instant::now(), duration, easing)
    }

    pub fn new_at(now: Instant, duration: Duration, easing: Easing) -> Self {
        Self {
            animation: Animation::new(duration, easing).paused(),
            from: 0.0,
            to: 0.0,
            duration,
            easing,
            last: now,
        }
    }

    /// Travel to `to`, starting from wherever the value is now.
    pub fn aim(&mut self, to: f32) {
        self.aim_at(Instant::now(), to);
    }

    pub fn aim_at(&mut self, now: Instant, to: f32) {
        if to == self.to {
            return;
        }
        // Bring the travel up to the present first: `from` must be where
        // the value is *now*, so a target arriving mid-flight keeps the
        // travel that came just before it.
        self.animation
            .advance(now.saturating_duration_since(self.last));
        self.from = self.value();
        self.to = to;
        self.animation = Animation::new(self.duration, self.easing);
        self.last = now;
    }

    /// Arrive at `to` this instant, with no travel: a different document
    /// under the same offset, or a thumb being dragged that must stay under
    /// the pointer.
    pub fn settle(&mut self, to: f32) {
        self.from = to;
        self.to = to;
        self.animation = Animation::new(self.duration, self.easing).paused();
    }

    /// Step the clock; returns whether anything visible changed.
    pub fn advance(&mut self) -> bool {
        self.advance_at(Instant::now())
    }

    pub fn advance_at(&mut self, now: Instant) -> bool {
        let before = self.value();
        let running = self
            .animation
            .advance(now.saturating_duration_since(self.last));
        self.last = now;
        running || before != self.value()
    }

    pub fn value(&self) -> f32 {
        self.animation.value(self.from, self.to)
    }

    /// Where it is heading: what to read when deciding where to go next.
    /// Accumulating onto [`Glide::value`] instead would make each notch of
    /// a fast scroll shorter than the one before it.
    pub fn target(&self) -> f32 {
        self.to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_blink_only_reports_a_change_when_it_flips() {
        let t0 = Instant::now();
        let mut blink = Stepped::new_at(t0, ms(100), Easing::Linear, 2);
        assert!(blink.is_on(), "starts lit");

        assert!(!blink.advance_at(t0 + ms(20)), "still on");
        assert!(blink.is_on());

        assert!(blink.advance_at(t0 + ms(60)), "flipped off");
        assert!(!blink.is_on());

        assert!(!blink.advance_at(t0 + ms(70)), "still off");
    }

    #[test]
    fn a_restart_resumes_lit() {
        let t0 = Instant::now();
        let mut blink = Stepped::new_at(t0, ms(100), Easing::Linear, 2);
        blink.advance_at(t0 + ms(60));
        assert!(!blink.is_on());

        blink.restart_at(t0 + ms(60));
        assert!(blink.is_on(), "typing must not leave the caret dark");
    }

    #[test]
    fn wake_in_points_at_the_next_step_and_is_never_zero() {
        let t0 = Instant::now();
        let mut blink = Stepped::new_at(t0, ms(100), Easing::Linear, 2);
        // Start of a 50 ms step.
        assert!(blink.wake_in() <= ms(50) && blink.wake_in() >= ms(45));
        blink.advance_at(t0 + ms(40));
        assert!(blink.wake_in() <= ms(11), "{:?}", blink.wake_in());
        assert!(blink.wake_in() >= ms(1));
    }

    #[test]
    fn a_long_idle_gap_wraps_instead_of_finishing() {
        let t0 = Instant::now();
        let mut blink = Stepped::new_at(t0, ms(100), Easing::Linear, 2);
        // 10.01 sweeps later: honest delta, repeating mode wraps it.
        blink.advance_at(t0 + ms(1_001));
        assert!(blink.is_on());
    }

    #[test]
    fn a_glide_reaims_from_where_it_is() {
        let t0 = Instant::now();
        let mut glide = Glide::new_at(t0, ms(100), Easing::Linear);
        glide.aim_at(t0, 100.0);
        glide.advance_at(t0 + ms(50));
        assert!((glide.value() - 50.0).abs() < 1.0);

        glide.aim_at(t0 + ms(50), 200.0);
        assert!(
            (glide.value() - 50.0).abs() < 1.0,
            "no jump back to the old resting place"
        );
        assert_eq!(glide.target(), 200.0);
    }

    #[test]
    fn aiming_where_it_is_already_going_changes_nothing() {
        let t0 = Instant::now();
        let mut glide = Glide::new_at(t0, ms(100), Easing::Linear);
        glide.aim_at(t0, 100.0);
        glide.advance_at(t0 + ms(30));
        let value = glide.value();
        glide.aim_at(t0 + ms(30), 100.0);
        glide.advance_at(t0 + ms(31));
        assert!(glide.value() > value, "the travel kept going");
    }

    #[test]
    fn the_landing_frame_reports_a_change() {
        let t0 = Instant::now();
        let mut glide = Glide::new_at(t0, ms(100), Easing::Linear);
        glide.aim_at(t0, 10.0);
        assert!(glide.advance_at(t0 + ms(60)));
        assert!(glide.advance_at(t0 + ms(120)), "the frame that lands");
        assert_eq!(glide.value(), 10.0);
        assert!(!glide.advance_at(t0 + ms(130)), "then it rests");
    }

    #[test]
    fn a_target_aimed_after_idle_is_not_charged_the_idle_time() {
        let t0 = Instant::now();
        let mut glide = Glide::new_at(t0, ms(100), Easing::Linear);
        // A minute of nothing, then one notch.
        let later = t0 + Duration::from_secs(60);
        glide.aim_at(later, 100.0);
        glide.advance_at(later + ms(10));
        assert!(glide.value() < 20.0, "{}", glide.value());
    }

    #[test]
    fn settle_arrives_immediately() {
        let t0 = Instant::now();
        let mut glide = Glide::new_at(t0, ms(100), Easing::Linear);
        glide.aim_at(t0, 100.0);
        glide.settle(7.0);
        assert_eq!(glide.value(), 7.0);
        assert!(!glide.advance_at(t0 + ms(500)));
    }
}
