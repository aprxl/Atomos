# Atomos
## A native application backend for Rust

Atomos is a library: the rendering, layout, styling, input and output systems
for cross-platform native applications, built on wgpu and winit as a
replacement for Electron-style stacks. Applications depend on it and own their
event loop.

- `atomos::renderer` — GPU layers, text, paths, images, effects
- `atomos::input` — window events gathered into per-frame input state
- `atomos::frame` — demand-driven frame scheduling
- `atomos::animation` — timed values and easing

Runnable examples live in `examples/` (`cargo run --example window`,
`clip_demo`, `input_demo`, `shader_demo`). Requires Rust 1.98+.

Atomos is in active development; see [TODO.md](TODO.md) and the reference docs
([RENDERER.md](RENDERER.md), [INPUT.md](INPUT.md), [ANIMATION.md](ANIMATION.md)).
