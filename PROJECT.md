# Project Description — A Modern, Opinionated Emacs Successor

> A standalone **buffer engine** with an absurdly extensible Lua scripting interface, where every piece of built-in functionality is itself a command that can be inspected, redefined, and re-executed at runtime. Shipped as a keyboard-centric GUI text editor, with all higher-level features (modal editing, Markdown previews, terminal, organization views) built on top of the same plugin API exposed to third parties.

This document is the development reference. It defines the philosophy, the architectural invariants that must not be violated, the technology stack, and a phased roadmap with concrete exit criteria for each phase.

---

## 1. Vision & Philosophy

The project is a *spiritual successor* to Emacs, not a clone and not a replacement. It keeps the one idea that made Emacs endure — **the editor is a programmable runtime, not an application with a scripting bolt-on** — and modernizes everything around it: Rust for the core, Lua instead of Emacs Lisp, a hand-rolled GPU renderer, and a text engine designed for rich, hypertext-style display from day one.

The guiding sentence for every design decision:

> If a built-in feature cannot be built using the same API a plugin author has, the API is wrong — not the feature.

This is the thesis the entire roadmap is structured to prove as early as possible.

### What "opinionated" means here

- **Keyboard-centric.** Mouse is supported but never required. Every action is reachable through a command and a keybinding.
- **Commands all the way down.** There is no privileged "core" behavior. Editing, navigation, rendering toggles, and feature activation are all commands in a single registry.
- **Lua orchestrates, Rust executes.** Lua decides *what* happens; Rust owns the rope, layout, and rendering. Lua is never in the per-keystroke hot path for heavy work.
- **The base editor is dumb on purpose.** Out of the box it is a plain modal text editor with no special features. Everything else — including the shipped features — arrives as plugins that consume the same API exposed to third parties.

---

## 2. Architectural Principles (Invariants)

These are non-negotiable. Violating one of them is a design bug, regardless of whether the feature works.

1. **Single command registry.** Built-ins and plugins register into the same table. Any command can be looked up, redefined, wrapped, or removed at runtime.
2. **Pure-Rust editing core.** The rope, cursor/selection model, and undo history are Rust-owned and Lua-visible, never Lua-owned. This keeps editing deterministic and fast.
3. **Hot-reloadable scripting.** Redefining a command or rebinding a key takes effect live, without restart.
4. **Transactional edits.** Every buffer mutation goes through a transaction log that powers undo/redo, change notifications, and (later) collaboration. No feature mutates the rope directly outside this path.
5. **Views are pluggable, buffers are primitive.** A buffer is the universal substrate. A "view" (editor, terminal, calendar, Markdown preview) is a renderer over a buffer or a buffer-like model. New view types must not require core changes.
6. **The render boundary is explicit.** Lua can *describe* decorations and overlays; it cannot draw pixels directly. This keeps rendering on the GPU fast path.

---

## 3. Rendering Architecture (Decided)

The rendering stack was evaluated during the project's inception against three candidates:

| Candidate | Verdict | Reason |
|---|---|---|
| **wgpu + glyphon + lyon** | ✅ **Chosen** | Full pipeline ownership, pure Rust, subpixel AA via glyph atlas, wgpu already native on Windows/Linux/macOS |
| **skia-safe** | ❌ Rejected | C++ build dependency, opaque internals, lives in Skia's world rather than ours |
| **Vello** | ⏸ Deferred | Pure Rust and excellent for free-form vector scenes, but alpha-stage, no native subpixel AA, and leaves all text layout to the caller — revisit for decorative layers in Phase 3 |

### Chosen stack and rationale

**winit** drives the event loop and window lifecycle. **wgpu** owns the GPU device, swap chain, and render passes — it is the backend for everything, providing Vulkan on Linux/Windows and Metal on macOS with no extra configuration. **glyphon** (built on `cosmic-text` + `swash` + `etagere`) handles text shaping, layout, glyph rasterization, and atlas packing, providing real subpixel AA via the atlas path. **lyon** tessellates Bézier paths and polygons into triangle meshes for non-text drawing (cursor, selections, UI chrome, decorations).

The renderer is hand-rolled on top of these: a `Renderer` struct that owns the wgpu state and exposes a high-level draw API (`draw_rect`, `draw_text`, `draw_path`, `draw_image`). Callers — including the Lua layer — never touch wgpu directly. The wgpu boilerplate is written once, wrapped, and invisible to the rest of the project.

### The subpixel decision *(revised during Phase 0)*

True RGB ClearType-style subpixel AA and arbitrary GPU vector compositing are fundamentally in tension on a shared surface. The position as actually built: **grayscale coverage-mask AA with hinting and physical-pixel snapping**, rendered through Atomos's own swash-driven glyph rasterizer (`glyph_effects.rs`) rather than glyphon's built-in path — which is what enables the continuous faux-bold/condense effects and color emoji. This matches where modern GPU-composited renderers landed (VS Code, Chrome, and macOS all dropped true RGB subpixel AA for the same reasons: it breaks under scaling, rotation, and compositing). Crispness comes from rasterizing at native physical-pixel density (DPI-aware sizing) plus hinting, not from LCD subpixel structure. Vello's grayscale-only path is not a regression here; it was simply not chosen as the base.

### Vello as a future compositing layer

Vello is not dead to this project — it is the right tool for richly decorated surfaces like Markdown preview regions where arbitrary vector shapes, gradients, and glyph runs need to compose freely. When that need arrives in Phase 3, a Vello scene can render into an off-screen wgpu texture that is then composited into the main frame as an image. This gives both: atlas-quality text in the editing core, and GPU vector expressiveness for the preview layer.

---

## 4. Technology Stack

| Subsystem | Primary crate(s) | Notes |
|---|---|---|
| **Windowing & event loop** | `winit` | Cross-platform: Windows (DX12/Vulkan), Linux (X11/Wayland), macOS (Metal) |
| **Input, frame scheduling, animation** | *(none — stdlib + `winit`)* | Hand-rolled in `src/input.rs`, `src/frame.rs`, `src/animation.rs`. Frame-state input over winit's event stream, demand-driven redraw, and 0..1 animation weights; no crate earns its keep here |
| **GPU backend** | `wgpu` | Owns the device, queue, swap chain, and all render passes |
| **Path tessellation** | `lyon` | Bézier paths and polygons → triangle meshes for wgpu |
| **Text shaping & layout** | `cosmic-text` | Built on `rustybuzz` (shaping) + `fontdb` (discovery); part of glyphon's stack |
| **Glyph rasterization** | `swash` | Driven directly by Atomos's own rasterizer (`glyph_effects.rs`) since Phase 0 — enables faux-bold/condense and color emoji that glyphon's built-in path can't do |
| **Text rendering** | `glyphon` | Provides the atlas + GPU pipeline (via its custom-glyph extension point); shaping stays cosmic-text, rasterization is already custom — full replacement remains an option if atlas ownership requires it |
| **Rope / text storage** | `ropey` | The buffer's spine; all edits go through it |
| **Incremental parsing** | `tree-sitter` + `tree-sitter-highlight` + grammar crates | Syntax highlighting, folding, structural edits; incremental re-parse on edit |
| **Search / replace** | `regex` | `fancy-regex` only if lookahead/backreferences are needed |
| **Scripting** | `mlua` (LuaJIT or Lua 5.4) | Embedded Lua with async support and sandboxing; the command registry and all config live here |
| **Scripting (future)** | `piccolo` | Pure-Rust, fuel-metered Lua VM; consider if hard sandboxing or deterministic plugin execution becomes a requirement |
| **Fuzzy matching** | `nucleo` | Helix's matcher; powers the command palette and all pickers |
| **Markdown parsing** | `comrak` or `markdown-rs` | AST with source spans — critical for round-tripping preview regions to buffer offsets |
| **Terminal backend** | `alacritty_terminal` + `portable-pty` | Same lineage as Zed's terminal; `vte` if custom escape-sequence parsing is needed |
| **Date / time** | `jiff` | Timezone-aware; preferred over `chrono` |
| **Calendar interop** | `icalendar` | `.ics` read/write; `orgize` if org-mode syntax is wanted |
| **File watching** | `notify` | Buffer dirty-tracking, config reload |
| **Clipboard** | `arboard` | Cross-platform; winit covers some paths |
| **Configuration** | **Lua itself** | The Emacs move — config is indistinguishable from scripting; no separate serialization layer |
| **LSP (Phase 4)** | `lsp-types` + `async-lsp` | Language server integration deferred until the plugin API is stable |
| **WASM plugins (optional)** | `wasmtime` + `wit-bindgen` | Sandboxed native-speed extensions alongside Lua; decide at Phase 4 |
| **Decorative rendering (Phase 3+)** | `vello` | Off-screen vector scenes composited as wgpu textures for Markdown previews and rich decoration |
| **Plumbing** | `anyhow`, `thiserror`, `tracing`, `tracing-subscriber`, `clap` | Standard error handling and observability stack |

---

## 5. Phases

Each phase lists its **objective**, **why it matters**, **scope**, **key crates**, **architecture notes**, **exit criteria**, and **risks**. The exit criteria are the contract: a phase is not done until all of them pass.

---

### Phase 0 — Renderer Foundation ✅ **COMPLETE** (August 2026)

**Objective.** Build the minimal wgpu renderer — window, event loop, text, and a colored rect — and prove the full stack compiles and runs correctly, before any editor logic is written on top of it. *(Development happened on Linux/Pop!_OS rather than the originally-assumed Windows target; wgpu's Vulkan backend made this a non-event.)*

**What was actually delivered** — substantially past the original scope. The renderer that exists in `src/renderer/`:

- **Layer architecture.** The core abstraction is a retained [`Layer`], not bare draw calls: each layer owns an offscreen texture and a queue of draw commands, rebuilt only when content changes (`Automatic` layers hash-diff their content per frame; `Manual` layers persist until `clear()`), then composited bottom-to-top. Layer order doubles as z-order — an app puts chrome above content by putting it on a higher layer.
- **Draw API.** `draw_rectangle` (per-corner rounding), `draw_polygon`, `draw_circle`, `draw_path`/`draw_path_from_file` (full SVG `d` grammar including arcs; fill with nonzero/evenodd rules, stroke with width/cap/join/miter, or both), `draw_svg_icon` (viewBox-aware icon rendering — handles nonstandard boxes like Material Symbols' `0 -960 960 960`), `draw_image`/`draw_image_from_file`, `draw_text`, `draw_styled_text` (multi-span rich text, one shape pass per call), plus `get_text_size`/`get_styled_text_size` for caller-side layout.
- **Text stack.** cosmic-text shaping (full Unicode/BiDi/complex scripts, system-font fallback) + a custom swash rasterizer providing continuous faux-bold/faux-condense on any static font, tracking with professional half-track centering, and color emoji (bitmap + COLR). Shaped-buffer caching keyed by shaping inputs; per-layer skip-prepare when text is unchanged; periodic atlas trim bounding memory.
- **DPI scaling.** Public API is logical pixels everywhere; internals are physical pixels — text and paths rasterize/tessellate at native display density. `ScaleFactorChanged` handled live.
- **Shader effects.** `Layer::set_effect`: built-in Gaussian blur and 4×5 color-matrix (grayscale/invert/sepia/saturate/brightness/contrast/tint presets), plus a custom-WGSL hook compiled against a fixed contract, error-scope-guarded so bad shader source degrades gracefully instead of panicking.
- **Quality/perf plumbing.** Adaptive MSAA (8x where the adapter grants `TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES`, 4x floor everywhere else); one shared MSAA scratch texture across layers; non-blocking GPU frame timing (`get_gpu_frametime`) split from wall-clock animation timing (`get_frametime`); u32 index buffers; reused tessellators.
- **Verification practice.** Compile-check alone proved insufficient for wgpu code (two real validation-error bugs shipped past `cargo check`/`clippy` and were only caught at runtime) — every renderer change now gets a bounded `cargo run` smoke test before it's called done.

**Not built, deliberately deferred:** dash patterns for strokes (needs a hand-rolled path-walk decomposition; `lyon_algorithms::walk` is the primitive when needed), per-primitive shaders within a shared batch (effects apply per-layer — the compositor-style unit; give a shape its own layer to shade it), image-upload caching for large/frequently-redrawn images (fine for icons, revisit when big images appear), and eviction for the effect-glyph bitmap cache (unbounded; a later problem at real-document scale).

**Why it matters.** The renderer is the highest-uncertainty, highest-rework-cost part of the project. Every subsequent phase assumes a working draw layer. A rendering dead-end discovered in Phase 0 costs days; discovered in Phase 3 it costs months. This phase also forces the wgpu boilerplate to be written and wrapped once, so it is never seen again.

The rendering decision is already made (Section 3), so Phase 0 is not a survey — it is an *implementation* spike with a clear target. Its only job is to retire risk and produce the `Renderer` struct that the rest of the project draws against.

**Scope / deliverables.**

- `winit` event loop with correct resize and close handling.
- `wgpu` device, queue, surface, and swap chain set up and wrapped behind a `Renderer` struct.
- A solid-color rect pipeline (two triangles, one uniform buffer) — the simplest possible proof that wgpu is working.
- `glyphon` wired in; `draw_text` renders a string from a `ropey` buffer at a given position with correct glyph metrics.
- A `lyon`-tessellated rounded rectangle drawn in the same frame as the text — proves both pipelines compose in one render pass.
- Resize is handled correctly: surface reconfigured, glyphon viewport updated, no glitches.
- The `Renderer` public API is defined: `draw_rect`, `draw_text`, `draw_path`, `draw_image`, `flush`. All wgpu types are private to the renderer module.

**Key crates.** `winit`, `wgpu`, `glyphon` (`cosmic-text`, `swash`), `lyon`, `ropey`, `pollster`.

**Architecture notes.** Keep spike code disposable up until the `Renderer` API crystallises — then that API is permanent. Do not expose wgpu types across the module boundary. The `Renderer` is the only thing the rest of the codebase ever calls; if a future phase needs a new draw primitive, it is added to `Renderer`'s interface, not worked around. Write the wgpu boilerplate (device setup, swap chain, render pass structure) once inside this module, then never look at it again.

**Exit criteria.** All met (with two noted amendments to the original wording):
- [x] A winit window opens and renders without errors *(on Linux — see objective note)*.
- [x] A colored rounded rectangle is drawn via `lyon` tessellation through a wgpu pipeline.
- [x] Text renders with correct glyph metrics and crisp AA *(grayscale coverage AA via the custom swash path, not glyphon subpixel — see Section 3's revised subpixel decision; the `ropey`-sourced-string detail moved to Phase 1, where the rope actually arrives)*.
- [x] Rect and text appear in the same frame with no z-ordering artifacts *(and cross-layer z-order is now an explicit, controlled thing)*.
- [x] Window resize (and DPI scale-factor change) handled correctly with no blank frames or panics.
- [x] All wgpu types are private to the renderer module; callers only see `Renderer`/`Layer` and vocabulary types.
- [x] Frame time is measured (wall-clock + optional GPU-timestamp) and displayed; vsync-capped 60fps holds with thousands of styled-text draw calls after the skip-prepare caching work.

**Validation beyond the criteria:** a full editor-UI mockup (tab bar, file tree with SVG icons, syntax-highlighted code view with relative line numbers and a block cursor, status bar — a Neovide screenshot recreation) was built entirely against the `Layer` API as a throwaway, and looked native-crisp. The API held up without needing renderer changes beyond `draw_svg_icon`. The mockup was deleted after review; `src/main.rs` is back to a minimal window + renderer + event-loop shell, and `src/bin/input_demo.rs` is the live capability demo.

**Risks.** *(Retrospective: version-lock was real but managed by pinning wgpu to glyphon's version; the actual recurring hazard was wgpu **runtime validation errors invisible to the type system** — hence the smoke-test practice above.)*

---

### Platform Layer — Input, Frame Scheduling, Animation ✅ **COMPLETE** (August 2026)

Built between Phase 0 and Phase 1 rather than as a numbered phase: these are the OS-facing systems that aren't rendering and aren't editor logic, and every one of them is cheaper to establish before there is a buffer than to retrofit around one. No new dependencies — all three are stdlib plus what `winit` already exposes.

**Input** (`src/input.rs`). Frame-based state over winit's event stream, because almost every caller wants to ask *state* questions ("is Ctrl held?", "where is the mouse?") at an arbitrary point in a frame rather than react to a one-shot event. Level-triggered (`is_key_down`) and edge-triggered (`is_key_pressed`) queries are distinct, as is `is_key_typed` — which includes OS auto-repeat, and is the correct question for held-key actions like backspace. Physical `KeyCode` identity (layout-independent, right for keybinds) is deliberately separated from `text()` (layout- and modifier-resolved, right for typing); a French layout's `KeyCode::KeyQ` types "a", and only one of those two answers is ever the one wanted. Also: exact-modifier chord matching (`Ctrl+S` must not fire on `Ctrl+Shift+S`), mouse position in logical pixels (directly comparable against drawn geometry, no conversion at the call site), cursor-space and raw device deltas, scroll normalized to lines across both wheel and touchpad units, and focus/cursor-presence flags.

*Stuck-input safety* is the one hard guarantee: losing focus mid-chord is the classic way input state desyncs, since the OS delivers the press but the release goes to whoever has focus next. `Focused(false)` force-releases every held key, button, and modifier, reporting each as a release edge so press/release pairing unwinds instead of hanging. Cursor exit and scale-factor changes drop the delta anchor so re-entry reports a teleport as zero movement rather than one enormous drag.

**Frame scheduling** (`src/frame.rs`). Frames are demand-driven. The naive loop — `ControlFlow::Poll` plus an unconditional `request_redraw()` — renders at display refresh rate forever whether or not anything changed; with vsync that isn't a runaway spin, but it is a full CPU frame and GPU composite every vblank for a window showing a static buffer, and an editor is idle almost all of the time. `FrameScheduler` inverts the default: sleep on `ControlFlow::Wait` until something asks. Input that changed state asks (which is why `Input::handle_event` returns a bool rather than `()`). A running animation asks, every frame, for as long as it runs. Resize and rescale ask explicitly, since neither flows through input. **Measured: idle CPU drops to 0.0%.**

"Dirty" and "animating" are the same concept here — animating is just asking again every frame — so the only thing that differs is pacing, which is why the scheduler owns a target-FPS knob (`WaitUntil`) rather than a separate animation clock. The knob is what makes deliberate sub-display-rate operation possible later (30 fps on battery, say); `None` follows the display and lets vsync do the pacing.

**Animation** (`src/animation.rs`). A timer producing a 0..1 weight over wall-clock time — the primitive everything else interpolates against. Two ownership patterns, deliberately: **caller-owned** `Animation` values are the default (a cursor's blink belongs to the cursor), with an **opt-in keyed registry** (`Animations`) for animations that must be addressable from code that doesn't own them — the shape Lua plugins will need in Phase 2, where a string key crosses the FFI boundary and a Rust reference does not. Easing covers the CSS named curves plus arbitrary `cubic-bezier`, sharing one Newton-solved implementation rather than polynomial lookalikes. `Repeat::{Once, Loop, PingPong}`.

**Architecture notes.** Three ordering/lifetime decisions that are load-bearing and non-obvious:
- `advance()` returns *whether the animation still wants frames*. That return value is the scheduler's signal; it is the entire coupling between the two systems, and it is why neither module depends on the other's types.
- `begin_frame()` is called at the **top** of `RedrawRequested`, not the bottom. Clearing the pending request at the end of a frame swallows the "still animating" request the frame itself just made, and the animation stalls after exactly one frame.
- The delta fed to `advance()` **must be clamped** (`FrameScheduler::animation_delta`, 100 ms). Under `Wait` the gap between frames is unbounded, so a window left alone for a minute reports a 60-second frame delta on the next keystroke; stepping animations by that teleports every one of them to its end state, and presents as "animations randomly don't play."
- Finished animations are never auto-evicted from the registry — `weight()` on a completed fade must keep returning 1.0, or the element it drives snaps back to invisible on the very frame the fade lands. Keys (not generational IDs) are what keeps the set bounded: replaying a key replaces rather than accumulates.

**Testing.** Unlike the renderer, all three are pure state machines with no GPU dependency, so they are covered by ordinary unit tests (21 at time of writing) rather than smoke runs — including the stuck-key guarantee, the request-survives-the-frame ordering, and the idle-gap clamp. The renderer's smoke-test practice still applies to anything that touches wgpu.

**Deferred:** IME (see `TODO.md` — needs the rope and caret to attach preedit to); keyframes, springs, and sequenced timelines (all build on the 0..1 weight, none need to exist for it to be useful); per-window scheduling, which becomes a question only if multiple windows land.

---

### Phase 1 — The Buffer Engine

**Objective.** A rock-solid, feature-free editing core with a clean internal API that the rest of the project is built on.

**Why it matters.** Everything is a view over a buffer. If the buffer engine has a design flaw — in the undo model, the cursor representation, or the transaction API — every feature built on top inherits it. This phase has no user-facing features by design; its output is purely internal API.

**Scope / deliverables.**
- Rope-backed buffers via `ropey`: insertion, deletion, range replacement.
- Cursor and selection model. Design explicitly for multiple cursors even if only one is active — retrofitting the model later is expensive.
- Undo/redo via a **transaction log** (not ad-hoc snapshots). Every mutation produces a typed transaction; nothing bypasses this path.
- Multiple buffers, buffer lifecycle (open, close, dirty tracking, unnamed buffers).
- Viewport math: line/column ↔ pixel coordinate conversion, scroll offset, visible range — decoupled from buffer storage.
- `tree-sitter` integration: incremental parse on edit, highlight spans emitted per visible range.
- Change-notification mechanism so views can subscribe to buffer mutations without polling.

**Key crates.** `ropey`, `tree-sitter` + `tree-sitter-highlight` + grammar crates, `regex`.

**Architecture notes.** The transaction type is the spine of the whole project — design it to carry enough information for undo, change notifications, and eventual CRDT collaboration. `enum Transaction { Insert { pos, text }, Delete { range }, Composite(Vec<Transaction>) }` is a reasonable starting shape. Highlighting must be incremental — only re-parse affected ranges, never the whole buffer. Keep viewport state out of buffer storage; it belongs to the view layer.

**Exit criteria.**
- [ ] Arbitrary edits, undo, and redo are correct under property/fuzz testing with `proptest` or `cargo-fuzz`.
- [ ] Two buffers can be open, switched between, and closed without state leakage.
- [ ] Syntax highlighting updates incrementally on edit across at least two languages (e.g. Rust and Markdown).
- [ ] No code path mutates the rope outside a transaction.
- [ ] The cursor model supports multiple cursors in its data representation, even if the UI only shows one.
- [ ] Viewport coordinate math correctly handles variable-width glyphs and Unicode.

**Risks.** Designing undo as snapshots (easy to reach for, expensive to fix later). Coupling viewport state into buffer storage. Skipping the multiple-cursor design on the grounds of "I'll add it later."

---

### Phase 2 — The Command & Scripting Layer

**Objective.** Embed Lua, expose the editor API through it, and route all commands — built-in and user-defined — through a single live, hot-reloadable registry. This phase is the project's thesis made real.

**Why it matters.** This is the make-or-break phase. If commands are introspectable, redefinable, and hot-reloadable — and built-ins use the same path as plugins — then the Emacs bet is validated. If any built-in takes a shortcut that plugins cannot, the architecture has already failed its core promise.

**Scope / deliverables.**
- `mlua` embedded with a stable, narrow Rust↔Lua boundary.
- Buffer and editor API exposed to Lua: read/modify buffers via transactions, move cursors, query state, register commands, bind keys, subscribe to events.
- A **single command registry** shared by built-ins and Lua plugins; no privileged registration path.
- A keymap system supporting layered and contextual bindings (normal mode, insert mode, palette, etc.).
- Hot reload: redefining a command or rebinding a key from a running Lua evaluation takes effect immediately.
- The `nucleo`-powered **command palette** as the first real consumer of the registry — if the registry is introspectable enough to drive the palette, it is introspectable enough for plugins.
- A scratch buffer from which arbitrary Lua can be evaluated against the live editor state.
- Config system: the init file is a Lua script loaded at startup, indistinguishable from a plugin.

**Key crates.** `mlua`, `nucleo`.

**Architecture notes.** Built-in commands must register through the *same* Lua-visible registry path as plugins — no back-door. Keep the Rust↔Lua boundary transaction-oriented and narrow: Lua submits typed intents (`editor.insert("text")`, `editor.move_cursor("next-word")`), Rust validates and applies them. Never expose raw rope mutation to Lua — not even as an "escape hatch." The moment a raw mutation leaks through, determinism and undo correctness are compromised.

**Exit criteria.**
- [ ] A command defined entirely in the scratch buffer (no Rust changes) can be invoked from the palette.
- [ ] An existing built-in command can be wrapped or replaced at runtime; the new behavior takes effect immediately.
- [ ] A keybinding can be changed live from the scratch buffer.
- [ ] Every registered command is visible and searchable in the palette, including user-defined ones.
- [ ] The init file is loaded at startup as plain Lua; the editor is fully configurable without touching Rust.
- [ ] A Lua error in a plugin does not crash the editor — it is caught, logged, and reported.

**Risks.** Lua owning buffer state or calling raw mutations (performance + determinism trap). A wide, leaky Rust↔Lua API that becomes impossible to stabilize later. Designing the keymap system too narrowly for modal editing (the next phase will stress it hard).

---

### Phase 3 — Built-ins as Plugins

**Objective.** Implement each shipped feature *entirely through the Phase 2 API*, in an order designed so each one stresses a different architectural dimension.

**Why it matters.** Four real features built against the public API will batter it into shape faster than any amount of upfront design. Each feature is simultaneously a deliverable and a stress test. If any feature needs a core change, that is an API gap — fix the API, not the feature.

**Build order and what each one proves.**

1. **Modal editing.** Proves the keymap layer is expressive enough for a complete input model: modes, operators, motion-counts, pending operator states. If Vim-style editing can be implemented purely in Lua against the keymap API, the keymap API is sufficient for anything.

2. **Markdown editor with inline previews.** Proves the hypertext-rendering thesis and the span-mapping between rendered preview regions and buffer offsets. Uses `comrak`/`markdown-rs` for AST with source spans. This is the feature most likely to expose gaps in both the render API and the buffer change-notification system — front-load it within Phase 3.

3. **Terminal emulator.** Proves async I/O and a non-text-buffer view coexist with the buffer/transaction model. Uses `alacritty_terminal` + `portable-pty`. If the terminal can be a first-class view without special-casing the event loop, the view architecture is general.

4. **Calendar / to-do / organization views.** Proves structured, non-editor views (date grids, task lists, agenda) can be built from the same plugin primitives. Uses `jiff` + `icalendar`, optionally `orgize` for org-mode syntax.

**Architecture notes.** Decorations, overlays, and preview regions are *described* by the plugin and *drawn* by the renderer — the render boundary from Section 2 holds throughout. If the Markdown preview needs richer vector compositing than the base renderer provides, this is the phase where Vello is introduced as an off-screen compositing layer (a Vello scene → wgpu texture → composited as an image in the main frame). That integration should not require changes to the base `Renderer` interface.

**Exit criteria.**
- [ ] Each feature is implemented entirely through the public command/view API with no privileged hooks in core Rust.
- [ ] Disabling any single feature leaves a working plain editor with no errors.
- [ ] Markdown preview regions correctly map back to source buffer positions after arbitrary edits.
- [ ] The terminal runs a real interactive shell session with correct async I/O and window-resize (SIGWINCH) handling.
- [ ] At least one feature was added or modified mid-phase without changing any core Rust code.
- [ ] If Vello is introduced for Markdown previews, it composites correctly without affecting frame timing of the base editor.

**Risks.** Special-casing a feature into core "just this once" — treat every needed core change as evidence of an API gap and fix the API instead. Markdown span-mapping proving more complex than expected — front-load it. The terminal's async PTY I/O conflicting with the winit event loop — research the integration pattern before starting feature 3.

---

### Phase 4 — Ecosystem & Polish

**Objective.** Make the editor a platform other people can extend, theme, and rely on.

**Why it matters.** The public Lua API must not be stabilized until Phase 3 has stress-tested it through four real features. This phase turns a working program into something with a compatibility contract.

**Scope / deliverables.**
- Plugin packaging and distribution: discovery, install, versioning, a plugin manifest format.
- Theming system: colors, typography scales, decoration styles — all describable in Lua config.
- LSP integration (`lsp-types` + `async-lsp`): completion, diagnostics, go-to-definition, hover.
- Documentation: the public Lua API, the command model, the rendering boundary, and a "build your first plugin" guide.
- API stabilization and a versioning/deprecation policy — a written compatibility contract for plugin authors.
- Optional: WASM plugin track (`wasmtime` + `wit-bindgen`) for sandboxed, native-speed extensions alongside Lua.

**Architecture notes.** Stabilize the API only after Phase 3's four features have battered it. Document the command model first — it is the single most important thing for plugin author adoption. LSP is deliberately deferred here because LSP clients are themselves complex plugins; they belong after the plugin API is solid.

**Exit criteria.**
- [ ] A third-party plugin can be installed and loaded without recompiling the editor.
- [ ] The public Lua API is fully documented and carries a versioning policy.
- [ ] LSP-backed completion and inline diagnostics work in at least two languages.
- [ ] A complete custom theme (colors, fonts, decoration) can be authored and applied in pure Lua.
- [ ] The plugin distribution system can resolve and install dependencies.

**Risks.** Stabilizing the API too early before Phase 3 has exposed its gaps. Under-documenting the command model. LSP complexity bleeding into the core before the plugin boundary is stable.

---

## 6. Cross-Cutting Concerns

**Performance boundary.** Lua decides intent; Rust owns rope edits, layout, and rendering. `mlua`/LuaJIT is fast enough for event handling and command dispatch, but must never sit in the per-glyph or per-frame hot path. Profile before optimizing, but design the boundary so the optimization is possible without an API break.

**Error handling.** `anyhow` at application boundaries, `thiserror` for typed errors in library-like core code. Plugin errors (Lua panics, failed commands) must be caught at the registry level, logged via `tracing`, and reported to the user without crashing the editor.

**Testing strategy.** Property and fuzz testing for the buffer engine and transaction log — this is where subtle correctness bugs hide and where they are cheapest to find. Integration tests that drive commands through the registry exactly as the palette does (not by calling internal functions directly). Golden-image or span-mapping tests for the Markdown view. The terminal requires PTY integration tests.

**Observability.** `tracing` + `tracing-subscriber` throughout. A command execution log is useful both for debugging and as the foundation of a future macro/replay system.

**Frame budget.** Frames are demand-driven (see the Platform Layer section), which makes "does this need a redraw?" a question every feature has to answer rather than something the loop papers over. Anything that changes what is on screen must reach `FrameScheduler::request_redraw`, and anything that changes it *over time* should do so by returning `true` from an `Animation::advance` rather than by unconditionally requesting frames — an unconditional request re-introduces the always-drawing default one feature at a time. Time-based state that isn't animation (a status-bar clock, a blinking caret between blinks) is a timer that requests a redraw, not a reason to poll.

---

## 7. Risk Register

| Risk | Phase | Mitigation |
|---|---|---|
| wgpu ↔ glyphon version lock causing type mismatch errors | 0 | Pin versions together; use glyphon's re-exported wgpu types rather than a separate wgpu dependency |
| wgpu boilerplate taking longer than expected | 0 | Time-box to one week; use `learn-wgpu` as the reference; wrap and move on |
| Undo designed as snapshots instead of a transaction log | 1 | Enforce the transaction type from day one; reject any snapshot-style shortcut in review |
| Multiple-cursor model deferred and then expensive to retrofit | 1 | Design the cursor model for multiple cursors in Phase 1 even if only one is active |
| Lua owning buffer state or in the per-frame hot path | 2 | Narrow, transaction-oriented boundary; Rust owns the rope; profile early |
| A feature unconditionally requesting redraws, quietly restoring the always-drawing default | 1–3 | Redraw requests come from input-state changes and `Animation::advance` returning `true`; treat any unconditional per-frame request as a bug |
| Leaky Rust↔Lua API stabilized before Phase 3 batters it | 2–3 | Explicitly defer stabilization; treat Phase 3 feature needs as API revision signals |
| Feature special-cased into core | 3 | Every needed core change is an API-gap bug; fix the API, not the feature |
| Markdown span-mapping more complex than expected | 3 | Front-load it as the first Phase 3 feature; use `comrak`/`markdown-rs` with source spans |
| Terminal PTY async I/O conflicting with winit event loop | 3 | Research the integration pattern before starting the terminal feature |
| Vello compositing introduction breaking frame timing | 3 | Off-screen texture compositing; Vello renders into a wgpu texture, not directly into the swap chain |
| Public API stabilized too early before ecosystem phase | 4 | Written policy: no stability guarantees until all four Phase 3 features pass their exit criteria |

---

## 8. Open Questions

These are unresolved. Revisit each one at the phase it becomes blocking.

- **LuaJIT vs Lua 5.4 vs `piccolo`.** LuaJIT is faster; Lua 5.4 is more portable and has a cleaner integer model; `piccolo` gives hard sandboxing and fuel metering. Decision needed before Phase 2 begins.
- **Multiple cursors: Phase 1 data model only, or Phase 2 activation?** The model must exist by end of Phase 1. Whether the UI exposes it is a Phase 2 decision.
- **Static config layer.** Lua-only config is the right long-term answer, but a small static TOML/KDL layer for first-run defaults (font path, colorscheme) might be pragmatic. Decide before Phase 2.
- **CRDT compatibility.** Is the transaction log designed to become CRDT-compatible for real-time collaboration later, or is collaboration explicitly out of scope for v1? Decide before Phase 1's transaction type is finalized.
- **WASM plugin track.** Ship alongside Lua at Phase 4, or Lua-only for v1 with WASM deferred to v2?
- **Vello for Markdown previews.** Confirmed as the direction in Section 3; the open question is whether Vello's API is stable enough by Phase 3, or whether a simpler CPU-rendered preview is the pragmatic fallback.

---

*Living document. Section 3 is settled. Update the open questions table as decisions are made, and revise exit criteria when reality teaches you better ones.*
