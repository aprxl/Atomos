# Atomos Renderer — API reference (AI-oriented)

Quick-reference for writing code against `src/renderer/`. Everything here is
verified against the source; when in doubt, the rustdoc on the item wins.

## Mental model

- **`Renderer`** owns the window surface, GPU device, and a stack of layers.
  You never touch wgpu types — they are private to `src/renderer/`.
- **`Layer`** is the drawing surface. All `draw_*` methods live on `Layer`,
  not `Renderer`. Each layer renders into its own offscreen texture and is
  composited bottom-to-top each frame.
- **All public coordinates are logical pixels.** DPI scaling happens inside
  every `draw_*`/measure call. Never multiply by scale factor yourself;
  `layer.scale_factor()` exists only for asset-selection decisions (e.g.
  picking a @2x image).
- A `Layer` is `Rc`-backed and cheap to clone. **Dropping the last handle
  removes the layer from the screen** — the renderer holds only a `Weak`.
  Keep layers alive in your app state.

## Setup and frame loop

```rust
mod renderer;
use renderer::*;

// In resumed():
let renderer = pollster::block_on(renderer::Renderer::new(window.clone()));
let layer = renderer.new_layer_top(LayerInvalidation::Automatic);

// In window_event():
//   WindowEvent::Resized(size)            => renderer.resize(size.width, size.height)  // physical px — the one exception
//   WindowEvent::ScaleFactorChanged {..}  => renderer.set_scale_factor(scale_factor)
//   WindowEvent::RedrawRequested          => renderer.render()?  (then window.request_redraw())
```

`Renderer` methods:

| Method | Notes |
|---|---|
| `new(window: Arc<Window>) -> Renderer` | async — call via `pollster::block_on` |
| `new_layer_top(invalidation) -> Layer` | drawn last (above everything) |
| `new_layer_bottom(invalidation) -> Layer` | drawn first (below everything) |
| `render() -> Result<(), RenderError>` | one frame; recoverable surface states are handled internally |
| `resize(width: u32, height: u32)` | **physical** pixels, straight from `WindowEvent::Resized` |
| `set_scale_factor(f64)` / `scale_factor() -> f64` | wire to `ScaleFactorChanged` |
| `get_frametime() -> Duration` | wall-clock frame-to-frame |
| `get_render_time() -> Duration` | CPU time inside `render()` |
| `get_gpu_frametime() -> Option<Duration>` | GPU timestamps; `None` if unsupported |
| `handle_event(&WindowEvent)` | forward every event; currently a no-op seam |

## Invalidation modes (choose at layer creation)

- **`LayerInvalidation::Automatic`** — immediate-mode. Re-issue *all* `draw_*`
  calls every frame. The layer hashes the commands and skips the GPU rebuild
  when nothing changed, so redrawing identical content is cheap.
- **`LayerInvalidation::Manual`** — retained. `draw_*` calls accumulate
  forever. Call `layer.clear()` then re-issue draws when content changes.
  Never re-issue draws without clearing first (they pile up).

Use `Manual` for content that rarely changes (chrome, static panels),
`Automatic` for per-frame content. Both skip all work on unchanged frames.

## Draw API (all on `Layer`)

```rust
draw_rectangle(top_left: (f32,f32), size: (f32,f32), color: Color, rounding: Rounding)
draw_polygon(vertices: &[(f32,f32)], color: Color)          // implicitly closed
draw_circle(center: (f32,f32), radius: f32, color: Color)   // no PerVertex
draw_path(d: &str, position: (f32,f32), paint: PathPaint) -> Result<(), ParseError>
draw_svg_icon(d: &str, viewbox: (f32,f32,f32,f32), size: (f32,f32),
              position: (f32,f32), paint: PathPaint) -> Result<(), ParseError>
draw_path_from_file(path: &Path, position: (f32,f32), paint: PathPaint) -> Result<(), PathFileError>
draw_image(pixels: Pixels, top_left: (f32,f32), size: (f32,f32), tint: Color)
draw_image_from_file(path: &Path, top_left: (f32,f32), size: (f32,f32), tint: Color) -> Result<(), ImageError>
draw_text(text: &str, position: (f32,f32), color: Color, alignment: Alignment,
          font: Font, font_parameters: FontParameters)      // Solid only
draw_styled_text(spans: &[TextSpan], position: (f32,f32), alignment: Alignment)
get_text_size(text, &Font, &FontParameters) -> (f32, f32)   // logical px, pre-alignment
get_styled_text_size(spans: &[TextSpan]) -> (f32, f32)
set_effect(effect: Option<ShaderEffect>)
set_clip_rect(rect: Option<((f32,f32),(f32,f32))>)   // (top_left, size), hardware scissor
set_clip_shape(shape: Option<ClipShape>) -> Result<(), ParseError>
clear()
scale_factor() -> f32
```

### Paths and icons

- `draw_path` takes raw SVG `<path d="...">` syntax (`M L H V C S Q T A Z`,
  absolute/relative, arcs included). It is **not** an SVG document parser —
  one path's `d` only, no `<svg>`/`transform`/`defs`.
- `draw_path`'s `position` is an *offset*; the path's authored units become
  logical pixels 1:1. Author paths at intended on-screen size.
- `draw_svg_icon` is for real-world icon assets: pass the source's
  `viewBox="min_x min_y w h"` as `viewbox` and the on-screen `size`; the remap
  is done for you. E.g. Material Symbols use `(0.0, -960.0, 960.0, 960.0)` —
  do **not** hand-transform their `d` strings, use `draw_svg_icon`.
- Parse errors are returned immediately at the `draw_*` call, not at render.

### Text

- `Font::Named("Iosevka")` (system family name), `Font::Bytes(include_bytes!(..))`,
  or `Font::File(path)`. Registration is cached — passing the same `Font`
  every frame is cheap.
- `FontParameters::new(size)` then set fields: `size` (px), `weight` (faux-bold,
  `0.0` = normal, continuous), `width` (faux-condense ratio, `1.0` = normal),
  `tracking` (extra letter-spacing in EM).
- `Alignment { horizontal, vertical }` says which point of the text's bounding
  box `position` anchors: `Alignment::TOP_LEFT`, `Alignment::CENTER`, or build
  from `HorizontalAlign::{Left,Center,Right}` × `VerticalAlign::{Top,Center,Bottom}`.
- For multi-colored runs (e.g. one syntax-highlighted line), build
  `TextSpan::new(text, font, font_parameters, color)` per token and make **one**
  `draw_styled_text` call — the whole span list is shaped once. Never loop
  `draw_text` per token.
- Full Unicode incl. color emoji works through the normal text calls.

### Colors and paint

- `Color::rgb(r,g,b)` / `Color::rgba(r,g,b,a)` / `Color::gradient(a, b, GradientDirection::{Horizontal,Vertical})`
  / `Color::PerVertex([tl, tr, br, bl])` (bilinear across the bounding box).
- Variant restrictions (violations are `debug_assert`s — they pass silently in
  release, so get them right): `draw_text`/`TextSpan` → `Solid` only;
  `draw_circle`/`draw_path`/`draw_svg_icon` → `Solid` or `Gradient`;
  `draw_rectangle`/`draw_polygon`/`draw_image` → anything.
- `PathPaint::fill(color)` for the common case; otherwise
  `PathPaint::Fill { color, rule: FillRule::{NonZero,EvenOdd} }`,
  `PathPaint::Stroke(Stroke)`, or `PathPaint::FillAndStroke { fill_color, fill_rule, stroke }`.
- `Stroke::new(color, width)` gives SVG defaults (butt cap, miter join,
  miter limit 4.0); override `cap`/`join`/`miter_limit` fields as needed.
- `Rounding::NONE`, `Rounding::uniform(radius)`, or per-corner struct literal.
- `Pixels::new(width, height, rgba8_vec)` — panics unless `len == w*h*4`.

### Clipping

Two mechanisms, both per-layer, both logical px, both clearable with `None`:

- **`set_clip_rect(Some((top_left, size)))`** — hardware scissor on the
  layer's render pass. Clips geometry, images, and text together.
  Effectively free (it *reduces* GPU work); changing it every frame costs
  nothing. Default choice for scroll viewports, panes, popup bounds. Rects
  partially outside the surface are clamped; a fully off-surface rect
  renders nothing.
- **`set_clip_shape(Some(ClipShape::...))`** — arbitrary-shape mask applied
  to the layer's *composited* output (same whole-layer unit as
  `set_effect`; an active effect is masked too, so blur can't bleed outside
  the shape). `ClipShape::{Rectangle{top_left,size,rounding}, Circle{center,radius},
  Polygon{vertices}, Path{d,position}}` — mirrors the solid `draw_*` calls,
  antialiased edges included. Cost: one mask texture (surface-sized) +
  one extra fullscreen texture sample per composited frame; re-setting the
  shape re-tessellates and re-renders the mask (cheap enough to animate
  per frame — the mask texture is reused). `Path` variant validates `d` and
  can return `ParseError`; other variants never fail.

Rule of thumb: unrotated rectangle → `set_clip_rect`; anything else →
`set_clip_shape`. Both can be active at once (scissor while rendering,
mask while compositing).

## Shader effects

Effects apply to a **whole layer's** composited output (`layer.set_effect`).
To shade one shape, give it its own layer. Setting `None` clears.

- `ShaderEffect::Blur { radius }` — Gaussian, radius in logical px, internally capped.
- `ShaderEffect::ColorMatrix(m)` — `ColorMatrix::{IDENTITY, grayscale(), invert(),
  sepia(), saturate(x), brightness(x), contrast(x), tint(color, amount)}`, or a
  raw 4×5 row-major `ColorMatrix([f32; 20])` (same model as SVG `feColorMatrix`).
- `ShaderEffect::Custom(&'static str)` — full WGSL module against a fixed
  contract (copy the template in `src/renderer/shader.rs`'s rustdoc and edit
  only `fs_main`'s body; `vs_main` + `@group(0)` bindings must match exactly).
  Invalid WGSL is caught at `set_effect`, logged, and the effect is dropped —
  no panic.

## Gotchas (things that will bite you)

1. **Within a single layer, all text renders after all geometry**, regardless
   of `draw_*` call order. You cannot draw a rectangle "over" text in the
   same layer. Occlusion across content types = separate layers
   (`new_layer_bottom` for content, `new_layer_top` for chrome).
2. Draw-call order *does* control geometry-over-geometry and text-over-text
   stacking within a layer.
3. `Manual` layers: forgetting `clear()` before redrawing duplicates content.
4. Dropped `Layer` handle = layer vanishes from screen (Weak-ref registry).
5. `Renderer::resize` takes **physical** px (from winit); everything else is
   logical.
6. `draw_image` takes `Pixels` by value — clone if you draw it again.
7. `cargo check` passing does **not** mean the renderer works: wgpu validation
   and WGSL errors only surface at runtime. After any renderer change, smoke
   test: `timeout 6 cargo run > /tmp/atomos_run.log 2>&1`; exit 124/143 = ran
   clean; then grep the log for `panic|wgpu error`.
8. Use glyphon's re-exports for cosmic-text types (`glyphon::Buffer`, etc.) —
   the standalone `cosmic-text` in Cargo.toml is a different version.
9. Clipping is **per layer**, like effects — there is no per-draw-call clip.
   Independently clipped regions (N scroll viewports) = N layers. Reach for
   `set_clip_rect` (free) before `set_clip_shape` (mask texture + one extra
   fullscreen sample per frame); a `ClipShape::Rectangle` with
   `Rounding::NONE` is a silent waste — that's exactly what `set_clip_rect`
   does for free.
10. Clip state is caller-driven like `set_effect`, **not** part of the
    drawn content: it survives `clear()` and `Automatic` re-issue, and it is
    not hashed for invalidation. Clear it explicitly with `None`.

## Minimal example

```rust
let chrome = renderer.new_layer_top(LayerInvalidation::Manual);
chrome.draw_rectangle((0.0, 0.0), (1024.0, 40.0), Color::rgb(0x28, 0x2C, 0x34), Rounding::NONE);
chrome.draw_text(
    "hello.rs",
    (12.0, 20.0),
    Color::rgb(0xAB, 0xB2, 0xBF),
    Alignment { horizontal: HorizontalAlign::Left, vertical: VerticalAlign::Center },
    Font::Named("Iosevka".into()),
    FontParameters::new(14.0),
);
chrome.draw_svg_icon(
    ICON_D, (0.0, -960.0, 960.0, 960.0), (16.0, 16.0), (980.0, 12.0),
    PathPaint::fill(Color::rgb(0xAB, 0xB2, 0xBF)),
)?;
// Manual layer: drawn once; call chrome.clear() + redraw only when it changes.

// A scrolling viewport: scissor once, draw past the edges freely each frame.
let viewport = renderer.new_layer_bottom(LayerInvalidation::Automatic);
viewport.set_clip_rect(Some(((0.0, 40.0), (1024.0, 680.0))));

// A rounded panel that actually clips its content (text included):
let popup = renderer.new_layer_top(LayerInvalidation::Automatic);
popup.set_clip_shape(Some(ClipShape::Rectangle {
    top_left: (312.0, 200.0),
    size: (400.0, 320.0),
    rounding: Rounding::uniform(16.0),
}))?;
```

See `src/bin/clip_demo.rs` for a full walkthrough of both clip paths,
including a per-frame animated mask.
