//! Timers that produce a 0..1 weight over wall-clock time.
//!
//! [`Animation`] is the whole primitive: a duration, an easing curve, and a
//! repeat mode. Everything an editor animates — cursor blink, smooth scroll,
//! a popup fading in, a diagnostic pulsing — is some value interpolated by a
//! weight this produces. Keyframes, springs, and sequences are all things
//! built *on* this later; none of them need to exist for it to be useful.
//!
//! Two ways to own one, for two different situations:
//!
//! 1. **Caller-owned** (the default). Store an [`Animation`] in whatever
//!    struct the animated thing lives in. This is right for almost
//!    everything — a cursor's blink belongs to the cursor.
//!
//!    ```ignore
//!    let mut blink = Animation::new(Duration::from_millis(500), Easing::Linear).ping_pong();
//!    let still_running = blink.advance(dt);   // once per frame
//!    let alpha = blink.weight();              // 0..1
//!    ```
//!
//! 2. **Keyed registry** ([`Animations`], opt-in). For animations that need
//!    to be addressable from somewhere that doesn't own them — a Lua plugin
//!    fading a panel it didn't create, say. One `advance` drives them all.
//!
//!    ```ignore
//!    animations.play("panel.fade", Animation::new(Duration::from_millis(120), Easing::EaseOut));
//!    let any_running = animations.advance(dt);
//!    let alpha = animations.weight("panel.fade");
//!    ```
//!
//! **`advance` returns whether the animation is still running**, which is the
//! signal the frame scheduler needs — see `frame::FrameScheduler`. An
//! animation that returns `false` has reached its final value, and that
//! final value is still what `weight()` reports, so the last frame drawn is
//! the finished state rather than a snap back to zero.
//!
//! **Feed `advance` a clamped delta.** Under a `ControlFlow::Wait` scheduler
//! the first frame after an idle gap has a multi-second frame time; passing
//! that through would teleport every animation to its end. `frame.rs` owns
//! that clamp.

// A foundation module: the entry points exist before the first caller does,
// and deleting the knobs would just mean adding them back for the first real
// animation. Same treatment as `input`'s query surface.
#![allow(dead_code)]

use std::collections::HashMap;
use std::time::Duration;

/// How a weight is shaped as it travels from 0 to 1.
///
/// The named curves are the CSS ones, delegating to their spec-defined
/// [`Easing::CubicBezier`] control points rather than to hand-rolled
/// polynomial lookalikes — one code path, and curves that match what a
/// designer means by "ease-out".
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Easing {
    /// No shaping — weight equals raw progress.
    Linear,
    /// `cubic-bezier(0.42, 0, 1, 1)` — slow start, abrupt stop.
    EaseIn,
    /// `cubic-bezier(0, 0, 0.58, 1)` — abrupt start, slow stop. The usual
    /// choice for UI that appears in response to a user action.
    EaseOut,
    /// `cubic-bezier(0.42, 0, 0.58, 1)` — slow at both ends.
    EaseInOut,
    /// An arbitrary CSS-style timing function: the two control points of a
    /// unit cubic Bézier from `(0,0)` to `(1,1)`, as `(x1, y1, x2, y2)`.
    /// The `x` components are clamped to `0..=1` (a non-monotonic curve has
    /// no single solution for a given time); `y` is unclamped, so
    /// overshooting "back" easings work.
    CubicBezier(f32, f32, f32, f32),
}

impl Easing {
    /// Shape a raw `0..1` progress value into an eased `0..1` weight.
    pub fn apply(self, progress: f32) -> f32 {
        let (x1, y1, x2, y2) = match self {
            Easing::Linear => return progress.clamp(0.0, 1.0),
            Easing::EaseIn => (0.42, 0.0, 1.0, 1.0),
            Easing::EaseOut => (0.0, 0.0, 0.58, 1.0),
            Easing::EaseInOut => (0.42, 0.0, 0.58, 1.0),
            Easing::CubicBezier(x1, y1, x2, y2) => (x1, y1, x2, y2),
        };
        cubic_bezier(progress.clamp(0.0, 1.0), x1.clamp(0.0, 1.0), y1, x2.clamp(0.0, 1.0), y2)
    }
}

/// Newton iterations used to invert the Bézier's x-axis. Eight is what
/// browser engines settle on; the curve is smooth and monotonic in `x`, so
/// it converges to well below display precision long before that.
const NEWTON_ITERATIONS: u8 = 8;
/// Below this slope Newton's method divides by ~nothing and diverges (flat
/// regions of a curve like `cubic-bezier(1, 0, 0, 1)`); fall back to the
/// current guess instead.
const NEWTON_MIN_SLOPE: f32 = 1e-4;

/// One axis of a unit cubic Bézier from `(0,0)` to `(1,1)`, with control
/// values `a1`/`a2` on that axis.
fn bezier_axis(t: f32, a1: f32, a2: f32) -> f32 {
    let inv = 1.0 - t;
    3.0 * inv * inv * t * a1 + 3.0 * inv * t * t * a2 + t * t * t
}

/// Derivative of [`bezier_axis`] with respect to `t`.
fn bezier_axis_slope(t: f32, a1: f32, a2: f32) -> f32 {
    let inv = 1.0 - t;
    3.0 * inv * inv * a1 + 6.0 * inv * t * (a2 - a1) + 3.0 * t * t * (1.0 - a2)
}

/// Evaluate a CSS-style timing function: given `x` (time), solve for the
/// curve parameter `t`, then return `y` (progress) at that `t`.
///
/// The solve is necessary because a Bézier is parametric — `t` is not time.
/// Treating `t` as time directly is the classic mistake, and it silently
/// produces a curve that is close enough to look plausible and wrong enough
/// to feel off.
fn cubic_bezier(x: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    // Identity curve: skip the solve entirely (also the `Linear` shape, and
    // common enough as an explicit `CubicBezier(0, 0, 1, 1)` to be worth it).
    if x1 == y1 && x2 == y2 {
        return x;
    }

    let mut t = x;
    for _ in 0..NEWTON_ITERATIONS {
        let slope = bezier_axis_slope(t, x1, x2);
        if slope.abs() < NEWTON_MIN_SLOPE {
            break;
        }
        t -= (bezier_axis(t, x1, x2) - x) / slope;
    }
    bezier_axis(t.clamp(0.0, 1.0), y1, y2)
}

/// What happens when an [`Animation`] reaches the end of its duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Repeat {
    /// Stop at weight 1.0 and stay there. The only mode that ever finishes.
    #[default]
    Once,
    /// Snap back to 0.0 and run again, forever.
    Loop,
    /// Run 0 → 1, then 1 → 0, forever. The natural shape for a cursor blink
    /// or a pulsing highlight, where a `Loop`'s hard snap would read as a
    /// glitch.
    PingPong,
}

/// A timer producing a 0..1 weight. See the module doc for the two ownership
/// patterns.
#[derive(Clone, Debug, PartialEq)]
pub struct Animation {
    duration: Duration,
    elapsed: Duration,
    easing: Easing,
    repeat: Repeat,
    playing: bool,
}

impl Animation {
    /// A one-shot animation, already playing, that runs from 0 to 1 over
    /// `duration`.
    pub fn new(duration: Duration, easing: Easing) -> Self {
        Self {
            duration,
            elapsed: Duration::ZERO,
            easing,
            repeat: Repeat::Once,
            playing: true,
        }
    }

    /// Restart from 0 on reaching the end, forever.
    pub fn looping(mut self) -> Self {
        self.repeat = Repeat::Loop;
        self
    }

    /// Reverse direction on reaching either end, forever.
    pub fn ping_pong(mut self) -> Self {
        self.repeat = Repeat::PingPong;
        self
    }

    /// Start held at weight 0 instead of running — for an animation created
    /// ahead of the event that triggers it.
    pub fn paused(mut self) -> Self {
        self.playing = false;
        self
    }

    /// Step the clock by `dt` and report **whether this animation still
    /// wants more frames**. That return value is the frame scheduler's
    /// signal; see the module doc.
    ///
    /// A paused animation returns `false` without advancing. A `Repeat::Loop`
    /// or `Repeat::PingPong` animation returns `true` until paused.
    pub fn advance(&mut self, dt: Duration) -> bool {
        if !self.playing {
            return false;
        }
        if self.duration.is_zero() {
            // Nothing to interpolate; a zero-length one-shot is already over.
            self.playing = self.repeat != Repeat::Once;
            return self.playing;
        }

        self.elapsed += dt;
        match self.repeat {
            Repeat::Once => {
                if self.elapsed >= self.duration {
                    self.elapsed = self.duration;
                    self.playing = false;
                }
            }
            // Wrap rather than letting `elapsed` grow without bound: an
            // animation looping for hours would otherwise lose enough f32
            // precision in `progress` to visibly stutter.
            Repeat::Loop => self.elapsed = wrap(self.elapsed, self.duration),
            Repeat::PingPong => self.elapsed = wrap(self.elapsed, 2 * self.duration),
        }
        self.playing
    }

    /// Raw, unshaped position through the animation, `0.0..=1.0`. For
    /// [`Repeat::PingPong`] this already accounts for the reversal, so it
    /// travels 0 → 1 → 0 rather than 0 → 1 → 2.
    pub fn progress(&self) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let cycle = self.elapsed.as_secs_f32() / self.duration.as_secs_f32();
        match self.repeat {
            Repeat::PingPong if cycle > 1.0 => (2.0 - cycle).clamp(0.0, 1.0),
            _ => cycle.clamp(0.0, 1.0),
        }
    }

    /// [`Animation::progress`] shaped by this animation's [`Easing`] —
    /// the value to actually drive things with.
    pub fn weight(&self) -> f32 {
        self.easing.apply(self.progress())
    }

    /// [`Animation::weight`] mapped onto a range: `from` at weight 0, `to`
    /// at weight 1.
    pub fn value(&self, from: f32, to: f32) -> f32 {
        from + (to - from) * self.weight()
    }

    /// Has this animation reached its end and stopped? Only ever true for
    /// [`Repeat::Once`] — the repeating modes run until paused.
    pub fn is_finished(&self) -> bool {
        self.repeat == Repeat::Once && !self.playing && self.elapsed >= self.duration
    }

    /// Is the clock currently running?
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Jump back to the start and play, whatever state this was in.
    pub fn restart(&mut self) {
        self.elapsed = Duration::ZERO;
        self.playing = true;
    }

    /// Freeze at the current weight. `advance` becomes a no-op returning
    /// `false`, so a paused animation stops asking the scheduler for frames.
    pub fn pause(&mut self) {
        self.playing = false;
    }

    /// Resume from wherever [`Animation::pause`] stopped. Resuming a
    /// finished one-shot does nothing until [`Animation::restart`].
    pub fn resume(&mut self) {
        if !self.is_finished() {
            self.playing = true;
        }
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn easing(&self) -> Easing {
        self.easing
    }

    pub fn repeat(&self) -> Repeat {
        self.repeat
    }
}

/// Wrap `elapsed` into `0..period` without the f32 round-tripping that
/// `as_secs_f32() % ..` would introduce.
fn wrap(elapsed: Duration, period: Duration) -> Duration {
    debug_assert!(!period.is_zero(), "wrap period must be non-zero");
    Duration::from_nanos((elapsed.as_nanos() % period.as_nanos()) as u64)
}

/// A string-keyed set of [`Animation`]s, advanced together.
///
/// Opt-in: reach for this only when something needs to drive an animation it
/// doesn't own. Keys are the reason this is safe to leave running —
/// [`Animations::play`] on an existing key *replaces* it, so a caller that
/// re-triggers "panel.fade" a thousand times still holds one animation, and
/// the set stays bounded by the number of distinct things being animated.
///
/// Finished animations are deliberately **not** evicted: `weight()` on a
/// completed fade must keep returning 1.0, or the element it drives snaps
/// back to invisible on the very frame the fade lands. Use
/// [`Animations::stop`] or [`Animations::clear`] to actually remove them.
#[derive(Debug, Default)]
pub struct Animations {
    animations: HashMap<String, Animation>,
}

impl Animations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `animation` under `key`, replacing (and therefore restarting)
    /// whatever was there.
    pub fn play(&mut self, key: impl Into<String>, animation: Animation) {
        self.animations.insert(key.into(), animation);
    }

    /// Step every animation in the set and report **whether any of them
    /// still wants more frames** — the same scheduler signal
    /// [`Animation::advance`] returns, for the whole set at once.
    pub fn advance(&mut self, dt: Duration) -> bool {
        let mut any_running = false;
        for animation in self.animations.values_mut() {
            // `|=`, not `||=`: every animation must be advanced, not just
            // the ones before the first one still running.
            any_running |= animation.advance(dt);
        }
        any_running
    }

    /// Eased weight for `key`, or `0.0` if no such animation exists — a
    /// missing animation reads as "not started", which is the sane default
    /// for the fade/slide/scale cases this drives.
    pub fn weight(&self, key: &str) -> f32 {
        self.animations.get(key).map_or(0.0, Animation::weight)
    }

    /// Raw progress for `key`, or `0.0` if absent.
    pub fn progress(&self, key: &str) -> f32 {
        self.animations.get(key).map_or(0.0, Animation::progress)
    }

    /// [`Animation::value`] for `key`, or `from` if absent.
    pub fn value(&self, key: &str, from: f32, to: f32) -> f32 {
        self.animations.get(key).map_or(from, |a| a.value(from, to))
    }

    /// Whether `key` has finished. An absent key counts as finished —
    /// "nothing is running under that name" is the useful answer.
    pub fn is_finished(&self, key: &str) -> bool {
        self.animations.get(key).is_none_or(Animation::is_finished)
    }

    pub fn get(&self, key: &str) -> Option<&Animation> {
        self.animations.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Animation> {
        self.animations.get_mut(key)
    }

    /// Remove and return the animation under `key`.
    pub fn stop(&mut self, key: &str) -> Option<Animation> {
        self.animations.remove(key)
    }

    pub fn clear(&mut self) {
        self.animations.clear();
    }

    pub fn len(&self) -> usize {
        self.animations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.animations.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HALF: Duration = Duration::from_millis(500);
    const FULL: Duration = Duration::from_millis(1000);

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn linear_weight_tracks_elapsed_time() {
        let mut anim = Animation::new(FULL, Easing::Linear);
        assert_eq!(anim.weight(), 0.0);

        assert!(anim.advance(HALF), "half-done animation still wants frames");
        assert!(close(anim.weight(), 0.5), "got {}", anim.weight());

        assert!(!anim.advance(HALF), "a completed one-shot stops asking for frames");
        assert_eq!(anim.weight(), 1.0);
        assert!(anim.is_finished());
    }

    #[test]
    fn finished_animation_holds_its_end_value() {
        // The retention rule the registry depends on: a completed fade must
        // not read back as "not started".
        let mut anim = Animation::new(FULL, Easing::EaseOut);
        anim.advance(FULL * 4);
        assert_eq!(anim.weight(), 1.0);
        assert!(!anim.advance(FULL), "already finished, no more frames wanted");
        assert_eq!(anim.weight(), 1.0, "weight must not decay after finishing");
    }

    #[test]
    fn loop_wraps_and_never_finishes() {
        let mut anim = Animation::new(FULL, Easing::Linear).looping();
        assert!(anim.advance(FULL + HALF));
        assert!(close(anim.progress(), 0.5), "got {}", anim.progress());
        assert!(!anim.is_finished());
        assert!(anim.advance(FULL * 100), "looping animations run until paused");
    }

    #[test]
    fn ping_pong_reverses_instead_of_snapping() {
        let mut anim = Animation::new(FULL, Easing::Linear).ping_pong();
        anim.advance(HALF);
        assert!(close(anim.progress(), 0.5), "got {}", anim.progress());
        anim.advance(FULL);
        // 1.5s into a 2s cycle -> travelling back down, halfway.
        assert!(close(anim.progress(), 0.5), "got {}", anim.progress());
        anim.advance(HALF);
        assert!(close(anim.progress(), 0.0), "got {}", anim.progress());
    }

    #[test]
    fn pause_stops_requesting_frames_and_resume_continues() {
        let mut anim = Animation::new(FULL, Easing::Linear);
        anim.advance(HALF);
        anim.pause();
        assert!(!anim.advance(HALF), "paused animations want no frames");
        assert!(close(anim.weight(), 0.5), "paused animation must not advance");

        anim.resume();
        assert!(anim.advance(Duration::from_millis(250)));
        assert!(close(anim.weight(), 0.75), "got {}", anim.weight());
    }

    #[test]
    fn zero_duration_is_instant_not_a_panic() {
        let mut anim = Animation::new(Duration::ZERO, Easing::EaseInOut);
        assert!(!anim.advance(HALF));
        assert_eq!(anim.weight(), 1.0);
        assert!(anim.is_finished());
    }

    #[test]
    fn identity_bezier_matches_linear() {
        // cubic-bezier(0, 0, 1, 1) is the CSS spelling of `linear`; if the
        // x-axis solve were wrong this would come out as 3t^2-2t^3.
        for step in 0..=10 {
            let x = step as f32 / 10.0;
            assert!(
                close(Easing::CubicBezier(0.0, 0.0, 1.0, 1.0).apply(x), x),
                "identity bezier bent at x={x}"
            );
        }
    }

    #[test]
    fn easing_curves_are_anchored_and_correctly_biased() {
        for easing in [Easing::EaseIn, Easing::EaseOut, Easing::EaseInOut] {
            assert!(close(easing.apply(0.0), 0.0), "{easing:?} must start at 0");
            assert!(close(easing.apply(1.0), 1.0), "{easing:?} must end at 1");
        }
        // ease-in lags, ease-out leads, ease-in-out is symmetric at the middle.
        assert!(Easing::EaseIn.apply(0.5) < 0.5);
        assert!(Easing::EaseOut.apply(0.5) > 0.5);
        assert!(close(Easing::EaseInOut.apply(0.5), 0.5));
    }

    #[test]
    fn registry_advances_everything_and_reports_any_running() {
        let mut animations = Animations::new();
        animations.play("fade", Animation::new(FULL, Easing::Linear));
        animations.play("blink", Animation::new(FULL, Easing::Linear).looping());

        assert!(animations.advance(FULL * 2));
        assert!(close(animations.weight("fade"), 1.0), "the one-shot still advanced");
        assert!(animations.is_finished("fade"));
        assert!(!animations.is_finished("blink"));

        animations.stop("blink");
        assert!(!animations.advance(HALF), "only a finished one-shot left");
        assert!(close(animations.weight("fade"), 1.0), "finished entries are retained");
    }

    #[test]
    fn replaying_a_key_restarts_rather_than_accumulating() {
        let mut animations = Animations::new();
        animations.play("fade", Animation::new(FULL, Easing::Linear));
        animations.advance(HALF);
        assert!(close(animations.weight("fade"), 0.5));

        animations.play("fade", Animation::new(FULL, Easing::Linear));
        assert_eq!(animations.len(), 1, "keys bound the set; nothing accumulates");
        assert_eq!(animations.weight("fade"), 0.0);
    }

    #[test]
    fn missing_keys_read_as_not_started() {
        let animations = Animations::new();
        assert_eq!(animations.weight("nope"), 0.0);
        assert_eq!(animations.value("nope", 5.0, 10.0), 5.0);
        assert!(animations.is_finished("nope"));
    }
}
