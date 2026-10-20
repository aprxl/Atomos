# Painter order, clip scopes, and bounded offscreen groups

Status: **proposal, awaiting owner decisions (section 7)**. Covers audit step 4,
"Decouple clipping/order from offscreen surfaces", in
`Typewritter/docs/audits/atomos-2026-10-20/AUDIT.md`. The relevant audit
sections are *A clip/effect boundary is a full-window allocation* and *Painter
order is not preserved*. Date: 2026-10-20.

`src/renderer/layer.rs` is being edited concurrently, so this document names
functions instead of line numbers. Anything marked **[assumption]** was not
verified, either in this repository or in the glyphon 0.11.0 source. Everything
else was checked by reading the code.

---

## 1. The problems, as the code behaves today

**1.1 Painter order is lost within a layer.**

- `LayerInner::build_buffers` walks `physical_commands()` once:
  - every `DrawCommand::Image` goes to `image_draws`;
  - every other non-text command is tessellated into **one** vertex/index
    buffer.
- `render_to_texture_if_needed` then draws, in this fixed order:
  1. all solid geometry, with one `draw_indexed(0..num_indices)`;
  2. all images;
  3. `TextStack::render`. This is glyphon's single instanced
     `draw(0..4, 0..n)` over everything the layer prepared (verified in glyphon
     `TextRenderer::render`).
- Result: an image followed by a covering rectangle still shows the image, and
  later shapes can never cover text (this is the audit's probe).
- The only workaround today is another layer, which costs another full-window
  texture.

**1.2 Every isolation boundary is a full-window texture.** Each of these is a
`create_layer_texture(.., self.size)` call: RGBA, single-sample, surface-sized.

| Source | Function | Textures |
|---|---|---|
| every layer | `LayerInner::new`, `resize_if_needed` | 1 |
| `set_effect(ColorMatrix \| Custom)` | `rebuild_effect_gpu` | +1 output |
| `set_effect(Blur)` | `rebuild_effect_gpu` | +2 (output, ping) |
| `set_clip_shape` | `rebuild_mask` | +1 (RGBA, though only alpha is read) |

- The shared MSAA scratch (`create_msaa_texture`) is the only memory saving
  already in place.
- `set_clip_rect` is a scissor, but the pass still clears and resolves the
  whole attachment.

**1.3 Every live layer composites fullscreen.**

- `render_layers` calls `composite` for each upgraded `Weak` with the constant
  fullscreen `composite_quad_vbuf`.
- A masked layer adds a second fullscreen sample.
- Nothing is skipped for being empty, scissored out, offscreen, or occluded.
- `Renderer::render` also runs a separate clear pass first.

**1.4 What must be kept:**

- logical-pixel commands (`to_physical` runs at build time);
- retained clean layers (`content_texture_dirty`/`effect_texture_dirty`,
  commits fb1a031 and 9c81175);
- uniform patching in `update_effect_uniforms`;
- a separate `TextRenderer` per draw unit (the reason is in
  `TextStack::create_text_renderer`);
- the blend contract: content `ALPHA_BLENDING` (straight in, premultiplied
  out), composite `PREMULTIPLIED_ALPHA_BLENDING`.

---

## 2. Proposed model

Today the layer and its texture do three jobs at once. The proposal splits them
apart:

| Concept | Role | GPU cost |
|---|---|---|
| **Layer** | Z-ordered unit of retention and invalidation (`Automatic`/`Manual`). Owns logical commands and the GPU batches built from them. | Geometry buffers, image textures, `TextRenderer`s. **No colour target by default.** |
| **Clip scope** | Nested clip over a run of commands. A rectangle becomes a scissor; any other shape becomes stencil. | No surfaces. One shared stencil attachment. |
| **Offscreen group** | A layer whose pixels must be isolated: it has an effect, an explicit cache policy, or a whole-layer clip applied after an effect. | One texture **bounded** to the group, plus effect scratch. |

### 2.1 Frame structure and invariants

Each frame runs three phases.

1. **Prepare.** CPU work plus queue writes; nothing is recorded yet. For each
   live layer:
   - resize and scale bookkeeping;
   - `rebuild_if_needed` (builds batches);
   - prepare each dirty text batch;
   - compute group bounds;
   - allocate or reuse group textures.
2. **Groups.** For each dirty group, bottom to top, record its content pass into
   size-matched scratch (2.5), then its effect passes. This is today's retained
   path, bounded.
3. **Main pass.** One render pass:
   - colour attachment: the shared surface MSAA texture,
     `Clear(background)`, resolving into the swap-chain view;
   - stencil attachment: shared surface MSAA stencil, `Clear(0)`, `Discard`.

   Layers are visited bottom to top. A **direct** layer replays its batches; a
   **group** draws one bounded composite quad. The clear pass in
   `Renderer::render` is deleted.

Invariants to document in code:

- **I1.** A `TextRenderer` is never prepared after its `render` has been
  recorded in the frame's encoder. glyphon's prepare destroys and recreates the
  vertex buffer when it grows (verified). This forces one renderer per text
  batch (2.2), and it is why today's per-layer prepare-then-record loop in
  `render_layers` cannot simply move inside one pass.
- **I2. Same-frame atlas reset.**
  - The hazard: if a later prepare in phase 1 calls `reset_glyph_atlas`, every
    batch prepared earlier in that phase references glyphs of the discarded
    atlas. That happens through the retry-on-full path or through
    `effect_glyphs.needs_reset()`.
  - Why it is new: today, earlier layers were already recorded against the old
    atlas, so they still drew correctly. In phase 3 they would render with the
    new atlas's bind group and show garbage.
  - The fix:
    1. Snapshot `atlas_generation` before phase 1.
    2. If it changed by the end, run one extra sweep that re-prepares every
       batch prepared before the change.
    3. If a reset happens during the sweep, log and count it (`atlas_resets`).
       Do not loop.
  - Atlas *growth* is different. glyphon re-uploads glyphs at the same packer
    positions (verified in `InnerAtlas::grow`), and `rebind` refreshes the bind
    group. **[Assumption:]** the glyph shader normalises by the current atlas
    size, so earlier-prepared vertices stay valid.

### 2.2 Ordered batches (painter order)

`build_buffers` turns the physical command stream into a flat replay list. A
batch is a run of consecutive compatible commands:

```rust
enum ReplayOp {
    Solid { indices: Range<u32> },   // sub-range of the layer's ONE vertex/index buffer
    Images { draws: Range<usize> },  // range into image_draws (one bind group each, as today)
    Text { batch: usize },           // index into the layer's TextBatch pool
    PushClip { clip: usize }, PopClip,
}
```

- **Solid geometry** keeps one buffer per layer. A batch is just another
  `draw_indexed` range, so the buffer count does not change.
- **Text.** glyphon cannot draw a sub-range: `render` draws everything
  prepared, and its buffer and pipeline are private (verified). So each text
  batch gets its own `TextRenderer`.
  - `LayerInner` keeps a `Vec<TextBatch { renderer, hash, generation }>`,
    reused by index across rebuilds. Surplus entries are dropped after the
    batch count has stayed lower for K rebuilds.
  - `last_text_hash` and `text_prepared_generation` move into `TextBatch`. An
    unchanged batch skips its prepare.
  - `TextStack::prepare_layer` becomes
    `prepare_batch(commands, origin: [f32; 2], renderer)`.
  - The existing reset-and-retry-once logic applies per batch.
- **Fragmentation.** An editor that interleaves line backgrounds with text
  yields 2N batches. Stage 7 adds *bounds-disjoint merging*: a command may
  join the previous batch of its kind if its physical bounds intersect nothing
  drawn in between. Overlapping commands are never reordered. Bounds come from:
  - the existing `bounding_box` and `path_bounding_box` for geometry;
  - the quad, for images;
  - `measure_shaped_buffer_with` plus `apply_alignment` for text.

### 2.3 Clip scopes without surfaces

- **Commands.** Add `DrawCommand::PushClip(ClipShape)` and
  `DrawCommand::PopClip`.
  - Both are stored in logical units and scaled by `to_physical`.
  - Both are hashed, so `Automatic` invalidation sees clip changes.
- **Scissor path.** Used for a `ClipShape::Rectangle` with `Rounding::NONE`
  whose *physical* edges are integer-aligned.
  - Replay keeps a stack of intersected rects and calls `set_scissor_rect`
    before each op, with today's clamp arithmetic.
  - An empty intersection skips every op until the matching pop.
  - A fractional edge goes through stencil instead, so the edge keeps the
    antialiasing that `set_clip_shape` gives it today.
- **Stencil path** for every other shape:
  - The shape is tessellated by `clip_shape_to_command` and
    `tessellate_command`, and appended to the layer's solid buffer as its own
    range.
  - Push at depth d: draw the range with `stencil_incr` (compare `Equal`,
    ref d, `IncrementClamp`, empty colour write mask), then d += 1.
  - Content: all content pipelines use `Equal` and `Keep`, with
    `set_stencil_reference(d)` before each op. Depth 0 over a cleared buffer
    passes everything.
  - Pop: redraw the range with `stencil_decr` (`Equal` at d,
    `DecrementClamp`), then d -= 1.
  - While a push or pop draws, the scissor is set to the parent scissor
    intersected with the shape bounds.
  - Maximum depth is 255. Deeper pushes become no-op clips, counted and logged
    once.
- **Clip-edge antialiasing.** Stencil has the colour attachment's sample count,
  and the test runs per sample, so clip edges get MSAA coverage AA, the same
  quality the mask path has today. **[Assumption]** for every backend; stage 3
  adds an edge-pixel regression test.
- **Text needs no glyphon fork.** `TextRenderer::new` takes
  `Option<DepthStencilState>`, and glyphon caches its pipeline per that state.
  Its `render` sets pipeline, bind groups, and vertex buffer but **not** the
  stencil reference (all verified).
- **Unbalanced streams.**
  - A missing `PopClip` at the end of a layer is implied.
  - A stray `PopClip` is ignored and counted.
  - So a panic inside a scoped closure, or a sloppy `Manual` layer, cannot leak
    clips into later layers.
- **Pipelines.**
  - Every pipeline used in a stencil-attached pass needs a matching
    `depth_stencil`.
  - Recommended (Q4): stencil is always attached, with one set of pipelines
    (solid, image, glyphon, composite, `stencil_incr`/`stencil_decr`), all
    using `Stencil8`.
  - The effect pipelines (`build_fullscreen_pipeline`) stay as they are:
    single-sample, no stencil.
- **Sample count.** `pick_msaa_sample_count` must return a count supported by
  **both** the colour format and the stencil format. Otherwise 8× colour with
  4×-only stencil fails validation when the pass is created. The test harness
  hardcodes `MSAA = 4` and will not catch this, so stage 3 adds a test at the
  adapter-picked count. **[Assumption:]** `Stencil8` costs 1 byte per sample
  and is native. Some backends may emulate it with `Depth24PlusStencil8`, which
  costs 4 bytes per sample.

### 2.4 Direct layers vs. groups

A layer is **direct** unless it has a `ShaderEffect`, its `CachePolicy` is
`Always`, or a whole-layer clip must apply after its effect.

- A direct layer replays its retained batches every presented frame and owns no
  colour texture.
- A clean direct layer costs only draw calls: no tessellation, upload, or text
  prepare.
- The swap chain has to be rebuilt every presented frame anyway. Today that
  costs one fullscreen composite per layer; under replay the cost scales with
  content, not with layer count × area.
- **New cost:** every presented frame clears and resolves the full MSAA target
  (risk R1).
- `CachePolicy::Always` reproduces today's behaviour exactly.

### 2.5 Bounded groups

**Bounds**, in integer physical pixels: the union of op bounds after scissor,
intersected with the whole-layer clip and the surface, then inflated by the
effect padding.

- **Blur** pads by `min(ceil(radius_phys), 32)` on each side. This matches
  `taps = min(ceil(radius), MAX_TAPS)` with `MAX_TAPS = 32` in
  `BLUR_SHADER_SRC` (verified).
- **Alpha-generating effects** take the whole-layer clip, or the surface if
  there is none, never their content bounds. These are a `ColorMatrix` with a
  nonzero alpha constant (row 4, column 5) and every `Custom` effect. This
  keeps `a_matrix_can_generate_alpha_from_nothing` green.
- Empty or offscreen bounds skip the group and its composite.

**Texture.**

- Allocate at a **bucketed** size: each axis rounded up to 256 and clamped to
  the surface. Content is drawn at the origin; the composite samples a uv
  sub-rect.
- Grow by reallocating. Shrink only after the content has used less than a
  quarter of the texture for K frames.
- Effect output and ping textures share the bucket, and
  `update_effect_uniforms` takes `texel_size` from the bucket.

**Scratch.** A resolve target must match the extent of its MSAA attachment.
`ScratchPool` therefore holds MSAA colour plus stencil per bucket size.

- It is used strictly sequentially in phase 2 and dropped after K unused
  frames.
- Surface-sized groups reuse the main MSAA and stencil textures. Phase 2 is
  recorded before phase 3, so they never overlap.

**Target uniform.** `ScreenSize { screen_size }` becomes
`Target { origin, size }`, with one bind group per target. The solid and image
shaders subtract `origin`.

**Composite.**

- The constant `composite_quad_vbuf` is replaced by a per-group 6-vertex
  `CompositeVertex` buffer, rewritten when the bounds change.
- The vertex layout is unchanged, so the `ShaderEffect::Custom` WGSL contract
  still compiles. Its `uv` now spans the group texture (Q3).
- The composite pipelines gain an MSAA+stencil variant for the main pass.
- A whole-layer shape clip on a group becomes push, quad, pop in the main pass:
  blur first, then clip, as today.
- Deleted: `MaskGpu`, the mask texture in `rebuild_mask`,
  `render_mask_if_needed`, and `MASKED_COMPOSITE_SHADER_SRC`.

### 2.6 Skipping (audit: visibility, empty, opaque coverage)

- **Empty layers:** no ops are recorded, and an empty group allocates nothing.
- **Clipped or offscreen content:** an empty scissor skips its ops, and a layer
  whose bounds miss the surface is skipped.
- **Opaque coverage is deferred.**
  - Opacity is not cheap to infer: gradients, images, and alpha all affect it.
  - Under replay, a covered direct layer costs only overdraw.
  - The dominant cost the audit measured (fullscreen composite per layer) is
    removed by 2.4 and 2.5.
  - Revisit after stage 8 with an explicit `Layer::set_opaque_rect` (Q7), not
    inference.

---

## 3. Public API

**Unchanged** (signatures and logical-pixel semantics):

- `Renderer::new_layer_top`, `Renderer::new_layer_bottom`, `LayerInvalidation`;
- all `Layer::draw_*` and `*_from_file` methods;
- `get_text_size`, `get_styled_text_size`, `scale_factor`, `clear`,
  `set_effect`.

New guarantee, to be documented on `Layer`: later draws paint over earlier ones.

**Added:**

```rust
impl Layer {
    /// Clip what `f` draws to `clip` (logical px). Nests. Integer-aligned unrounded
    /// rects are a free scissor; anything else is a stencil clip. No extra surface.
    /// Err (and `f` not called) only for an unparsable `ClipShape::Path`.
    pub fn with_clip<R>(&self, clip: ClipShape, f: impl FnOnce(&Layer) -> R)
        -> Result<R, lyon_extra::parser::ParseError>;
    /// Whole-layer clip: outermost scope, applied after any effect.
    pub fn set_clip(&self, clip: Option<ClipShape>) -> Result<(), lyon_extra::parser::ParseError>;
    pub fn set_cache_policy(&self, policy: CachePolicy);
}
pub enum CachePolicy { Auto /* offscreen only if an effect needs it */, Always /* today's behaviour */ }
impl Renderer {
    pub fn stats(&self) -> &RenderStats;
    pub fn set_offscreen_budget(&mut self, bytes: u64);
}
```

How `with_clip` works:

- It borrows the `RefCell` only long enough to push `PushClip`.
- It releases the borrow while `f` runs, then pushes `PopClip`.
- `f` receives the same handle, so nesting reads naturally.

**Removed:**

- `set_clip_rect` and `set_clip_shape`, replaced by `set_clip`. Whether to keep
  aliases is Q2.
- The module-doc claims that "a `Layer` owns its own offscreen GPU texture" and
  "give a shape its own layer to shade it". Rewrite those docs in `layer.rs` and
  `shader.rs`.
- Internals:
  - the fullscreen `composite_quad_vbuf`;
  - the per-layer surface `texture` in `LayerInner::new` and
    `resize_if_needed`;
  - the masked composite pipeline and its bind group layout;
  - the clear pass in `Renderer::render`;
  - `layer::render_layers`, replaced by
    `layer::render_frame(encoder, target_view, clear, ..)`.

---

## 4. Interactions

- **Retained-texture cache.**
  - `content_texture_dirty` and `effect_texture_dirty` remain, for groups
    only.
  - Direct layers rebuild batches when dirty and replay every frame.
  - A group whose bounds move within their bucket re-renders without
    reallocating.
- **DPI.**
  - Clip shapes are logical; bounds and buckets derive from physical bounds at
    build time.
  - `set_scale_factor` already marks layers dirty and patches the blur radius;
    it now also invalidates bounds.
  - `retained_content_follows_a_dpi_change` and
    `content_drawn_before_the_first_render_uses_the_scale_it_renders_at` stay
    green unchanged.
- **glyphon.**
  - Prepare happens per text batch, in phase 1, and is skipped when the hash
    and generation are unchanged (I1, I2).
  - Renderers are created with the stencil state.
  - `TextStack` today owns one shared `Viewport`, and `resize_if_needed`
    resizes it once per layer.
  - Proposed: `TextStack::viewport_for(size)`, a small cache keyed by target
    size, and `TextStack::render(renderer, pass)` becomes
    `render(renderer, viewport, pass)`. The `Renderer` updates the main
    viewport once per resize.
  - Group text is prepared with the group origin subtracted, because glyphon
    has no origin uniform. **A change in group origin therefore re-prepares
    that group's text.**
  - `FontSystem`, the atlas, and the shape cache stay shared.
- **Shared MSAA.** It becomes the colour attachment of the main pass and gains a
  sibling MSAA stencil. Both also serve as scratch for surface-sized groups,
  and both resize lazily in `Renderer::render`.
- **Test harness** (`test_support::Scene`):
  - `frame` calls `render_frame(clear = BLACK)` instead of its own clear pass
    followed by `render_layers`.
  - `Scene` owns the stencil.
  - Add `Scene::stats()`, `Scene::layer_with(CachePolicy)`, and
    `Scene::pixels(rect)`.

---

## 5. Budgets and counters

No stats type exists yet. `src/timing.rs` holds the `Stepped`/`Glide`
animation primitives, so it is not the right home. Add
`src/renderer/stats.rs`, separate from `get_gpu_frametime` (they can be joined
later).

```rust
pub struct RenderStats {          // reset at the start of each render()
    pub layers_live: u32, pub layers_replayed: u32, pub layers_skipped: u32,
    pub groups: u32, pub group_content_passes: u32, pub group_effect_passes: u32,
    pub batches: u32, pub draw_calls: u32,
    pub clip_pushes_scissor: u32, pub clip_pushes_stencil: u32, pub max_clip_depth: u8,
    pub clip_overflows: u32, pub unbalanced_pops: u32,
    pub geometry_rebuilds: u32, pub text_prepares: u32, pub text_renderers_live: u32,
    pub atlas_resets: u32, pub image_uploads: u32, pub image_upload_bytes: u64,
    pub bytes: ResourceBytes, pub budget_overruns: u32,
}
pub struct ResourceBytes {        // allocation arithmetic, not driver residency
    pub msaa_color: u64, pub msaa_stencil: u64, pub group_textures: u64,
    pub effect_textures: u64, pub scratch_pool: u64, pub geometry_buffers: u64,
}
```

**Offscreen budget** = group textures + effect textures + scratch pool.

- When it is exceeded, `Always` groups without an effect are demoted to direct,
  least-recently-dirty first. Their pixels stay identical.
- Effect groups are never demoted. Exceeding the budget with them increments
  `budget_overruns` and logs once per transition.
- The default is Q6.
- The `#[cfg(test)]` per-layer `content_passes`, `effect_passes`, and
  `effect_rebuilds` stay.

Attachment allocation arithmetic, with Stencil8 at 1 byte per sample
(**[assumption]**, see 2.3):

| Window | MSAA | Today, 12 layers | Today, 24 layers | Proposed, all direct (colour + stencil) |
|---|---|---:|---:|---:|
| 1920×1080 | 4× | 126.6 MiB | 221.5 MiB | 31.6 + 7.9 = **39.6 MiB** |
| 3840×2160 | 4× | 506.3 MiB | 885.9 MiB | 126.6 + 31.6 = **158.2 MiB** |
| 3840×2160 | 8× | 632.8 MiB | 1,012.5 MiB | 253.1 + 63.3 = **316.4 MiB** |

- **Group cost:** `bucket_w × bucket_h × 4` bytes, multiplied by 2 with a
  colour-matrix or custom effect and by 3 with blur. A scratch entry is added
  while the group is dirty.
- **Example:** a 400×300 blurred popup at 4K uses a 512² bucket. That is about
  3 MiB of textures, plus an 8 MiB 8× scratch while dirty. Today the same popup
  costs 94.9 MiB.
- **Emulated stencil:** if Stencil8 really costs 4 bytes per sample, the 4K 8×
  stencil is 253.1 MiB, which strengthens the case for a lazy stencil (Q4).

---

## 6. Staged plan

Each stage is one or two small commits that keep `cargo check` and the tests
green. All pixel tests use `test_support::Scene` (64×64, Rgba8UnormSrgb,
cleared to black).

- **Stage 0: counters and baseline.**
  - Add `stats.rs` and byte accounting for today's textures. Add
    `Scene::stats()`.
  - Tests: `n_layers_allocate_n_surface_textures` (pins today's cost so later
    drops show) and `stats_reset_each_frame`.
  - No behaviour change.
  - Optional pure-move split: `layer.rs` into
    `layer/{mod,pipelines,batch,clip,group}.rs`. Do this only after the
    concurrent edits land.
- **Stage 1: ordered batches, still full-size layer textures.**
  - `build_buffers` emits `ReplayOp`s, and `render_to_texture_if_needed`
    replays them.
  - Add the `TextBatch` pool and `prepare_batch`. Move every prepare ahead of
    all recording in `render_layers` (I1), with the I2 resweep.
  - Tests:
    - **`an_image_then_a_covering_rect_shows_the_rect`**: the audit probe, a
      white image under a blue rect, which must read blue.
    - `text_then_a_covering_rect_hides_the_text`: find an inked pixel first,
      then cover it.
    - `rect_text_rect_sandwich`.
    - `two_text_layers_in_one_frame`: the regression for glyphon destroying
      its vertex buffer.
    - `unchanged_text_batch_skips_prepare`.
    - `atlas_reset_mid_prepare_keeps_earlier_text`: two text layers, the
      second pushed near the ID limit with `set_glyph_ids_in_use_for_test`. An
      inked pixel of the first layer must survive.
  - The existing translucent tests must stay green unmodified.
- **Stage 2: scoped scissor clips.**
  - Add `PushClip`/`PopClip`, rect-only `with_clip`, and the scissor stack.
  - Tests: `nested_rect_clips_intersect`, `content_after_pop_is_unclipped`,
    `empty_intersection_draws_nothing` (via `draw_calls`),
    `scoped_clip_follows_dpi_change`,
    `unclosed_scope_does_not_leak_into_next_layer`.
- **Stage 3: stencil clips.**
  - Add the stencil attachment, pipeline variants, and glyphon
    `DepthStencilState`, plus the sample-count intersection.
  - Tests: `circle_clip_inside_rounded_rect_clip`,
    `text_is_clipped_by_a_shape`, `shape_clip_edge_is_antialiased` (the edge
    pixel must sit strictly between the inside and outside values),
    `fractional_rect_clip_uses_stencil`, `pop_restores_parent_shape_clip`,
    `clip_depth_overflow_is_counted`, and one test at the adapter-picked MSAA
    count.
- **Stage 4: main pass and direct layers.**
  - Add `render_frame`. Layers without an effect become direct; effect layers
    stay full-size groups composited in-pass.
  - `set_clip` replaces the old clip calls. Delete the mask path and the clear
    pass.
  - New tests: `direct_layers_allocate_no_colour_textures`,
    `empty_layer_records_no_draws`,
    `interleaved_direct_and_group_layers_keep_z_order`, and
    `blurred_layer_is_clipped_after_blur`. Port
    `a_clip_shape_cuts_the_layer_outside_it` to `set_clip`.
  - **Existing tests whose meaning changes.** Do not claim "all tests pass
    unchanged" for this stage:
    - `passes()`, `a_clean_layer_is_composited_without_re_rendering`,
      `changing_only_the_effect_reruns_only_the_effect`, and
      `animating_effect_parameters_reuses_the_effect_textures` set an effect,
      so they still exercise the group path.
    - Each of them gets a direct-layer twin asserting that `geometry_rebuilds`
      and `text_prepares` stay at 0 on clean frames.
    - `a_dpi_change_rerenders_a_retained_layer` uses a plain rect. It moves to
      `CachePolicy::Always`, and its direct twin asserts exactly one
      `geometry_rebuilds`.
- **Stage 5: bounded groups.**
  - Bounds, buckets, `ScratchPool`, the `Target` uniform, `viewport_for`,
    origin-offset text prepare, the bounded composite quad, effect padding, and
    the alpha-generating rule.
  - Tests:
    - `group_pixels_match_direct_pixels`: compare a pixel grid between
      `Always` and `Auto`.
    - `group_at_offset_with_text_renders_in_place`.
    - `blur_bleeds_past_content_by_its_padding`.
    - `a_matrix_can_generate_alpha_from_nothing`, unchanged.
    - `group_texture_is_smaller_than_surface`.
    - `moving_group_within_bucket_does_not_reallocate`.
- **Stage 6: budget and skipping.**
  - Tests: `over_budget_cached_group_demotes_with_identical_pixels`,
    `effect_group_over_budget_counts_overrun`, `offscreen_layer_is_skipped`.
- **Stage 7: bounds-disjoint batch merging.**
  - Tests: `disjoint_rect_text_lines_merge_to_two_batches` and
    `overlapping_commands_are_never_reordered` (the audit probe with overlap).
- **Stage 8: exit criteria for audit step 4.**
  - Re-run the audit's 1080p and 4K layer workloads plus dirty-scene workloads,
    recording `RenderStats` bytes and passes.
  - Add the R1 scene: a blinking caret over a full-screen styled-text layer at
    4K, 8× MSAA, on an **integrated GPU**, run once direct and once with
    `CachePolicy::Always`.
  - Record the results beside the audit's numbers before declaring the budget
    suitable.

---

## 7. Risks and open questions (ranked)

1. **R1: clean frames cost more.**
   - Today a caret blink re-renders only the caret layer and composites N
     quads.
   - Under replay, every presented frame clears and resolves the full MSAA
     target and replays every direct layer.
   - **[Assumption]** Fast-clear and compressed resolve make this cheap on
     discrete GPUs. Integrated GPUs at 4K 8× may be slower than today.
   - Mitigations: `CachePolicy::Always` for large static layers, a later
     `Auto` promotion heuristic, 4× MSAA at high DPI.
   - Stage 8 decides.
2. **R2: text-batch fragmentation.**
   - Every batch has its own glyphon vertex buffer (initially
     `next_copy_buffer_size(4096)`) and its own prepare.
   - Mitigation: stage 7. Watch `text_renderers_live`.
3. **R3: stencil support and cost.**
   - **[Assumption]** Stencil8 size and emulation.
   - The sample count must satisfy both formats.
   - Lazy attachment would double the pipeline and glyphon-renderer variants.
4. **R4: semantic shifts.**
   - `Custom` effect `uv` becomes group-local.
   - Alpha-generating effects need clip or surface bounds.
   - Direct blending skips the 8-bit intermediate layer texture, so pixels may
     move by about 1 level. Existing tolerances are 1–3.
   - I2's resweep costs a second prepare in reset frames.
5. **R5: merge conflicts.** `layer.rs` is 3.3k lines and under concurrent edit.
   Sequence stage 0's split after that work lands.

**Questions for April:**

- **Q1.** Direct replay as the default (R1), or keep every layer cached and
  make direct opt-in? *Recommendation: direct, confirmed by stage-8 data.*
- **Q2.** Closure `with_clip` (recommended), explicit push/pop, or a guard?
  Keep `set_clip_rect`/`set_clip_shape` as aliases? *Recommendation: remove
  them.*
- **Q3.** Should `Custom` effect `uv` be group-local (as proposed), or should
  `Custom` groups always be surface-sized to keep screen-space `uv`?
- **Q4.** Stencil always attached (simple; recommended until measured) or lazy
  (saves memory, needs two pipeline sets)?
- **Q5.** In-stream groups (`with_group(effect, |l| ..)`) now or later?
  *Recommendation: later.* Once groups are bounded, layers already scope
  effects cheaply.
- **Q6.** Default offscreen budget? *Suggestion: max(64 MiB, 2 × surface RGBA
  bytes).*
- **Q7.** Opaque-coverage skipping through an explicit `set_opaque_rect`, or
  leave it out until profiling asks?

---

## 8. Implementation status and measured results (2026-10-20)

Implemented: stages 1 to 4 and the counters of stage 0/6 (`Renderer::stats`).
Not implemented: stage 5 (bounded group textures and the scratch pool),
stage 6 budgets and demotion, stage 7 (bounds-disjoint batch merging).
Decisions taken: direct replay by default (Q1), closure `with_clip` and the
old setters removed (Q2), `Custom` effect `uv` stays surface-wide for now
since groups are still surface-sized (Q3), stencil always attached (Q4),
in-stream groups later (Q5).

Benchmark (`cargo test --release --lib renderer::bench -- --ignored
--nocapture --test-threads=1`), Windows, NVIDIA RTX 5070 Ti, Vulkan, 8x MSAA
(the adapter-picked count). Serialized CPU+GPU cost per clean frame, 60
samples after 10 warmup; the audit's workload (N layers, each a 100x100 rect
with a clip). Memory is the allocation arithmetic from `RenderStats`.

| Target | Layers | Direct: median / p95 ms | Direct MiB | Cached: median / p95 ms | Cached MiB |
|---|---:|---|---:|---|---:|
| 1920x1080 | 1 | 0.096 / 0.228 | 79.1 | 0.097 / 0.156 | 87.0 |
| 1920x1080 | 12 | 0.101 / 0.162 | 79.1 | 0.102 / 0.160 | 174.0 |
| 1920x1080 | 24 | 0.109 / 0.173 | 79.1 | 0.104 / 0.149 | 268.9 |
| 3840x2160 | 12 | 0.195 / 0.245 | 316.4 | 0.192 / 0.304 | 696.1 |
| 3840x2160 | 24 | 0.203 / 0.369 | 316.4 | 0.204 / 0.371 | 1,075.8 |

The audit's clean-frame figures for comparison (4x MSAA, same GPU): upstream
0.512 ms (1080p, 12 layers) and 2.750 ms (4K, 24 layers); the fork 0.234 and
0.839 ms. A blinking caret over a full-screen text page: 1080p 0.114 ms direct
vs 0.126 ms cached; 4K 0.214 ms direct vs 0.295 ms cached.

Findings:

- **R1 did not materialise on a discrete GPU.** Replaying a full text page
  every presented frame costs no more than compositing a cached texture. An
  integrated GPU was not tested and remains the open question.
- **Memory is dominated by the MSAA targets, not by layers.** Colour plus
  stencil at 8x is 79 MiB at 1080p and 316 MiB at 4K regardless of layer
  count. Cached groups still cost a surface-sized texture each (stage 5).
- An adaptive sample count (4x above a pixel budget) would cut the 4K figure
  to 158 MiB; the pipelines are built for one count at startup, so this needs
  an owner decision on quality versus memory.
