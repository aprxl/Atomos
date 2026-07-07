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
    fontdb, Buffer, Color, ContentType, CustomGlyph, CustomGlyphId, FontSystem,
    RasterizeCustomGlyphRequest, RasterizedCustomGlyph,
};
use swash::scale::{image::Image, Render, ScaleContext, Source};
use swash::zeno::{Format, Transform};

/// Identifies one distinct (font, glyph, size, weight, condense)
/// combination. Floats are stored as bits so the key can derive `Eq`/`Hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct EffectGlyphKey {
    font_id: fontdb::ID,
    glyph_id: u16,
    size_bits: u32,
    embolden_bits: u32,
    condense_bits: u32,
}

/// Rasterizes text through swash directly instead of cosmic-text's built-in
/// `SwashCache`, so `embolden` (faux bold) and `transform` (faux condense)
/// can be applied on any font. A simple map keyed by [`EffectGlyphKey`] is
/// the honest scale for a Phase 0 spike (a handful of demo lines) — no
/// eviction; real documents with thousands of glyphs are a later problem.
pub struct EffectGlyphs {
    scale_context: ScaleContext,
    cache: HashMap<EffectGlyphKey, Image>,
    ids: HashMap<EffectGlyphKey, CustomGlyphId>,
    keys_by_id: HashMap<CustomGlyphId, EffectGlyphKey>,
    next_id: u16,
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
        }
    }

    /// Convert a shaped `Buffer`'s glyphs into `CustomGlyph` entries, with
    /// `embolden`/`condense` applied uniformly to every glyph. `left`/`top`
    /// place the buffer's own origin on screen, matching how a normal
    /// `TextArea` positions text.
    ///
    /// Rasterizes any not-yet-seen (font, glyph, size, weight, condense)
    /// combination immediately, rather than lazily inside the rasterize
    /// callback — glyphon requires a `CustomGlyph`'s exact pixel width/height
    /// to be declared up front, before it asks for the bitmap, and that size
    /// isn't known until swash has actually rendered the glyph.
    pub fn layout_to_custom_glyphs(
        &mut self,
        font_system: &mut FontSystem,
        buffer: &Buffer,
        position: (f32, f32),
        effect: (f32, f32), // (embolden, condense)
        color: [u8; 4],
    ) -> Vec<CustomGlyph> {
        let (left, top) = position;
        let (embolden, condense) = effect;
        let mut glyphs = Vec::new();
        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical = glyph.physical((left, top + run.line_y), 1.0);
                let key = EffectGlyphKey {
                    font_id: physical.cache_key.font_id,
                    glyph_id: physical.cache_key.glyph_id,
                    size_bits: physical.cache_key.font_size_bits,
                    embolden_bits: embolden.to_bits(),
                    condense_bits: condense.to_bits(),
                };

                if !self.cache.contains_key(&key)
                    && let Some(image) = rasterize_glyph(
                        &mut self.scale_context,
                        font_system,
                        key,
                        physical.cache_key.font_weight,
                        embolden,
                        condense,
                    )
                {
                    self.cache.insert(key, image);
                }
                let Some(image) = self.cache.get(&key) else {
                    continue; // no outline for this glyph (e.g. whitespace)
                };
                if image.placement.width == 0 || image.placement.height == 0 {
                    continue;
                }

                let id = if let Some(&id) = self.ids.get(&key) {
                    id
                } else {
                    let id: CustomGlyphId = self.next_id;
                    self.next_id += 1;
                    self.ids.insert(key, id);
                    self.keys_by_id.insert(id, key);
                    id
                };

                glyphs.push(CustomGlyph {
                    id,
                    left: physical.x as f32 + image.placement.left as f32,
                    top: physical.y as f32 - image.placement.top as f32,
                    width: image.placement.width as f32,
                    height: image.placement.height as f32,
                    color: Some(Color::rgba(color[0], color[1], color[2], color[3])),
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
        Some(RasterizedCustomGlyph {
            data: image.data.clone(),
            content_type: ContentType::Mask,
        })
    }
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
) -> Option<Image> {
    let font = font_system.get_font(key.font_id, weight)?;
    let size = f32::from_bits(key.size_bits);
    let mut scaler = scale_context.builder(font.as_swash()).size(size).hint(true).build();
    let transform = if condense != 1.0 {
        Some(Transform::scale(condense, 1.0))
    } else {
        None
    };
    Render::new(&[Source::Outline])
        .format(Format::Alpha)
        .embolden(embolden)
        .transform(transform)
        .render(&mut scaler, key.glyph_id)
}
