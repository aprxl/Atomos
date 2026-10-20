# TODO

Deferred work with a known trigger. Each entry says *when* to do it, not
just what — an item with no trigger is a wish, not a task. Origin: the
2026-10-20 readiness audit (`Typewritter/docs/audits/atomos-2026-10-20/AUDIT.md`)
and the painter-order design (`docs/design/painter-order-and-clip-scopes.md`).

## Verification debt

### IME needs a real input method
**Trigger:** before an editor ships on Atomos.

Preedit, commit and candidate-window placement are implemented
(`Input::events`, `Input::preedit`, `Input::sync_ime`) and unit-tested with
synthetic `Ime` events, but never driven by a real input method. winit's
`KeyEvent` cannot be constructed in tests, so real key-event decoding is also
untested. April overrides compose keys with a separate Atomos-based system, so
local verification needs a real CJK input method (e.g. `ibus` +
`ibus-anthy`, Windows Japanese IME, macOS Kotoeri) and layouts with dead keys
and AltGr (French, German, Brazilian), plus macOS Option characters.

### macOS and Linux have not been run
**Trigger:** before calling Atomos cross-platform ready.

Ported untested from the fork: deferred surface reconfigure (macOS hang),
Metal timestamp gating, one-frame swap-chain latency, `FrameOutcome`
handling, per-platform shortcut-versus-text rules, display-rate ceiling
(`sync_display`). All development and tests so far ran on Windows / Vulkan.
Also verify minimize/restore, resize bursts, suspend/resume and surface loss
(the lost-surface path re-creates the surface in place and was never
exercised).

### Integrated GPU benchmark
**Trigger:** before setting frame-time targets, or if direct-layer replay
looks slow on a laptop.

`cargo test --release --lib renderer::bench -- --ignored --nocapture
--test-threads=1`. Results so far are discrete-GPU only. The open question is
design risk R1: every presented frame clears and resolves the whole MSAA target
and replays every direct layer. `CachePolicy::Always` is the escape hatch.

### Stencil8 and MSAA assumptions
**Trigger:** when the 4K memory numbers matter.

`RenderStats` assumes `Stencil8` costs 1 byte per sample; a backend may emulate
it with 4. Also unverified: that stencil tests run per sample on every backend
(clip-edge antialiasing is tested on the dev GPU only).

## Renderer

### Content-sized offscreen groups (design stage 5)
**Trigger:** an app uses more than a couple of layers with an effect or
`CachePolicy::Always`, or a blurred popup shows up in a memory profile.

Cached and effect layers still own a surface-sized texture (and blur a second
one), plus the shared MSAA at surface size. Plan: size each group to its
content bounds (blur padding, capped at the 32-tap limit), bucket to 256 px,
a scratch pool of matching-size MSAA colour+stencil targets (a resolve target
must match its attachment), a `Target { origin, size }` uniform, one glyphon
viewport per target size, group text prepared with the origin subtracted.
Effects that create alpha (matrix with an alpha constant, any `Custom`) need
clip or surface bounds. April does not expect his next app to use blur.

### Resource budget and demotion (design stage 6)
**Trigger:** together with content-sized groups.

An offscreen-bytes budget (suggested max(64 MiB, 2x surface RGBA)); over
budget, `Always` groups without an effect demote to direct, least-recently
dirty first; `RenderStats` gains an overrun counter. Skip layers whose bounds
miss the surface.

### Text-batch merging (design stage 7)
**Trigger:** `RenderStats::text_batches` is large in a real app's profile
(an editor interleaving line backgrounds with text), or a profile shows
glyphon prepare/draw overhead.

Each run of consecutive text commands is its own glyphon renderer. 64 batches
cost 0.27 ms per frame in release, so this is not urgent. Plan: a command may
join an earlier batch of its kind if its physical bounds intersect nothing
drawn in between; overlapping commands are never reordered.

### Opaque-rect skipping
**Trigger:** profiling shows overdraw from covered full-screen direct layers.

Explicit `Layer::set_opaque_rect`, not inference.

### Adaptive MSAA on display change
**Trigger:** windows move between very different displays in practice.

The MSAA sample count is fixed at `Renderer::new` (capped to 4x above 1440p by
a 160 MiB budget) because every pipeline is built for it. A window created on
a 1080p display and dragged to 4K keeps 8x (316 MiB); the reverse keeps 4x.
Fix needs pipeline and glyphon-renderer rebuilds on a count change.

### Text prepare failure does not request a redraw
**Trigger:** a real "atlas full" report.

When a text prepare fails even after an atlas reset the layer retries on its
next frame, but under a demand-driven scheduler nothing asks for that frame.
`render()` could report it (e.g. `FrameOutcome::Retry`).

### `Custom` effect `uv` is surface-wide
**Trigger:** content-sized groups. Design question Q3: make `uv` group-local,
or keep `Custom` groups surface-sized.

## Text

### Wrapping for styled text and wrap options
**Trigger:** an app needs wrapped `draw_styled_text` (e.g. wrapped,
syntax-highlighted lines), or hanging indents / `Wrap::Glyph`-only /
justified lines.

`draw_wrapped_text`, `get_wrapped_text_size` and `layout_wrapped_text` wrap
plain text to a logical-pixel width (word boundaries, glyph breaks for an
over-long word); `draw_styled_text` and its size query still never wrap. Wrapped
text is left-aligned inside its block, and each distinct width is its own
shape-cache entry (animating the width re-shapes every frame). A wrapped line's
swallowed trailing whitespace has no glyph, so selection rectangles stop at the
last visible glyph.

### Shaped-glyph data and appearance effects from the fork
**Trigger:** PDF/export or an interaction effect is needed.

`FontParameters::slant` (synthetic italic, with its draw-hash regression) is
ported. Not ported: `ShapedText`/`FaceId`/`ShapedGlyph` for export,
filled-plus-stroked synthetic bold (the fork changed bold's construction),
focus-band and radial-wipe effects, `capture_into`/`freeze` frame snapshots.
Only port what an app needs.

### Font and typography capabilities not in the renderer
**Trigger:** an app needs them. True RGB subpixel AA, variable-font axes,
OpenType feature controls, dashed strokes, general affine transforms, shader
chains, complete SVG documents.

## Platform

### Driver gaps
**Trigger:** the app needs them.

No shared multi-window driver, accessibility tree / OS text semantics,
touch/pen/gesture abstraction, drag-and-drop, or background-job wake API.
Accessibility should come early if the product has editable text or ordinary
desktop controls. Clipboard and native dialogs live in Typewritter's
application layer (arboard / rfd), not Atomos. Device-loss recovery does not
exist (`Renderer::new` still `expect`s on adapter and device).

### Stable image and path resources
**Trigger:** before image- or canvas-heavy work (plots, large images).

Images are re-uploaded and paths re-tessellated whenever a layer rebuilds.
Needs image/path handles, transforms separate from resource identity, and
reusable geometry capacity. Profile dirty scrolling/zooming first.

### Timing diagnostics
**Trigger:** before performance targets are set for an app.

`get_render_time()` is really renderer uptime and `get_frametime()` includes
idle and pacing. Add encode/submit/present timing and upload/cache counters
next to `RenderStats`.

## Project

### Atomos `AGENTS.md` and docs cleanup
**Trigger:** when April asks. He wants Atomos's own `AGENTS.md`. `PROJECT.md`
is the old editor roadmap (marked historical).

### Typewritter migration
**Trigger:** when April decides. Typewritter is untouched on purpose: its
vendored platform copy should be deleted in favour of the `atomos` crate (path
or git dependency), after porting its remaining callers to `with_clip` /
`set_clip`, `FrameOutcome`, `InputEvent`, and logical-pixel layers.
