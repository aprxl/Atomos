//! [`TextStack`] — the glyphon/cosmic-text machinery shared by every
//! [`super::Layer`]'s `draw_text`.
//!
//! `FontSystem`/`SwashCache`/`TextAtlas`/`Viewport` are one shared instance,
//! not one per layer: `FontSystem` is expensive to duplicate (it walks the
//! system font database once at startup), and `TextAtlas` (the rasterized
//! glyph bitmap cache) is specifically designed to back multiple
//! `TextRenderer`s at once. `TextRenderer` itself is *not* shared, though —
//! see [`TextStack::create_text_renderer`]'s doc for why one shared
//! `TextRenderer` across layers is actually unsound (a real crash this
//! renderer hit): each layer gets its own via that method.
//!
//! **Text is re-*prepared* every frame, but only re-*shaped* when its
//! content actually changes.** The prepare side is a hard constraint:
//! glyphon's `prepare()`/`render()` pair must run back-to-back for a given
//! layer (prepare uploads this frame's glyphs into the shared atlas;
//! render draws whatever was just uploaded), and since every layer's
//! render pass shares that one atlas, skipping `prepare()` for an
//! "unchanged" layer risks drawing glyphs a *different* layer's
//! `prepare()` already evicted. Shaping, though — the genuinely expensive
//! part (rustybuzz layout plus a fresh cosmic-text `Buffer`) — is cached
//! in [`TextStack::shape_cache`], keyed by shaping inputs only: text,
//! font, size, tracking (plus per-span color for styled text, which is
//! baked into the shaped buffer via `Attrs`). Position, alignment, and
//! the synthetic weight/width are placement/raster-time inputs and
//! deliberately *not* part of the key, so moving or fading text never
//! re-shapes. Entries unused for [`SHAPE_CACHE_EVICT_AFTER_FRAMES`]
//! frames are dropped by [`TextStack::advance_frame`], called once per
//! rendered frame — the cache is bounded by "distinct text visible
//! recently", which for an editor viewport is a few thousand small
//! buffers at most.

use std::collections::hash_map::{DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphonColor, Family, FontSystem, LayoutGlyph, Metrics,
    Resolution, Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
    fontdb,
};

use super::color::Color;
use super::draw_command::DrawCommand;
use super::font::Font;
use super::font_parameters::FontParameters;
use super::glyph_effects::{EffectGlyphs, GlyphEffect, effect_advance_delta};
use super::text_span::TextSpan;
use super::{Alignment, HorizontalAlign, VerticalAlign};

/// Line height as a multiple of font size — matches the ratio the
/// original Phase-0 text spike used.
const LINE_HEIGHT_RATIO: f32 = 1.25;

/// How many frames a [`TextStack::shape_cache`] entry may go unused before
/// [`TextStack::advance_frame`] drops it. ~5 seconds at 60 Hz: long enough
/// that scrolling back to recently-seen text is still a hit, short enough
/// that one-off strings (a frame counter, a transient message) don't
/// accumulate forever.
const SHAPE_CACHE_EVICT_AFTER_FRAMES: u64 = 300;

/// How often [`TextStack::advance_frame`] calls `atlas.trim()` (bounding
/// the shared glyph atlas's memory growth) and bumps `atlas_generation`.
/// ~30 seconds at 60 Hz — infrequent enough that the "every layer with
/// text must re-prepare once" cost a trim forces (see
/// `LayerInner::rebuild_if_needed`) stays rare, frequent enough to reclaim
/// glyphs from long-idle sessions with heavy glyph churn (e.g. continuous
/// `FontParameters::weight`/`width` animation, which mints a new
/// `EffectGlyphKey` — and atlas entry — per distinct value).
const ATLAS_TRIM_INTERVAL_FRAMES: u64 = 1800;

/// One cached shaping result — see the module doc for what the cache key
/// covers and why.
struct CachedShape {
    buffer: Buffer,
    /// `TextStack::frame` value the entry was last requested on.
    last_used: u64,
}

/// Cache key for [`TextStack::registered_fonts`]. `Bytes` is keyed by
/// pointer identity + length rather than content — same reasoning as
/// [`Font`]'s `Hash` impl.
#[derive(PartialEq, Eq, Hash)]
enum FontKey {
    Bytes(usize, usize),
    File(PathBuf),
}

impl FontKey {
    fn from_font(font: &Font) -> Option<Self> {
        match font {
            Font::Named(_) => None,
            Font::Bytes(bytes) => Some(FontKey::Bytes(bytes.as_ptr() as usize, bytes.len())),
            Font::File(path) => Some(FontKey::File(path.clone())),
        }
    }
}

pub(super) struct TextStack {
    font_system: FontSystem,
    swash_cache: SwashCache,
    atlas: TextAtlas,
    viewport: Viewport,
    effect_glyphs: EffectGlyphs,
    // Kept to rebuild `atlas` when the glyph-ID space is recycled.
    glyph_cache: Cache,
    format: wgpu::TextureFormat,
    // Resolved family name for each not-yet-seen `Font::Bytes`/`Font::File`
    // — `None` means registration failed (logged once, at registration
    // time). `Font::Named` never needs an entry; it's already a name.
    registered_fonts: HashMap<FontKey, Option<String>>,
    // Every `TextArea` draws through `custom_glyphs` (so `FontParameters`'
    // weight/width apply uniformly, even at the neutral 0.0/1.0 defaults),
    // so every `TextArea` needs a `buffer` field despite never using its
    // glyphs directly. One permanently-empty `Buffer` shared by all of
    // them, rather than allocating a fresh empty one per `draw_text` call.
    blank_buffer: Buffer,
    // Shaped-buffer cache — see the module doc. Keyed by a hash of the
    // shaping inputs ([`shaped_text_key`]/[`shaped_spans_key`]).
    shape_cache: HashMap<u64, CachedShape>,
    // Frame clock for `shape_cache` eviction, bumped once per rendered
    // frame by [`TextStack::advance_frame`].
    frame: u64,
    // Bumped every [`ATLAS_TRIM_INTERVAL_FRAMES`] frames, each time
    // `advance_frame` calls `atlas.trim()`. A `LayerInner` compares this
    // against the generation it last successfully prepared under — a
    // mismatch means the atlas may have evicted glyphs it depends on since
    // then (trim clears glyphon's "in use" protection set), so it must
    // re-prepare even if its own text content hasn't changed. See
    // `LayerInner::rebuild_if_needed`'s skip-prepare logic.
    atlas_generation: u64,
    // The MSAA sample count every `Layer`'s pipelines use — resolved once
    // by `Renderer::new` via `pick_msaa_sample_count` and threaded in here
    // so `create_text_renderer` (called later, per layer) can match it;
    // glyphon's `TextRenderer` must be built against the same sample count
    // as the render pass it draws into.
    msaa_sample_count: u32,
}

impl TextStack {
    pub(super) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        size: (u32, u32),
        msaa_sample_count: u32,
    ) -> Self {
        // `FontSystem::new` walks the system font database; on cold caches
        // it can take a second — paid once, here, not per layer.
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        // `Cache` holds the shared pipeline/bind group layouts a `TextAtlas`
        // and `Viewport` are built from — sharing it (rather than the atlas
        // itself, which is per-`TextRenderer`) is glyphon's own intended
        // pattern for multiple text-drawing surfaces.
        let glyph_cache = Cache::new(device);
        let atlas = TextAtlas::new(device, queue, &glyph_cache, format);
        let mut viewport = Viewport::new(device, &glyph_cache);
        viewport.update(
            queue,
            Resolution {
                width: size.0,
                height: size.1,
            },
        );

        let blank_buffer = build_buffer(&mut font_system, "", &Attrs::new(), 16.0);

        Self {
            font_system,
            swash_cache,
            atlas,
            viewport,
            effect_glyphs: EffectGlyphs::new(),
            glyph_cache,
            format,
            registered_fonts: HashMap::new(),
            blank_buffer,
            shape_cache: HashMap::new(),
            frame: 0,
            atlas_generation: 0,
            msaa_sample_count,
        }
    }

    /// Advance the shape-cache clock (dropping entries unused for
    /// [`SHAPE_CACHE_EVICT_AFTER_FRAMES`] frames) and, every
    /// [`ATLAS_TRIM_INTERVAL_FRAMES`] frames, trim the shared glyph atlas
    /// and bump `atlas_generation`. Called once per rendered frame by
    /// `Renderer::render` (not per layer — several layers share this stack
    /// within one frame).
    pub(super) fn advance_frame(&mut self) {
        self.frame += 1;
        let frame = self.frame;
        self.shape_cache
            .retain(|_, entry| frame - entry.last_used <= SHAPE_CACHE_EVICT_AFTER_FRAMES);

        if frame.is_multiple_of(ATLAS_TRIM_INTERVAL_FRAMES) {
            self.atlas.trim();
            self.atlas_generation += 1;
        }
    }

    /// Discard every rasterized glyph and the atlas holding them, then
    /// bump `atlas_generation` so every layer re-prepares its text. The IDs
    /// and the atlas are only ever dropped together; see [`EffectGlyphs`].
    pub(super) fn reset_glyph_atlas(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        self.effect_glyphs.reset();
        self.atlas = TextAtlas::new(device, queue, &self.glyph_cache, self.format);
        self.atlas_generation += 1;
    }

    #[cfg(test)]
    pub(super) fn set_glyph_ids_in_use_for_test(&mut self, in_use: usize) {
        self.effect_glyphs.set_next_id_for_test(in_use);
    }

    #[cfg(test)]
    pub(super) fn glyph_ids_in_use_for_test(&self) -> usize {
        self.effect_glyphs.next_id_for_test()
    }

    /// Current atlas protection epoch — see the `atlas_generation` field
    /// doc. Compared by [`super::layer::LayerInner`] against the
    /// generation it last successfully prepared under.
    pub(super) fn atlas_generation(&self) -> u64 {
        self.atlas_generation
    }

    /// Build a new [`TextRenderer`] registered against this stack's shared
    /// [`TextAtlas`] — one per [`super::Layer`], not one shared instance.
    ///
    /// A `TextRenderer` owns exactly one vertex buffer, and
    /// `prepare_with_custom` calls `.destroy()` on it *immediately*
    /// (synchronously, not deferred to whenever the GPU actually gets
    /// around to it) when it needs to grow — see glyphon's
    /// `text_render.rs`. `render_layers` records every layer's render pass
    /// into one shared `wgpu::CommandEncoder` that isn't submitted until
    /// the whole frame is done, so if every layer shared one
    /// `TextRenderer`, an earlier layer's already-recorded
    /// `set_vertex_buffer` could end up pointing at a buffer a *later*
    /// layer's `prepare_with_custom` destroyed before the frame ever
    /// submits — a real crash this renderer hit as soon as more than one
    /// layer drew text in the same frame. Giving each layer its own
    /// `TextRenderer` (own vertex buffer) sidesteps this entirely, while
    /// the actually-expensive-to-duplicate state (`FontSystem`'s font
    /// database, `TextAtlas`'s rasterized glyph bitmaps) stays shared —
    /// `TextAtlas` is specifically designed to back multiple
    /// `TextRenderer`s at once (`TextRenderer::new` takes `&mut TextAtlas`
    /// for exactly this reason).
    pub(super) fn create_text_renderer(&mut self, device: &wgpu::Device) -> TextRenderer {
        TextRenderer::new(
            &mut self.atlas,
            device,
            wgpu::MultisampleState {
                count: self.msaa_sample_count,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            Some(super::layer::content_stencil_state()),
        )
    }

    pub(super) fn resize(&mut self, queue: &wgpu::Queue, size: (u32, u32)) {
        self.viewport.update(
            queue,
            Resolution {
                width: size.0,
                height: size.1,
            },
        );
    }

    /// Measure the pixel size `text` would occupy shaped with `font`/
    /// `font_parameters`, before any [`Alignment`] offset is applied — the
    /// same measurement [`TextStack::prepare_layer`] uses internally to
    /// resolve alignment (and the same shape cache, so measuring text that
    /// is also drawn this frame shapes it once, not twice). Exposed via
    /// [`super::Layer::get_text_size`].
    pub(super) fn measure(
        &mut self,
        text: &str,
        font: &Font,
        font_parameters: &FontParameters,
    ) -> (f32, f32) {
        let key = self.ensure_shaped(text, font, font_parameters);
        let effect = GlyphEffect::from(font_parameters);
        measure_shaped_buffer_with(&self.shape_cache[&key].buffer, |_| effect)
    }

    /// The interaction geometry of one `draw_text`-style string, in
    /// physical pixels (the caller converts to logical).
    pub(super) fn layout(
        &mut self,
        text: &str,
        font: &Font,
        font_parameters: &FontParameters,
    ) -> super::text_layout::TextLayout {
        let key = self.ensure_shaped(text, font, font_parameters);
        let effect = GlyphEffect::from(font_parameters);
        super::text_layout::TextLayout::from_buffer(&self.shape_cache[&key].buffer, text, effect)
    }

    /// Look up (or shape and insert) the cached buffer for one
    /// `draw_text`-style string, returning its cache key. The entry is
    /// stamped with the current frame either way.
    fn ensure_shaped(&mut self, text: &str, font: &Font, font_parameters: &FontParameters) -> u64 {
        let key = shaped_text_key(text, font, font_parameters);
        let frame = self.frame;
        if let Some(entry) = self.shape_cache.get_mut(&key) {
            entry.last_used = frame;
            return key;
        }
        let buffer = self.shape(text, font, font_parameters);
        self.shape_cache.insert(
            key,
            CachedShape {
                buffer,
                last_used: frame,
            },
        );
        key
    }

    /// [`TextStack::ensure_shaped`], for a `draw_styled_text` span list.
    fn ensure_shaped_spans(&mut self, spans: &[TextSpan]) -> u64 {
        let key = shaped_spans_key(spans);
        let frame = self.frame;
        if let Some(entry) = self.shape_cache.get_mut(&key) {
            entry.last_used = frame;
            return key;
        }
        let buffer = self.shape_spans(spans);
        self.shape_cache.insert(
            key,
            CachedShape {
                buffer,
                last_used: frame,
            },
        );
        key
    }

    /// Resolve a [`Font`] to the family name cosmic-text's `Attrs` should
    /// select, registering `Bytes`/`File` fonts with `FontSystem`'s
    /// `fontdb` the first time they're seen.
    fn resolve_family(&mut self, font: &Font) -> Option<String> {
        let Font::Named(name) = font else {
            let key = FontKey::from_font(font).expect("only Named has no FontKey");
            if let Some(cached) = self.registered_fonts.get(&key) {
                return cached.clone();
            }
            let resolved = match font {
                Font::Bytes(bytes) => register_font_bytes(self.font_system.db_mut(), bytes),
                Font::File(path) => match register_font_file(self.font_system.db_mut(), path) {
                    Ok(name) => name,
                    Err(e) => {
                        eprintln!("Atomos: failed to load font {}: {e}", path.display());
                        None
                    }
                },
                Font::Named(_) => unreachable!(),
            };
            self.registered_fonts.insert(key, resolved.clone());
            return resolved;
        };
        Some(name.clone())
    }

    fn shape(&mut self, text: &str, font: &Font, font_parameters: &FontParameters) -> Buffer {
        let family = self.resolve_family(font);
        let mut attrs = Attrs::new();
        if let Some(family) = &family {
            attrs = attrs.family(Family::Name(family));
        }
        attrs = attrs.letter_spacing(font_parameters.tracking);
        build_buffer(&mut self.font_system, text, &attrs, font_parameters.size)
    }

    /// Measure the combined bounding box `spans` would occupy shaped
    /// together, before any [`Alignment`] offset — the spans counterpart to
    /// [`TextStack::measure`], sharing the same shape cache. Exposed via
    /// [`super::Layer::get_styled_text_size`].
    pub(super) fn measure_spans(&mut self, spans: &[TextSpan]) -> (f32, f32) {
        let key = self.ensure_shaped_spans(spans);
        let span_effects: Vec<GlyphEffect> = spans
            .iter()
            .map(|span| GlyphEffect::from(&span.font_parameters))
            .collect();
        measure_shaped_buffer_with(&self.shape_cache[&key].buffer, |glyph| {
            span_effects
                .get(glyph.metadata)
                .copied()
                .unwrap_or(GlyphEffect::NEUTRAL)
        })
    }

    /// Like [`TextStack::shape`], but for [`TextSpan`]s: every span is
    /// shaped together as a single cosmic-text rich-text buffer (one shape
    /// operation for the whole run, not one per span — the entire point of
    /// `draw_styled_text`), each keeping its own font/size/tracking/color.
    /// `weight`/`width` (this renderer's synthetic faux-bold/condense) have
    /// no native per-span cosmic-text attribute and don't participate in
    /// shaping at all — callers rebuild the per-span [`GlyphEffect`] table
    /// themselves and recover a glyph's effect via the `Attrs::metadata`
    /// span index each span is tagged with below. Color, size, and
    /// tracking all have native per-span `Attrs` fields.
    fn shape_spans(&mut self, spans: &[TextSpan]) -> Buffer {
        // Resolved before building any `Attrs` below, since those borrow
        // the resolved family names by reference — `resolve_family` needs
        // `&mut self` and can't be called again once that borrow starts.
        let families: Vec<Option<String>> = spans
            .iter()
            .map(|span| self.resolve_family(&span.font))
            .collect();

        let attrs_per_span: Vec<Attrs> = spans
            .iter()
            .zip(&families)
            .enumerate()
            .map(|(index, (span, family))| {
                let mut attrs = Attrs::new();
                if let Some(family) = family {
                    attrs = attrs.family(Family::Name(family));
                }
                attrs = attrs.letter_spacing(span.font_parameters.tracking);
                attrs = attrs.metrics(Metrics::new(
                    span.font_parameters.size,
                    span.font_parameters.size * LINE_HEIGHT_RATIO,
                ));
                let Color::Solid(rgba) = span.color else {
                    unreachable!(
                        "draw_styled_text only accepts Color::Solid, enforced in Layer::draw_styled_text"
                    );
                };
                attrs = attrs.color(GlyphonColor::rgba(rgba[0], rgba[1], rgba[2], rgba[3]));
                attrs = attrs.metadata(index);
                attrs
            })
            .collect();

        // Only used as the buffer's initial/default metrics — every span
        // above carries its own `Attrs::metrics` override, so this never
        // actually applies to rendered glyphs. Just needs to be non-zero.
        let default_size = spans.first().map_or(16.0, |span| span.font_parameters.size);
        let mut buffer = Buffer::new(
            &mut self.font_system,
            Metrics::new(default_size, default_size * LINE_HEIGHT_RATIO),
        );
        {
            let mut borrowed = buffer.borrow_with(&mut self.font_system);
            borrowed.set_size(None, None);
            borrowed.set_rich_text(
                spans
                    .iter()
                    .zip(&attrs_per_span)
                    .map(|(span, attrs)| (span.text.as_str(), attrs.clone())),
                &Attrs::new(),
                Shaping::Advanced,
                None,
            );
            borrowed.shape_until_scroll(true);
        }
        buffer
    }

    /// Shape and rasterize every `DrawCommand::Text`/`StyledText` in
    /// `commands`, then hand the resulting glyphs to `text_renderer` (the
    /// caller `Layer`'s own — see [`TextStack::create_text_renderer`]) —
    /// called once per layer, immediately before that layer's own render
    /// pass (see the module doc for why the ordering matters). Layers with
    /// no text commands still call this (with an empty batch), so stale
    /// glyphs from a previous frame don't linger.
    pub(super) fn prepare_layer(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        commands: &[DrawCommand],
        text_renderer: &mut TextRenderer,
    ) -> Result<(), glyphon::PrepareError> {
        // Recycle the glyph-ID space (and the bitmaps behind it) before it
        // can run out. glyphon keys atlas entries by these `u16` IDs, so IDs
        // are only reusable if the atlas is rebuilt in the same step; bumping
        // the generation makes every layer re-prepare against the new one.
        if self.effect_glyphs.needs_reset() {
            self.reset_glyph_atlas(device, queue);
        }

        let mut all_glyphs = Vec::new();
        for command in commands {
            match command {
                DrawCommand::Text {
                    text,
                    position,
                    font,
                    font_parameters,
                    alignment,
                    color,
                } => {
                    let key = self.ensure_shaped(text, font, font_parameters);
                    let effect = GlyphEffect::from(font_parameters);
                    let rgba = match color {
                        Color::Solid(rgba) => *rgba,
                        _ => {
                            debug_assert!(
                                false,
                                "draw_text only supports Color::Solid — falling back to opaque white"
                            );
                            [0xFF, 0xFF, 0xFF, 0xFF]
                        }
                    };
                    // Destructured so the cached buffer (immutable) and
                    // `font_system`/`effect_glyphs` (mutable) borrows are
                    // visibly disjoint fields of `self`.
                    let TextStack {
                        shape_cache,
                        font_system,
                        effect_glyphs,
                        ..
                    } = self;
                    let buffer = &shape_cache[&key].buffer;
                    let (width, height) = measure_shaped_buffer_with(buffer, |_| effect);
                    let anchored = apply_alignment(*position, *alignment, width, height);
                    let glyphs = effect_glyphs.layout_to_custom_glyphs(
                        font_system,
                        buffer,
                        (anchored[0], anchored[1]),
                        effect,
                        rgba,
                    );
                    all_glyphs.push(glyphs);
                }
                DrawCommand::StyledText {
                    spans,
                    position,
                    alignment,
                } => {
                    let key = self.ensure_shaped_spans(spans);
                    let span_effects: Vec<GlyphEffect> = spans
                        .iter()
                        .map(|span| GlyphEffect::from(&span.font_parameters))
                        .collect();
                    let TextStack {
                        shape_cache,
                        font_system,
                        effect_glyphs,
                        ..
                    } = self;
                    let buffer = &shape_cache[&key].buffer;
                    let (width, height) = measure_shaped_buffer_with(buffer, |glyph| {
                        span_effects
                            .get(glyph.metadata)
                            .copied()
                            .unwrap_or(GlyphEffect::NEUTRAL)
                    });
                    let anchored = apply_alignment(*position, *alignment, width, height);
                    let glyphs = effect_glyphs.layout_to_custom_glyphs_spans(
                        font_system,
                        buffer,
                        (anchored[0], anchored[1]),
                        &span_effects,
                    );
                    all_glyphs.push(glyphs);
                }
                _ => continue,
            }
        }

        let text_areas = all_glyphs.iter().map(|glyphs| TextArea {
            buffer: &self.blank_buffer,
            left: 0.0,
            top: 0.0,
            scale: 1.0,
            bounds: TextBounds::default(),
            default_color: GlyphonColor::rgb(0xFF, 0xFF, 0xFF),
            custom_glyphs: glyphs,
        });

        // `effect_glyphs` is bound as a local so the closure below captures
        // just this reference, not `self` — `self.font_system`/`self.atlas`
        // need their own simultaneous `&mut self` borrows below.
        let effect_glyphs = &self.effect_glyphs;
        text_renderer.prepare_with_custom(
            device,
            queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            text_areas,
            &mut self.swash_cache,
            |request| effect_glyphs.rasterize(request),
        )
    }

    /// Draw whatever [`TextStack::prepare_layer`] just uploaded into
    /// `text_renderer` (the caller `Layer`'s own) into the currently active
    /// render pass.
    pub(super) fn render(
        &self,
        text_renderer: &TextRenderer,
        rpass: &mut wgpu::RenderPass<'_>,
    ) -> Result<(), glyphon::RenderError> {
        text_renderer.render(&self.atlas, &self.viewport, rpass)
    }
}

/// Shape-cache key for one `draw_text` string: exactly the inputs
/// [`TextStack::shape`] reads, nothing more. Weight/width/color/position/
/// alignment are deliberately absent — they're applied per glyph at
/// placement/raster time, so changing them (fading, moving) must stay a
/// cache hit.
fn shaped_text_key(text: &str, font: &Font, font_parameters: &FontParameters) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write_u8(0); // namespace tag vs. `shaped_spans_key`
    text.hash(&mut hasher);
    font.hash(&mut hasher);
    hasher.write_u32(font_parameters.size.to_bits());
    hasher.write_u32(font_parameters.tracking.to_bits());
    hasher.finish()
}

/// Shape-cache key for a `draw_styled_text` span list. Same rules as
/// [`shaped_text_key`], with one addition: span *color* is part of the key
/// because it's baked into the shaped buffer via `Attrs::color` (unlike
/// `draw_text`'s color, which is applied at glyph emission) — so animating
/// a styled span's color re-shapes. Weight/width stay excluded here too.
fn shaped_spans_key(spans: &[TextSpan]) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write_u8(1); // namespace tag vs. `shaped_text_key`
    hasher.write_usize(spans.len());
    for span in spans {
        span.text.hash(&mut hasher);
        span.font.hash(&mut hasher);
        hasher.write_u32(span.font_parameters.size.to_bits());
        hasher.write_u32(span.font_parameters.tracking.to_bits());
        span.color.hash_bits(&mut hasher);
    }
    hasher.finish()
}

fn build_buffer(font_system: &mut FontSystem, text: &str, attrs: &Attrs, size_px: f32) -> Buffer {
    let metrics = Metrics::new(size_px, size_px * LINE_HEIGHT_RATIO);
    let mut buffer = Buffer::new(font_system, metrics);
    {
        let mut borrowed = buffer.borrow_with(font_system);
        borrowed.set_size(None, None);
        borrowed.set_text(text, attrs, Shaping::Advanced, None);
        borrowed.shape_until_scroll(true);
    }
    buffer
}

/// Total width (widest line) and height (bottom edge of the last line) of
/// an already-shaped buffer, *including* the per-glyph advance corrections
/// the synthetic `weight`/`width` effects introduce at render time
/// (`effect` returns a glyph's `(embolden, condense)` — see
/// [`effect_advance_delta`]). Without these deltas, a line ending in bold
/// glyphs would measure narrower than it renders (and a condensed line
/// wider), breaking `Alignment::Center`/`Right` and `get_text_size`-sized
/// boxes.
fn measure_shaped_buffer_with(
    buffer: &Buffer,
    mut effect: impl FnMut(&LayoutGlyph) -> GlyphEffect,
) -> (f32, f32) {
    let mut width: f32 = 0.0;
    let mut height: f32 = 0.0;
    for run in buffer.layout_runs() {
        let mut delta: f32 = 0.0;
        for glyph in run.glyphs {
            let effect = effect(glyph);
            delta += effect_advance_delta(effect.embolden.max(0.0), effect.condense, glyph.w);
        }
        width = width.max(run.line_w + delta);
        height = height.max(run.line_top + run.line_height);
    }
    (width, height)
}

/// Shift an anchor position so it represents `alignment`'s point of a
/// `width` x `height` box instead of always its top-left.
fn apply_alignment(position: [f32; 2], alignment: Alignment, width: f32, height: f32) -> [f32; 2] {
    let dx = match alignment.horizontal {
        HorizontalAlign::Left => 0.0,
        HorizontalAlign::Center => -width / 2.0,
        HorizontalAlign::Right => -width,
    };
    let dy = match alignment.vertical {
        VerticalAlign::Top => 0.0,
        VerticalAlign::Center => -height / 2.0,
        VerticalAlign::Bottom => -height,
    };
    [position[0] + dx, position[1] + dy]
}

/// Register font bytes with `db` if not already present, returning the
/// resolved family name to select it by. Discovers the name by diffing
/// `db`'s faces before/after — `load_font_data` doesn't return the ID(s) it
/// added directly.
fn register_font_bytes(db: &mut fontdb::Database, bytes: &'static [u8]) -> Option<String> {
    let before: std::collections::HashSet<fontdb::ID> = db.faces().map(|f| f.id).collect();
    db.load_font_data(bytes.to_vec());
    db.faces()
        .find(|f| !before.contains(&f.id))
        .and_then(|f| f.families.first().map(|(name, _)| name.clone()))
}

/// [`register_font_bytes`], loading from a file instead.
fn register_font_file(
    db: &mut fontdb::Database,
    path: &Path,
) -> Result<Option<String>, std::io::Error> {
    let before: std::collections::HashSet<fontdb::ID> = db.faces().map(|f| f.id).collect();
    db.load_font_file(path)?;
    Ok(db
        .faces()
        .find(|f| !before.contains(&f.id))
        .and_then(|f| f.families.first().map(|(name, _)| name.clone())))
}
