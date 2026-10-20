//! [`FontParameters`] — continuous size/weight/width/tracking/slant knobs
//! for `draw_text`.

/// Continuous text-shaping parameters, independent of which [`super::Font`]
/// is used.
///
/// `weight` and `width` are synthetic (faux) effects applied at
/// rasterization time via `glyph_effects::EffectGlyphs` — they work on any
/// static font, not just ones with variable-font axes. `tracking` is a real
/// cosmic-text feature (`Attrs::letter_spacing`), not synthetic.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontParameters {
    /// Font size in physical pixels.
    pub size: f32,
    /// Faux-bold strength: swash `embolden` outline expansion. `0.0` is
    /// normal weight; larger values are progressively bolder. Continuous,
    /// not a discrete set of weight steps.
    pub weight: f32,
    /// Faux condense/expand ratio: a horizontal scale applied to each
    /// glyph's outline before rasterizing. `1.0` is normal width; `< 1.0`
    /// condenses, `> 1.0` expands. A geometric approximation, not a true
    /// condensed cut of the typeface.
    pub width: f32,
    /// Extra letter-spacing in EM units. `0.0` is normal; positive values
    /// spread letters apart.
    pub tracking: f32,
    /// Faux-italic shear applied to each glyph's outline before
    /// rasterizing. `0.0` is upright; positive values shear right with
    /// height (the italic direction), `0.2` being a typical oblique. A
    /// geometric approximation, not a true italic cut of the typeface.
    pub slant: f32,
}

impl FontParameters {
    /// Normal weight, normal width, no extra tracking, at the given pixel
    /// size.
    pub const fn new(size: f32) -> Self {
        Self {
            size,
            weight: 0.0,
            width: 1.0,
            tracking: 0.0,
            slant: 0.0,
        }
    }

    /// Convert a caller's logical-pixel `size`/`weight` to physical pixels
    /// at the given DPI scale factor. `width` (a ratio), `tracking` (EM
    /// units, relative to `size`) and `slant` (a ratio) are already scale-independent and left
    /// untouched.
    pub(super) fn scaled(&self, factor: f32) -> Self {
        Self {
            size: self.size * factor,
            weight: self.weight * factor,
            width: self.width,
            tracking: self.tracking,
            slant: self.slant,
        }
    }
}
