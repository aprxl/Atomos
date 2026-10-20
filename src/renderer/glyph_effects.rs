//! Custom glyph rasterization for text effects that cosmic-text's built-in
//! pipeline doesn't support: continuous faux-bold and faux-condense on any
//! static font (no variable-font axis required), via swash's `embolden`
//! and `transform` applied directly during rasterization.
//!
//! Shaping and layout (BiDi, kerning, line-breaking) still come from
//! cosmic-text's `Buffer`/`layout_runs` — only the rasterization step is
//! replaced, through glyphon's `custom_glyph` extension point. cosmic-text's
//! own built-in `SwashCache` never calls `embolden`/`transform` for regular
//! text (verified against its source), so getting either effect means
//! driving swash ourselves, the same way cosmic-text's own (private)
//! `swash.rs` module does internally, just with those two calls added.

use std::collections::HashMap;

use glyphon::{
    Buffer, Color, ContentType, CustomGlyph, CustomGlyphId, FontSystem, LayoutGlyph,
    RasterizeCustomGlyphRequest, RasterizedCustomGlyph, fontdb,
};
use swash::scale::{Render, ScaleContext, Source, StrikeWith, image::Content, image::Image};
use swash::zeno::{Format, Transform};

/// The synthetic per-glyph effect parameters carried by
/// [`super::FontParameters`], in the form the placement/measurement code
/// consumes: one value per *span* (or per whole buffer, for `draw_text`),
/// applied to each of that span's glyphs.
#[derive(Clone, Copy, Debug)]
pub struct GlyphEffect {
    /// Faux-bold outline dilation in pixels; `0.0` = none.
    pub embolden: f32,
    /// Faux condense/expand ratio; `1.0` = none.
    pub condense: f32,
    /// Tracking in EM units, mirroring `FontParameters::tracking` (and
    /// cosmic-text's `letter_spacing`, which is where it's actually
    /// applied). Carried here because glyph *placement* needs to know it —
    /// see the tracking-recentering note in
    /// [`EffectGlyphs::layout_to_custom_glyphs_with`].
    pub tracking: f32,
    /// Faux-italic shear (mirrors `FontParameters::slant`); `0.0` = none.
    pub slant: f32,
}

impl GlyphEffect {
    pub const NEUTRAL: Self = Self {
        embolden: 0.0,
        condense: 1.0,
        tracking: 0.0,
        slant: 0.0,
    };
}

impl From<&super::FontParameters> for GlyphEffect {
    fn from(parameters: &super::FontParameters) -> Self {
        Self {
            embolden: parameters.weight,
            condense: parameters.width,
            tracking: parameters.tracking,
            slant: parameters.slant,
        }
    }
}

/// glyphon identifies a custom glyph by a `u16` ([`CustomGlyphId`]), so at
/// most this many distinct glyph variants can be live at once.
const MAX_GLYPH_IDS: usize = u16::MAX as usize;

/// Once this many IDs are handed out the owner is asked to [`reset`]
/// (see [`EffectGlyphs::needs_reset`]) before the next preparation, leaving
/// headroom for the variants a single preparation can add.
///
/// [`reset`]: EffectGlyphs::reset
const RESET_AT_IDS: usize = MAX_GLYPH_IDS - 4096;

/// Ceiling on rasterized bitmap memory before a reset is requested. Glyph
/// bitmaps are cheap to regenerate, so the cache is a working set, not an
/// archive.
const BITMAP_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// Identifies one distinct (font, glyph, size, weight, condense, slant)
/// combination. Floats are stored as bits so the key can derive `Eq`/`Hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct EffectGlyphKey {
    font_id: fontdb::ID,
    glyph_id: u16,
    size_bits: u32,
    embolden_bits: u32,
    condense_bits: u32,
    slant_bits: u32,
}

/// Rasterizes text through swash directly instead of cosmic-text's built-in
/// `SwashCache`, so `embolden` (faux bold) and `transform` (faux condense)
/// can be applied on any font.
///
/// A map keyed by [`EffectGlyphKey`] holds each rasterized bitmap and the
/// `u16` ID glyphon knows it by. Both are bounded together: past
/// [`RESET_AT_IDS`] IDs or [`BITMAP_BUDGET_BYTES`] of bitmaps,
/// [`EffectGlyphs::needs_reset`] turns true and the owner must call
/// [`EffectGlyphs::reset`] **together with discarding the glyph atlas**,
/// because the atlas is keyed by these IDs and a recycled ID would alias an
/// old bitmap. Doing it as one coordinated step is what makes reuse safe.
pub struct EffectGlyphs {
    scale_context: ScaleContext,
    cache: HashMap<EffectGlyphKey, Image>,
    ids: HashMap<EffectGlyphKey, CustomGlyphId>,
    keys_by_id: HashMap<CustomGlyphId, EffectGlyphKey>,
    next_id: usize,
    bitmap_bytes: usize,
}

impl Default for EffectGlyphs {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectGlyphs {
    pub fn new() -> Self {
        Self {
            scale_context: ScaleContext::new(),
            cache: HashMap::new(),
            ids: HashMap::new(),
            keys_by_id: HashMap::new(),
            next_id: 0,
            bitmap_bytes: 0,
        }
    }

    /// Whether the ID space or bitmap budget is close enough to exhausted
    /// that the owner should [`reset`](Self::reset) before preparing more
    /// text.
    pub fn needs_reset(&self) -> bool {
        self.next_id >= RESET_AT_IDS || self.bitmap_bytes > BITMAP_BUDGET_BYTES
    }

    /// Forget every bitmap and ID. Only sound when the glyph atlas holding
    /// the old IDs' bitmaps is discarded in the same step, and every layer
    /// re-prepares its text afterwards.
    pub fn reset(&mut self) {
        self.cache.clear();
        self.ids.clear();
        self.keys_by_id.clear();
        self.next_id = 0;
        self.bitmap_bytes = 0;
    }

    #[cfg(test)]
    pub(super) fn set_next_id_for_test(&mut self, next_id: usize) {
        self.next_id = next_id;
    }

    #[cfg(test)]
    pub(super) fn next_id_for_test(&self) -> usize {
        self.next_id
    }

    /// Convert a shaped `Buffer`'s glyphs into `CustomGlyph` entries, with
    /// `embolden`/`condense` applied uniformly to every glyph. `left`/`top`
    /// place the buffer's own origin on screen, matching how a normal
    /// `TextArea` positions text.
    ///
    /// Rasterizes any not-yet-seen (font, glyph, size, weight, condense, slant)
    /// combination immediately, rather than lazily inside the rasterize
    /// callback — glyphon requires a `CustomGlyph`'s exact pixel width/height
    /// to be declared up front, before it asks for the bitmap, and that size
    /// isn't known until swash has actually rendered the glyph.
    pub fn layout_to_custom_glyphs(
        &mut self,
        font_system: &mut FontSystem,
        buffer: &Buffer,
        position: (f32, f32),
        effect: GlyphEffect,
        color: [u8; 4],
    ) -> Vec<CustomGlyph> {
        let uniform_color = Color::rgba(color[0], color[1], color[2], color[3]);
        self.layout_to_custom_glyphs_with(font_system, buffer, position, true, |_glyph| {
            (effect, uniform_color)
        })
    }

    /// The rectangle (`[min_x, min_y, max_x, max_y]`) every glyph of
    /// `buffer` inks, for ordering decisions made before text is prepared.
    /// Rasterizes into the same cache [`Self::layout_to_custom_glyphs`]
    /// reads, but assigns no atlas IDs, so it never moves the ID space
    /// towards a reset. `None` when nothing is drawn.
    pub fn ink_bounds(
        &mut self,
        font_system: &mut FontSystem,
        buffer: &Buffer,
        position: (f32, f32),
        span_effects: Option<&[GlyphEffect]>,
        uniform_effect: GlyphEffect,
    ) -> Option<[f32; 4]> {
        let glyphs =
            self.layout_to_custom_glyphs_with(font_system, buffer, position, false, |glyph| {
                let effect = span_effects.map_or(uniform_effect, |effects| {
                    effects
                        .get(glyph.metadata)
                        .copied()
                        .unwrap_or(GlyphEffect::NEUTRAL)
                });
                (effect, Color::rgba(0xFF, 0xFF, 0xFF, 0xFF))
            });
        let mut bounds: Option<[f32; 4]> = None;
        for glyph in &glyphs {
            let rect = [
                glyph.left,
                glyph.top,
                glyph.left + glyph.width,
                glyph.top + glyph.height,
            ];
            bounds = Some(bounds.map_or(rect, |b| {
                [
                    b[0].min(rect[0]),
                    b[1].min(rect[1]),
                    b[2].max(rect[2]),
                    b[3].max(rect[3]),
                ]
            }));
        }
        bounds
    }

    /// Per-span variant, for `Layer::draw_styled_text`: every span in a
    /// call is shaped together as one cosmic-text rich-text buffer (see
    /// `TextStack::shape_spans`), so unlike [`Self::layout_to_custom_glyphs`]
    /// there's no single effect/color for the whole buffer — each glyph
    /// recovers its own span's `(weight, width)` via `glyph.metadata`, an
    /// index into `span_effects` (cosmic-text has no native per-span
    /// concept for this synthetic effect, so it rides through shaping via
    /// the generic `metadata` field `shape_spans` tags every span with).
    /// Color, unlike weight/width, *is* native to cosmic-text — it comes
    /// back already resolved per glyph via `glyph.color_opt`.
    pub fn layout_to_custom_glyphs_spans(
        &mut self,
        font_system: &mut FontSystem,
        buffer: &Buffer,
        position: (f32, f32),
        span_effects: &[GlyphEffect],
    ) -> Vec<CustomGlyph> {
        self.layout_to_custom_glyphs_with(font_system, buffer, position, true, |glyph| {
            let effect = span_effects
                .get(glyph.metadata)
                .copied()
                .unwrap_or(GlyphEffect::NEUTRAL);
            let color = glyph
                .color_opt
                .unwrap_or(Color::rgba(0xFF, 0xFF, 0xFF, 0xFF));
            (effect, color)
        })
    }

    /// Shared core of [`Self::layout_to_custom_glyphs`]/
    /// [`Self::layout_to_custom_glyphs_spans`]: walks every glyph in
    /// `buffer`, asking `effect_and_color` for that glyph's
    /// `(embolden, condense)`/color, then rasterizes (cached by
    /// `EffectGlyphKey`) and emits a `CustomGlyph`.
    fn layout_to_custom_glyphs_with(
        &mut self,
        font_system: &mut FontSystem,
        buffer: &Buffer,
        position: (f32, f32),
        allocate_ids: bool,
        mut effect_and_color: impl FnMut(&LayoutGlyph) -> (GlyphEffect, Color),
    ) -> Vec<CustomGlyph> {
        let (left, top) = position;
        let mut glyphs = Vec::new();
        for run in buffer.layout_runs() {
            // `weight`/`width` are rasterization-time effects cosmic-text's
            // shaper never sees, so it reserves each glyph only its
            // *unmodified* advance. The pen corrections here mirror what
            // FreeType documents for its own faux bold
            // (`FT_Outline_Embolden`): dilation pushes ink outward by
            // `embolden` px on *every* side, so each emboldened glyph is
            // offset right by `embolden` (re-centering the ink in its
            // slot) and its advance grows by `2 * embolden`; faux-condense
            // scales ink toward the glyph origin by `condense`, so its
            // advance scales by `condense` too (a compensating-only-one-
            // side `+embolden` was tried first: bold still clipped its
            // left neighbor, and condensed spans still gapped).
            // `pen_delta` accumulates corrected-minus-natural advances —
            // pure per-glyph arithmetic from the shaped `glyph.w`, never
            // inference from rasterized bitmap edges (also tried: normal
            // antialiased glyphs routinely overhang their advance, which
            // compounded into runaway gaps on plain text). Measurement
            // applies these same deltas — see
            // `text_stack::measure_shaped_buffer_with`. Reset per run,
            // since each run is an independent visual line.
            let mut pen_delta: f32 = 0.0;
            for glyph in run.glyphs {
                let (effect, color) = effect_and_color(glyph);
                let embolden = effect.embolden.max(0.0);
                let condense = effect.condense;
                let physical = glyph.physical((left, top + run.line_y), 1.0);
                // cosmic-text applies letter-spacing CSS-style: the whole
                // track is appended *after* each glyph's advance (see its
                // shape.rs), which makes a tracked span cram flush against
                // whatever precedes it while leaving a full orphan track
                // dangling after its last glyph. Shifting every tracked
                // glyph right by half a track recenters it: interior gaps
                // are untouched (all glyphs shift equally), and each span
                // boundary ends up with half a track instead of
                // zero-then-full — how professional layout engines treat
                // tracking (space *between* letters, not after them).
                // Scaled by `condense` because the track rides inside
                // `glyph.w`, which `pen_delta` condenses along with
                // everything else.
                let track_shift = 0.5 * effect.tracking * glyph.font_size * condense;
                // Updated before the whitespace `continue`s below — a
                // space inside a condensed/bold span adjusts the pen the
                // same way an inked glyph does.
                let render_x = physical.x as f32 + pen_delta + embolden + track_shift;
                pen_delta += effect_advance_delta(embolden, condense, glyph.w);

                let key = EffectGlyphKey {
                    font_id: physical.cache_key.font_id,
                    glyph_id: physical.cache_key.glyph_id,
                    size_bits: physical.cache_key.font_size_bits,
                    embolden_bits: embolden.to_bits(),
                    condense_bits: condense.to_bits(),
                    slant_bits: effect.slant.to_bits(),
                };

                if !self.cache.contains_key(&key)
                    && let Some(image) = rasterize_glyph(
                        &mut self.scale_context,
                        font_system,
                        key,
                        physical.cache_key.font_weight,
                        embolden,
                        condense,
                        effect.slant,
                    )
                {
                    self.bitmap_bytes += image.data.len();
                    self.cache.insert(key, image);
                }
                let Some(image) = self.cache.get(&key) else {
                    continue; // no outline for this glyph (e.g. whitespace)
                };
                if image.placement.width == 0 || image.placement.height == 0 {
                    continue;
                }

                let id = if !allocate_ids {
                    0 // placeholder: the caller reads only the geometry
                } else if let Some(&id) = self.ids.get(&key) {
                    id
                } else {
                    // Unreachable in practice: `needs_reset` fires well
                    // before. If one preparation still outgrows the
                    // headroom, drop the glyph rather than alias an ID.
                    let Ok(id) = CustomGlyphId::try_from(self.next_id) else {
                        continue;
                    };
                    self.next_id += 1;
                    self.ids.insert(key, id);
                    self.keys_by_id.insert(id, key);
                    id
                };

                // A color glyph (bitmap or COLR emoji) already carries its
                // own full RGBA — glyphon's shader ignores `CustomGlyph`'s
                // `color` entirely for `ContentType::Color` content (see
                // `rasterize`'s doc), so `None` here just documents that
                // rather than changing behavior.
                let glyph_color = if image.content == Content::Color {
                    None
                } else {
                    Some(color)
                };

                glyphs.push(CustomGlyph {
                    id,
                    left: render_x + image.placement.left as f32,
                    top: physical.y as f32 - image.placement.top as f32,
                    width: image.placement.width as f32,
                    height: image.placement.height as f32,
                    color: glyph_color,
                    snap_to_physical_pixel: true,
                    metadata: 0,
                });
            }
        }
        glyphs
    }

    /// The `rasterize_custom_glyph` callback for
    /// `TextRenderer::prepare_with_custom`. Looks up the image already
    /// rendered by [`Self::layout_to_custom_glyphs`] — no swash work happens
    /// here, only a cache read.
    pub fn rasterize(&self, request: RasterizeCustomGlyphRequest) -> Option<RasterizedCustomGlyph> {
        let key = *self.keys_by_id.get(&request.id)?;
        let image = self.cache.get(&key)?;
        // `Content::Color` (bitmap/COLR emoji) carries full RGBA and is
        // drawn as-is; `Mask`/`SubpixelMask` (plain outline glyphs — always
        // `Mask` here, since rasterization always requests `Format::Alpha`)
        // carry single-channel coverage, tinted by the text's own color —
        // see glyphon's `shader.wgsl` `fs_main` for exactly how each
        // `content_type` is interpreted.
        let content_type = match image.content {
            Content::Color => ContentType::Color,
            Content::Mask | Content::SubpixelMask => ContentType::Mask,
        };
        Some(RasterizedCustomGlyph {
            data: image.data.clone(),
            content_type,
        })
    }
}

/// How much wider (or narrower, for condense) a glyph's advance becomes
/// once its synthetic effects are applied, relative to the `natural_advance`
/// cosmic-text shaped it with: `2 * embolden` for faux bold (ink dilates by
/// `embolden` px on each side — FreeType's documented advance correction
/// for its equivalent `FT_Outline_Embolden`), and the advance scales by
/// `condense` for faux condense/expand, exactly as the ink does. Used by
/// both glyph placement ([`EffectGlyphs::layout_to_custom_glyphs_with`]'s
/// `pen_delta`) and text measurement
/// (`text_stack::measure_shaped_buffer_with`) so rendered and measured
/// widths always agree. `embolden` is expected pre-clamped to `>= 0.0`.
pub fn effect_advance_delta(embolden: f32, condense: f32, natural_advance: f32) -> f32 {
    (condense - 1.0) * natural_advance + 2.0 * embolden
}

/// The swash [`Transform`] combining faux-condense (horizontal scale) with
/// faux-italic (horizontal shear): `x' = condense * x + slant * y`, `y' = y`.
/// Font outlines are y-up with the baseline at 0, so a positive `slant`
/// shears upper parts to the right, the italic direction. `None` when both
/// are neutral, which keeps untouched outlines on swash's fast path.
fn effect_transform(condense: f32, slant: f32) -> Option<Transform> {
    (condense != 1.0 || slant != 0.0).then(|| Transform::new(condense, 0.0, slant, 1.0, 0.0, 0.0))
}

/// Rasterize one glyph via swash directly, applying `embolden` (continuous
/// faux-bold outline expansion) and, if `condense != 1.0`, a horizontal-scale
/// `transform` (faux condense/expand). Mirrors the recipe cosmic-text's own
/// private `swash.rs` uses internally, minus the two calls it never makes.
fn rasterize_glyph(
    scale_context: &mut ScaleContext,
    font_system: &mut FontSystem,
    key: EffectGlyphKey,
    weight: fontdb::Weight,
    embolden: f32,
    condense: f32,
    slant: f32,
) -> Option<Image> {
    let font = font_system.get_font(key.font_id, weight)?;
    let size = f32::from_bits(key.size_bits);
    let mut scaler = scale_context
        .builder(font.as_swash())
        .size(size)
        .hint(true)
        .build();
    let transform = effect_transform(condense, slant);
    // Color sources first (embedded-bitmap emoji, then COLR layered-outline
    // emoji), falling back to the plain monochrome outline every other
    // glyph uses. Each source is a no-op fallthrough when the font doesn't
    // have it (`has_color_bitmaps`/`has_color_outlines`/`has_outlines`,
    // checked internally by swash's `render_into`) — so this is exactly
    // the previous behavior for every non-emoji glyph. `embolden`/
    // `transform` only apply where swash's own `render_into` applies them
    // (`Outline` unconditionally, `ColorOutline` for `transform` only) —
    // harmlessly ignored for the embedded-bitmap source, which has no
    // outline to dilate/skew in the first place.
    Render::new(&[
        Source::ColorBitmap(StrikeWith::BestFit),
        Source::ColorOutline(0),
        Source::Outline,
    ])
    .format(Format::Alpha)
    .embolden(embolden)
    .transform(transform)
    .render(&mut scaler, key.glyph_id)
}
