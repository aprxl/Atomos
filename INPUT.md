# Atomos Input — API reference (AI-oriented)

`src/input.rs`. One type: `Input`. No dependencies beyond `winit` and the standard library.

Companion docs: [`RENDERER.md`](./RENDERER.md), [`ANIMATION.md`](./ANIMATION.md).

## Mental model

winit hands you a *stream of events*. Almost every caller actually wants to ask a *state question* at an arbitrary point in a frame — "is Ctrl held right now?", "where is the cursor?", "did the user just press Enter?".

`Input` sits between the two. You feed it events; everything else is a cheap state query callable from anywhere, with no borrow of the event loop.

Three query flavours, and picking the wrong one is the most common mistake:

| Flavour | Example | Means |
|---|---|---|
| **Level** | `is_key_down` | True *every frame* the key stays held. |
| **Edge** | `is_key_pressed` | True on *exactly one frame* per physical press. Excludes OS auto-repeat. |
| **Repeat-edge** | `is_key_typed` | Like edge, but *includes* OS auto-repeat. |

Use level for "while held" behaviour (a modifier, a drag). Use edge for commands (save, toggle). Use repeat-edge for things the OS repeat rate should drive (backspace, arrow-key cursor movement).

## The frame contract

Get this wrong and every edge query is permanently true.

1. Forward every `WindowEvent` to `handle_event`.
2. Forward every `DeviceEvent` to `handle_device_event`, *if* you want raw mouse motion.
3. Read whatever state the frame needs.
4. Call `end_frame()` **exactly once**, at the end of the frame.

`end_frame` is what makes edge queries and deltas mean "since the last frame" rather than "since the program started". Held state (`is_key_down`, `mouse_position`, `modifiers`) survives it; edges, deltas, scroll, and typed text do not.

```rust
// in ApplicationHandler
fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
    if self.input.handle_event(&event) {
        self.scheduler.request_redraw();     // only state changes wake the renderer
    }
    match event {
        WindowEvent::RedrawRequested => {
            // ... read input, update, draw ...
            self.input.end_frame();          // exactly once, last
        }
        _ => {}
    }
}

fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
    self.input.handle_device_event(&event);
}
```

`handle_event` **returns `bool`**: whether the event actually changed input state. `Moved`, `Occluded`, `ThemeChanged`, `AxisMotion` and friends all reach it but return `false`, so a demand-driven scheduler does not wake for them. See [`ANIMATION.md`](./ANIMATION.md) for the scheduler side.

## Construction

```rust
Input::new(scale_factor: f64) -> Input
```

`scale_factor` is `window.scale_factor()`. It is kept current from `ScaleFactorChanged` afterwards, so pass the real value once at window creation and never think about it again.

If the window does not exist yet (an `App::default()` before `resumed`), construct with `1.0` and replace the whole `Input` in `resumed` — that is what `main.rs` does.

## Keyboard

```rust
is_key_down(key: KeyCode) -> bool        // level
is_key_pressed(key: KeyCode) -> bool     // edge, no auto-repeat
is_key_typed(key: KeyCode) -> bool       // edge, with auto-repeat
is_key_released(key: KeyCode) -> bool    // edge
keys_down() -> impl Iterator<Item = KeyCode>
```

`KeyCode` is a **physical key position**, layout-independent. `KeyCode::KeyQ` is the key where Q sits on a US layout, whatever that key produces on the user's actual layout. This is what you want for keybinds.

### Modifiers

```rust
modifiers() -> ModifiersState        // the raw bitflag set
shift() -> bool
ctrl() -> bool
alt() -> bool
super_key() -> bool                  // Super / Command / Windows
```

### Chords

```rust
is_shortcut_pressed(modifiers: ModifiersState, key: KeyCode) -> bool   // edge
is_shortcut_down(modifiers: ModifiersState, key: KeyCode) -> bool      // level
```

Both require **exact** modifier equality. `Ctrl+S` does not fire on `Ctrl+Shift+S`, because that is usually a different command. Combine flags with `|`:

```rust
use winit::keyboard::{KeyCode, ModifiersState};

if input.is_shortcut_pressed(ModifiersState::CONTROL, KeyCode::KeyS) { save(); }
if input.is_shortcut_pressed(ModifiersState::CONTROL | ModifiersState::SHIFT, KeyCode::KeyS) { save_as(); }
```

### Typed text

```rust
text() -> &str
```

Characters typed this frame, control characters stripped. Empty on frames with no typing. **This is the one to insert into a document** — it is layout- and modifier-resolved, so a French layout's `KeyCode::KeyQ` correctly yields `"a"`.

Never reconstruct text from `KeyCode`. `KeyCode` answers "which physical key"; `text()` answers "what did the user type". Only one of those is ever the right question.

Capped at 256 characters per frame (`MAX_TEXT_PER_FRAME`); a burst past that is a stuck key or an IME paste, and the excess is dropped rather than buffered forever.

## Mouse

```rust
is_mouse_down(button: MouseButton) -> bool       // level
is_mouse_pressed(button: MouseButton) -> bool    // edge
is_mouse_released(button: MouseButton) -> bool   // edge
buttons_down() -> impl Iterator<Item = MouseButton>

mouse_position() -> (f32, f32)
mouse_delta() -> (f32, f32)
raw_mouse_delta() -> (f32, f32)
scroll_delta() -> (f32, f32)
```

`mouse_position` is in **logical pixels** — the same coordinate space every `Layer::draw_*` method takes. A hit-test is therefore a direct comparison against drawn geometry, no conversion:

```rust
let (mx, my) = input.mouse_position();
let hovered = mx >= x && mx < x + w && my >= y && my < y + h;
```

`mouse_delta` is per-frame movement in logical pixels: clipped to the window and subject to OS pointer acceleration. This is the right measure for dragging UI.

`raw_mouse_delta` is unaccelerated, unclipped device motion from `DeviceEvent::MouseMotion`. Units are device-defined, **not pixels**. Zero unless `handle_device_event` is wired up. Use it for pointer-lock camera-style input, never for UI.

`scroll_delta` accumulates in **lines**. Positive `y` is scroll-up / content-down; positive `x` is scroll-right, matching winit. Pixel deltas from touchpads are converted at 20 logical pixels per line (`PIXELS_PER_LINE`) so callers only ever deal with one unit.

## Window state

```rust
is_focused() -> bool
is_cursor_in_window() -> bool
```

`mouse_position` retains its last known value while the cursor is outside the window, so check `is_cursor_in_window()` before treating a position as a live hover.

## Stuck-input safety

Losing focus mid-chord is the classic way input state desyncs: the OS delivers the press, then the release goes to whoever has focus now, and your Ctrl stays "held" forever.

`WindowEvent::Focused(false)` therefore force-releases everything held:

```rust
release_all()
```

Every held key and button is drained into the *released* sets, so it reports as a release edge this frame — press/release pairing (drag handlers, held-key repeat) unwinds cleanly instead of waiting forever. Modifiers and all deltas are cleared. Positions are **kept**, because the cursor did not move.

This runs automatically on focus loss. Call it manually when the app takes over input itself — opening a modal, entering a capture mode.

## Gotchas

1. **`end_frame()` exactly once, at the end.** Skipping it leaves every edge query stuck true. Calling it twice, or before reading, silently swallows a frame of input.
2. **`handle_event` returns `bool` for a reason.** Ignoring it and requesting a redraw unconditionally restores the always-drawing default the frame scheduler exists to remove.
3. **`is_key_pressed` excludes auto-repeat, `is_key_typed` includes it.** Holding backspace with `is_key_pressed` deletes exactly one character.
4. **Chord matching is exact.** `is_shortcut_pressed(CONTROL, KeyS)` is `false` while Shift is also held. That is intentional; if you want "Ctrl held, whatever else", test `ctrl() && is_key_pressed(...)` instead.
5. **`raw_mouse_delta` is not pixels** and is zero unless you forward `DeviceEvent`s.
6. **`mouse_position` is logical pixels, `Renderer::resize` takes physical pixels.** Do not mix them.
7. **IME is not implemented.** Dead keys, compose sequences, and CJK candidate windows do not work. `text()` receives only what winit's `KeyEvent::text` delivers. See [`TODO.md`](./TODO.md).

## Worked example

`src/bin/input_demo.rs` exercises the whole surface: key squares for a plain key and two chords, a hover circle, a scroll-resized cursor-follower, and a live readout of every raw field. Alt-tab away while holding Space to watch the stuck-key guarantee work.

```bash
cargo run --bin input_demo
```
