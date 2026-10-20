# Atomos

**Native application backend, as a library.** Rendering, layout, styling, input and output on wgpu + winit (glyphon + lyon for text and paths). Other projects (e.g. Typewritter) depend on it; it is not an editor.

## Quick facts

- **Location:** `~/Develop/Atomos`
- **Layout:** single library crate `atomos` (`src/lib.rs`); demos in `examples/`
- **Language:** Rust (edition 2024, `rust-version` 1.98), wgpu 0.29 (pinned to match glyphon)
- **Status:** being hardened for serious use; see the readiness audit in the Typewritter repo (`docs/audits/atomos-2026-10-20/AUDIT.md`).
- **PROJECT.md** is the old editor roadmap, kept for history; it does not describe this library.

## Architecture invariants

- All wgpu types are private to `src/renderer/`. Callers only see `Renderer` and its draw methods.
- The `Renderer` API will be `draw_rect`, `draw_text`, `draw_path`, `draw_image`, `flush` (not yet abstracted — capabilities first, API second).
- `cosmic-text` is not a direct dependency; glyphon pulls it in. Use glyphon's re-exports (`glyphon::Buffer`, `glyphon::FontSystem`, etc.).
- wgpu version follows glyphon (currently 29.0.4). Upgrade both together.

## Guidelines

- **No `exec` without explicit approval.** This is April's personal machine, not a sandbox.
- Development uses Haiku agents for exploration, Sonnet for code, Opus for design; never run several Sonnet/Opus agents at once.
- Keep code clean, no warnings. `cargo check` must pass.

