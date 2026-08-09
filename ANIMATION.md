# Atomos Animation — API reference (AI-oriented)

`src/animation.rs` (`Animation`, `Easing`, `Repeat`, `Animations`) and `src/frame.rs` (`FrameScheduler`). They are documented together because the coupling between them is one method's return value, and neither makes sense alone.

No dependencies beyond `winit` and the standard library.

Companion docs: [`RENDERER.md`](./RENDERER.md), [`INPUT.md`](./INPUT.md).

## Mental model

`Animation` is a timer that produces a **0..1 weight** over wall-clock time. That is the whole primitive. Everything an editor animates — cursor blink, smooth scroll, a popup fading in, a diagnostic pulsing — is some value interpolated by a weight this produces.

Keyframes, springs, and sequences are things built *on* this later. None of them need to exist for it to be useful.

The one non-obvious rule:

> **`advance(dt)` returns whether the animation still wants more frames.**

That return value is the only coupling between animation and the frame scheduler. Atomos draws nothing unless something asks; a running animation is a thing that asks, every frame, for as long as it runs.

## Two ways to own one

### 1. Caller-owned (the default)

Store an `Animation` in whatever struct the animated thing lives in. Right for almost everything — a cursor's blink belongs to the cursor.

```rust
use std::time::Duration;
use animation::{Animation, Easing};

struct Cursor {
    blink: Animation,
}

let mut blink = Animation::new(Duration::from_millis(500), Easing::Linear).ping_pong();

// once per frame:
let still_running = blink.advance(dt);
let alpha = blink.weight();          // 0..1
```

### 2. Keyed registry (opt-in)

`Animations` is a `HashMap<String, Animation>`. Reach for it **only** when something needs to drive an animation it does not own — a Lua plugin fading a panel it did not create. One `advance` drives them all.

```rust
use animation::{Animation, Animations, Easing};

animations.play("panel.fade", Animation::new(Duration::from_millis(120), Easing::EaseOut));
let any_running = animations.advance(dt);
let alpha = animations.weight("panel.fade");
```

String keys rather than generational IDs, so a scripting layer can address an animation across an FFI boundary without holding a handle.

This is not a global registry for every animation ever. Default to caller-owned.

## `Animation`

### Construction

```rust
Animation::new(duration: Duration, easing: Easing) -> Animation   // Repeat::Once, already playing
    .looping()      -> Animation    // restart from 0 at the end, forever
    .ping_pong()    -> Animation    // reverse at each end, forever
    .paused()       -> Animation    // held at weight 0 until resumed
```

Builder methods consume and return `Self`, so they chain:

```rust
Animation::new(Duration::from_millis(900), Easing::EaseInOut).ping_pong()
```

### Driving

```rust
advance(&mut self, dt: Duration) -> bool
```

Steps the clock and returns **still running**. A paused animation returns `false` without advancing. `Loop` and `PingPong` return `true` until paused — they never finish on their own.

### Reading

```rust
progress() -> f32              // raw, unshaped, 0.0..=1.0
weight()   -> f32              // progress shaped by the easing curve — use this
value(from: f32, to: f32) -> f32   // from at weight 0, to at weight 1
```

`progress()` already accounts for `PingPong` reversal: it travels 0 → 1 → 0, not 0 → 1 → 2.

`value` is the convenience you want most of the time:

```rust
let radius = anim.value(0.0, 16.0);
let x = anim.value(panel_hidden_x, panel_shown_x);
```

### Control and state

```rust
is_finished() -> bool          // only ever true for Repeat::Once
is_playing()  -> bool
restart()                      // jump to 0 and play, from any state
pause()                        // freeze at the current weight
resume()                       // continue; no-op on a finished one-shot
duration() -> Duration
easing()   -> Easing
repeat()   -> Repeat
```

`resume()` on a finished one-shot deliberately does nothing — use `restart()` to replay it.

## `Easing`

```rust
Easing::Linear
Easing::EaseIn                          // cubic-bezier(0.42, 0, 1, 1)     slow start, abrupt stop
Easing::EaseOut                         // cubic-bezier(0, 0, 0.58, 1)     abrupt start, slow stop
Easing::EaseInOut                       // cubic-bezier(0.42, 0, 0.58, 1)  slow at both ends
Easing::CubicBezier(x1, y1, x2, y2)     // arbitrary CSS timing function
Easing::apply(self, progress: f32) -> f32
```

The named curves are the CSS ones and delegate to their spec control points, not to hand-rolled polynomial lookalikes — so `EaseOut` matches what a designer means by "ease-out". `EaseOut` is the usual choice for UI appearing in response to a user action.

`CubicBezier` is the two control points of a unit cubic Bézier from `(0,0)` to `(1,1)`. `x` components are clamped to `0..=1` (a non-monotonic curve has no single solution for a given time); `y` is **unclamped**, so overshooting "back" easings work:

```rust
Easing::CubicBezier(0.34, 1.56, 0.64, 1.0)   // overshoots past 1.0, settles back
```

Inverting the curve's x-axis uses 8 Newton iterations, the same figure browser engines settle on.

**`t` is not time.** The Bézier's parameter is not the progress value; that is what the inversion is for. Do not try to shortcut it.

## `Repeat`

```rust
Repeat::Once        // stop at weight 1.0 and stay there. The only mode that finishes.
Repeat::Loop        // snap back to 0.0, run again, forever
Repeat::PingPong    // 0 → 1, then 1 → 0, forever
```

`PingPong` for a cursor blink or pulsing highlight — `Loop`'s hard snap reads as a glitch.

## `Animations` (registry)

```rust
Animations::new() -> Animations
play(key: impl Into<String>, animation: Animation)   // insert-or-replace
advance(dt: Duration) -> bool                        // any still running
weight(key: &str)   -> f32                           // 0.0 if absent
progress(key: &str) -> f32                           // 0.0 if absent
value(key: &str, from: f32, to: f32) -> f32          // `from` if absent
is_finished(key: &str) -> bool                       // true if absent
get(key: &str)     -> Option<&Animation>
get_mut(key: &str) -> Option<&mut Animation>
stop(key: &str)    -> Option<Animation>              // remove and return
clear()
len() -> usize
is_empty() -> bool
```

`play` on an existing key **replaces** it, so a caller re-triggering `"panel.fade"` a thousand times still holds one animation. The set stays bounded by the number of distinct things being animated, which is why keys beat IDs here.

**Finished animations are deliberately not evicted.** `weight()` on a completed fade must keep returning 1.0, or the element it drives snaps back to invisible on the very frame the fade lands. Use `stop` or `clear` to actually remove them.

A missing key reads as "not started" (`0.0`), which is the sane default for fade/slide/scale.

## `FrameScheduler`

`src/frame.rs`. Decides whether the event loop sleeps or draws.

The naive winit loop — `ControlFlow::Poll` plus an unconditional `request_redraw()` — renders forever at display refresh rate whether or not anything changed. An editor is idle almost all the time; that is the wrong default. `FrameScheduler` inverts it: **draw nothing until something asks.**

"Dirty" and "animating" are the same concept here. Animating is just asking again every frame. Only the pacing differs, which is why the FPS knob lives on the scheduler and not on a separate animation clock.

### API

```rust
FrameScheduler::new() -> FrameScheduler      // starts with one frame already owed
request_redraw()                             // "another frame is needed"; idempotent
wants_redraw()   -> bool                     // is a frame owed at all? (ignores pacing)
should_draw_now() -> bool                    // owed AND past the FPS deadline — use this
begin_frame()                                // call at the TOP of RedrawRequested
control_flow()   -> ControlFlow
set_target_fps(fps: Option<f32>)             // None = follow the display
target_fps()     -> Option<f32>
FrameScheduler::animation_delta(frametime: Duration) -> Duration   // associated fn
```

### Wiring

```rust
fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
    if self.scheduler.should_draw_now()
        && let Some(w) = &self.window
    {
        w.request_redraw();
    }
    event_loop.set_control_flow(self.scheduler.control_flow());
}

// in WindowEvent::RedrawRequested:
self.scheduler.begin_frame();                       // FIRST, before per-frame logic

let dt = FrameScheduler::animation_delta(renderer.get_frametime());
if self.animations.advance(dt) {
    self.scheduler.request_redraw();                // still animating -> one more frame
}
// ... draw ...
self.input.end_frame();
```

Also call `request_redraw()` explicitly on `Resized` and `ScaleFactorChanged` — neither flows through `Input`.

### `control_flow` behaviour

| State | Returns |
|---|---|
| Nothing owed | `ControlFlow::Wait` — sleep until an OS event |
| Owed, no FPS target | `ControlFlow::Poll` — vsync paces it |
| Owed, with a target | `ControlFlow::WaitUntil(last_frame + 1/fps)` |

`WaitUntil` is the only way to run *slower* than the display — battery throttling, a deliberate 30 fps cap.

## Gotchas

1. **`begin_frame()` goes at the *top* of the frame, not the bottom.** It clears the pending request, so anything the frame itself requests survives to the next `about_to_wait`. Clearing at the end swallows the "still animating" request and the animation stalls after exactly one frame. Pinned by the test `a_request_during_a_frame_survives_to_the_next`.

2. **Gate on `should_draw_now()`, not `wants_redraw()`.** `window.request_redraw()` queues a `RedrawRequested` that winit services on the next loop iteration **whatever `ControlFlow` says**. Requesting as soon as a frame is owed makes `WaitUntil` decorative and `set_target_fps` a silent no-op. Pinned by `a_target_actually_withholds_the_redraw_request`.

3. **Always clamp `dt`.** Under `Wait`, the first frame after an idle stretch reports however long the window sat untouched — seconds. Feeding that to `advance` teleports every animation to its end state, and it presents as "animations randomly skip". `FrameScheduler::animation_delta` clamps to `MAX_ANIMATION_DELTA` (100 ms). Use it; do not pass `get_frametime()` raw.

4. **`advance` must be called on every animation, every frame.** In a loop, accumulate with `|=` and never `||`— short-circuiting stops advancing everything after the first one still running.

5. **Never request a redraw unconditionally.** That restores the always-drawing default the scheduler exists to remove. Requests come from input state changes and from `advance` returning `true`.

6. **A finished animation still reports its final weight.** That is intentional. Do not "clean up" by evicting it from the registry on completion.

7. **`FrameScheduler::new()` starts with a frame owed.** Under `Wait`, a scheduler that started clean would never paint anything and the window would stay blank.

8. **`is_finished()` is never true for `Loop`/`PingPong`.** They run until paused.

## Worked example

`src/bin/input_demo.rs` has an animation panel with one caller-owned bar and one driven through the registry, plus a frame counter that visibly freezes when nothing is animating and nothing is being touched — the proof that frames are demand-driven.

```bash
cargo run --bin input_demo
```

`src/bin/shader_demo.rs` drives a blur radius from an `Animation` and shows CPU and GPU frametime alongside it.

## Deferred

Keyframe tracks, spring physics, and animation sequencing/chaining. All build on `Animation` without changing it. Not needed until something asks for them.
