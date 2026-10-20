# Flexbox layout for Atomos

Status: **proposal, awaiting owner decisions (section 8)**. Nothing is implemented. Date: 2026-10-20.

Goal: declare elements the way you declare `div`s (size, padding, gap, grow, align, position) and get their rects computed, then draw, clip and hit-test from the same rects. Top principles: performance, low memory, then developer experience.

Anything marked **[estimate]** was not measured.

---

## 1. What Typewritter does today

- `src/layout.rs` (618 lines) is a purpose-built row/column solver: `Size::Fixed(px)` or `Size::Flex(weight)`, `gap`, `visible`, and a pushed-in `min_size` that propagates up so the window can refuse to shrink below the shell's floor. It re-solves only when a style changed by value or the viewport moved, and snaps each child's two edges so neighbours share an edge exactly.
- The shell is about 15 nodes (`shell/mod.rs:695`, `graph_editor.rs:191`). Each node hosts one `Region`: one Atomos `Layer` in `Manual` invalidation, scissored to the node's rect, redrawn only when the component is dirty or its rect moved.
- Everything *inside* a region is hand-placed: `title_bar.rs` computes `sidebar_rect` as `bar.x + 188.0`, `focus_rect` from the theme switch's rect minus 94, the search box from both, then clamps. The same free functions feed drawing and input hit-testing, so they cannot drift, but every component re-derives its own arithmetic.
- Typewritter's own `src/renderer` is the pre-extraction vendored fork. Layout only touches `Layer` (draw calls, `get_text_size`, `set_clip_rect`), which Atomos has.

So the missing piece is not the shell skeleton; it is a general solver for *component internals* (toolbars, menus, dialogs, rows), with text that wraps.

## 2. Taffy or custom

| | Wrap `taffy` | Write our own |
|---|---|---|
| Correctness | Passes the CSS flexbox test suite (wrap, shrink, min/max, aspect ratio, baselines, align-content, absolute inset, percent). Used by Bevy UI, Dioxus, Zed's GPUI. | Each of those is a corner case we would re-discover. A correct subset is ~1.5-2k lines plus tests. |
| Memory | Per-node style, two layouts and a 9-entry cache **[estimate: several hundred bytes to ~1 KB per node, to be measured with the chosen features]**. At hundreds of nodes that is irrelevant; at tens of thousands (a virtualised list gone wrong) it is not, see section 7. | Whatever we choose. |
| Speed | Per-node cache; a subtree whose inputs did not change is not re-solved. | We would build the same cache. |
| Cost | One dependency (`slotmap`; `smallvec` only with `grid`). | Maintenance. |

**Recommendation: use taffy, hidden behind an Atomos-owned API.** Pin an exact version (0.15.0 today), `default-features = false, features = ["std", "taffy_tree", "flexbox"]`. Block, float, grid, calc, content_size and detailed_layout_info are all off. A cargo feature `grid` can turn grid on later.

Because the public types are ours (section 3), a profile that shows taffy's memory or time as the bottleneck can be answered by swapping the backend (taffy's low-level `LayoutPartialTree` traits over our own compact storage) without breaking callers.

## 3. API

Two layers, so the fast path is not tied to the pleasant one.

**3a. Retained tree (the core).** What Typewritter's `Layout` is today, generalised.

```rust
let mut ui = Layout::new();
let root = ui.root();
let bar = ui.add(root, Style::row().height(px(60.0)).items_center().gap(12.0).padding_x(20.0));
let search = ui.add(bar, Style::new().grow(1.0).max_width(px(340.0)).min_width(px(120.0)));
ui.set_style(search, |s| s.max_width(px(280.0)));   // no-op if unchanged
ui.compute(viewport, &measurer);                    // returns true only if it solved
let r: Rect = ui.rect(search);                      // absolute, logical px, pixel-snapped
```

- `Style` is an Atomos type with builder methods and CSS names: `width/height`, `min_*/max_*`, `padding`, `margin`, `gap`, `grow/shrink/basis`, `wrap`, `align_items/self/content`, `justify`, `position_absolute` + `inset`, `aspect_ratio`, `overflow`, `hidden`. It converts to `taffy::Style` internally; callers never name taffy.
- `set_style` compares by value first and only marks dirty on a real change, so an animation writing the same width every frame costs a comparison (Typewritter's rule, kept).
- `compute` is a no-op unless a style changed, a text leaf's content changed, or the viewport moved.
- `min_content(node)` gives what a subtree needs (the window floor), see section 6.

**3b. Declarative builder (sugar, phase 2).** Describe the tree each time a component rebuilds; it is reconciled by position (and optional key) against the retained tree, so unchanged nodes keep their taffy caches and no allocation happens in the steady state.

```rust
// Illustrative title bar. Replaces sidebar_rect/focus_rect/search_box_rect.
fn view(&self, ui: &mut Ui) {
    ui.row(Style::new().height(px(60.0)).items_center().padding_x(20.0).gap(12.0), |ui| {
        ui.leaf(Id::MARK,    Style::new().size(30.0, 30.0));
        ui.leaf(Id::SIDEBAR, Style::new().size(34.0, 34.0).margin_left(138.0));
        ui.leaf(Id::SEARCH,  Style::new().grow(1.0).max_width(px(340.0)).height(px(36.0)));
        ui.leaf(Id::FOCUS,   Style::new().size(80.0, 34.0));
        ui.leaf(Id::SWITCH,  Style::new().size(switch::W, switch::H));
    });
}
// draw and input read the same rects:
let search = ui.rect(Id::SEARCH);           // draw
if ui.hit(point) == Some(Id::SEARCH) { .. } // click
```

(Typewritter centres the search box on the *bar* and clamps against neighbours; flex centres it between siblings, so the real port would use auto margins or an absolute child for that one. That is a behaviour detail to settle at port time, not an API gap.)

`Id` is a small user-defined tag (`u32`) stored per node. It exists for hit-testing and for finding a node after a rebuild, not for reconciliation by pointer.

## 4. Drawing and clipping

Layout stays **geometry only**, as Typewritter already insists. Paint is an optional side table:

- `ui.set_paint(node, Paint { background, border, radius, clip })`, stored in a sparse `Vec<Option<Paint>>`. Nodes with no visuals cost one `None`.
- `ui.paint(&layer)` walks the tree depth-first in order and calls the existing `Layer::draw_rectangle` / `with_clip`. Text leaves call `draw_text` / `draw_wrapped_text` at their rect. Nothing in the renderer changes, and `Renderer` / wgpu types stay private as the architecture invariants require.
- `overflow: hidden` becomes `Layer::with_clip(ClipShape::Rectangle)`. An unrounded rect on whole physical pixels is a free hardware scissor, which is why pixel snapping below matters.
- Callers who want full control skip `paint` and just read `rect()`; the Typewritter components keep their own `draw`.

**Pixel snapping.** Taffy's built-in rounding snaps to whole *logical* units. Atomos draws in logical pixels but scissors in physical ones, so I turn taffy's rounding **off** and snap in our own pass: round each node's absolute near and far edges to the `1/scale_factor` grid (so adjacent siblings still share an edge exactly, as in Typewritter) and derive width from the difference. That pass already walks the tree to detect moved rects (section 5), so it adds no extra traversal.

## 5. Invalidation and performance

1. **Solve gating.** A dirty flag (set by `add`, `remove`, a *changed* style, a changed text leaf, or a viewport change) in front of `compute`; a static UI costs one branch per frame.
2. **Taffy's per-node cache** absorbs repeat queries inside one solve (it probes min-content, max-content and definite sizes) and keeps unchanged subtrees warm across solves. Dirtying a node dirties only its ancestors.
3. **Moved-rect report.** After a solve we diff each node's snapped rect against the previous one (16 bytes per node) and expose `ui.moved(node) -> bool` plus a count. That is exactly Typewritter's second redraw condition ("the layout moved or resized its rect"), so a `Manual` layer redraws only on a real change.
4. **No steady-state allocation.** Scratch `Vec`s live on the `Layout` and are reused; the reconcile pass reuses nodes in place.
5. **Perf budget to verify before claiming anything:** `compute` on 200 nodes with every text leaf dirty, and on a one-leaf change, with numbers reported the same way the renderer benches are (`cargo test --release ... --ignored`).

## 6. Text measurement and wrapping

Text leaves are the one place layout needs the renderer. Layout takes a `&dyn Measurer` (implemented by `Layer`) rather than owning fonts, so the solver stays testable with a fake.

- Leaf context: `Text { text, font, params, wrap }`, stored in a side table keyed by node.
- Taffy asks a leaf for its size given a width constraint. **Definite width** maps to `get_wrapped_text_size`; **max-content** maps to the unwrapped `get_text_size`.
- **Min-content is a gap in Atomos.** CSS min-content is the *longest unbreakable word*, which is what stops a flex item shrinking below a word. Today, a very narrow `get_wrapped_text_size` breaks *between characters* (cosmic-text's default `WordOrGlyph`), so it cannot answer that. Needed primitive: `Layer::get_min_content_width(text, font, params)`, implemented by shaping once with word-only wrapping at width 0 and taking the widest line (one extra cached shape per distinct text). This touches `text_stack.rs`, which the other thread (text batching and wrapped styled text) is editing, so I would do it after that merges.
- **Per-width shaping cost.** Atomos caches one shaped buffer per distinct `max_width`, and a window-resize drag produces a new width every frame. Two mitigations: (a) taffy's 9-slot cache already collapses the repeated probes inside a single solve, so shaping happens roughly once per distinct width per solve, not once per probe; (b) measure leaves memoise the *size* (not the buffer) keyed by width quantised to the physical pixel, so near-identical widths reuse the answer. If resize drags still show shaping in a profile, quantise to a coarser step during drags.
- **Styled text** (`draw_styled_text`, `get_styled_text_size`) gets the same leaf kind once the other thread lands wrapped styled text.
- Custom leaves (an image, a canvas, an editor surface) pass a closure instead of text: `leaf_measured(style, |known, avail| size)`.

## 7. Semantics mapping, and what is deliberately not in v1

Typewritter's semantics differ from CSS in ways a naive port would break:

| Typewritter | Atomos style |
|---|---|
| `Size::Fixed(x)` is exact and **not** floored by its minimum (the collapse animation relies on it). | `width(x)`, `shrink(0)`, `min_width(0)`. CSS `min-width: auto` would floor flex items at their content size, so the explicit `0` is required. |
| `Size::Flex(w)` | `grow(w)`, `basis(0)`; the minimum is an explicit `min_size`. |
| Axis defaults to column. | See decision 1. |
| `visible = false` keeps the last rect. | Taffy's `display: none` zeroes the layout. Atomos keeps the *last rect* too (stored in our snapped-rect table), because a hiding panel must not jump while it animates away. |
| `Layout::min_size(ROOT)` is the window floor. | `min_content(root)`: one extra solve with `AvailableSpace::MinContent`, run *before* the real `compute` (a later solve overwrites layouts, and cached entries make the second pass cheap). Only run when dirty. |

Not in v1:

- **Scrolling and virtualisation.** Taffy reports content extent for `overflow: scroll`, but scroll offsets, clip and "only lay out visible rows" are a separate design. Until then a very long list should be laid out by the caller, not as tens of thousands of nodes.
- **Grid.** Cargo feature later.
- **CSS cascade, selectors, classes.** Styles are values on nodes.

## 8. Decisions for April

1. **Default axis of `Style::new()`**: column (a `div` that stacks, matching HTML flow and Typewritter) or row (CSS flexbox default). *Recommend column*, with `Style::row()` for rows.
2. **Grid**: later behind a cargo feature, or never. *Recommend later.*
3. **Declarative reconcile builder (3b)**: phase 2 or skip. *Recommend phase 2*, after the retained core is proven on one Typewritter component.

## 9. Plan

1. `src/layout/` in Atomos: `Style`, `Layout`, retained API, snapping, moved-rect report, `Measurer` trait with a fake, unit tests (shrink/grow, min, gap, absolute, snapping at scale 1.5, hide-keeps-rect, no-op solve).
2. `Layer` implements `Measurer`; text leaf with definite/max-content; `get_min_content_width` after the text-stack thread merges.
3. Paint pass and `hit()`; an `examples/layout_demo.rs`.
4. Benchmarks and a memory-per-node measurement; update this doc's estimates.
5. Phase 2: declarative builder. Then port one Typewritter component (title bar) as the proof; Typewritter stays untouched until you decide.
