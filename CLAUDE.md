# Atomos

**Spiritual successor to Emacs.** Pure-Rust core, Lua scripting, hand-rolled GPU renderer (wgpu + glyphon + lyon). Text editor that's a programmable runtime, not an application with a scripting bolt-on.

## Quick facts

- **Location:** `~/.dev/Atomos/`
- **Stack:** winit (windowing) → wgpu (GPU) → glyphon (text) + lyon (vector paths) → cosmic-text (layout)
- **Language:** Rust (edition 2024), wgpu 0.29 (pinned to match glyphon)
- **State:** Phase 0 — renderer foundation. Rectangle + text + rounded rect on screen. No editor logic yet.
- **Cargo.toml** has all dependencies; **PROJECT.md** is the full design doc.

## Architecture invariants

- All wgpu types are private to `src/renderer.rs`. Callers only see `Renderer` and its draw methods.
- The `Renderer` API will be `draw_rect`, `draw_text`, `draw_path`, `draw_image`, `flush` (not yet abstracted — capabilities first, API second).
- `cosmic-text 0.19.0` is in `Cargo.toml` but unused — glyphon pulls `cosmic-text 0.18.2` internally. Use glyphon's re-exports (`glyphon::Buffer`, `glyphon::FontSystem`, etc.).
- wgpu version follows glyphon (currently 29.0.4). Upgrade both together.

## Guidelines

- **No `exec` without explicit approval.** This is April's personal machine, not a sandbox.
- Read `PROJECT.md` for full context before any substantial work.
- Keep code clean, no warnings. `cargo check` must pass.

## Full project doc

See [`PROJECT.md`](./PROJECT.md).