//! [`ShaderEffect`] — a post-process effect applied to a whole
//! [`super::Layer`] at composite time (see `Layer::set_effect`). "Whole
//! layer" is the practical unit of shader application here rather than
//! per-`draw_*`-call: every layer already renders into its own offscreen
//! texture and is composited as one quad (see `layer.rs`'s module doc,
//! which calls out this exact seam), so shading a specific rectangle/path/
//! text run means giving *that* content its own layer and shading it —
//! the same unit real compositors use for filter effects (e.g. a
//! `CALayer`'s `filters` in AppKit/UIKit).
//!
//! [`ColorMatrix`] and [`ShaderEffect::Blur`] are built-in, GPU-cheap
//! effects with hand-written WGSL. [`ShaderEffect::Custom`] is the
//! caller-WGSL hook: a full WGSL module compiled against a fixed contract
//! (see its own doc) — validated at `set_effect` time via a wgpu error
//! scope, so a bad shader logs and leaves the layer unaffected rather than
//! panicking the whole renderer.

use super::Color;

/// A post-process effect a [`super::Layer`] can have applied to its whole
/// composited output.
#[derive(Clone, Debug, PartialEq)]
pub enum ShaderEffect {
    /// Separable Gaussian blur. `radius` is in logical pixels (scaled to
    /// physical internally, same convention as every `Layer::draw_*` size
    /// — see `Layer::set_effect`), and is the standard deviation-ish
    /// falloff distance, not a hard cutoff — larger values cost more
    /// (more texture taps), capped internally so a runaway value can't
    /// tank frame time.
    Blur { radius: f32 },
    /// A 4x5 color transform applied to every pixel — see [`ColorMatrix`].
    ColorMatrix(ColorMatrix),
    /// A caller-supplied WGSL module, compiled against a fixed contract:
    ///
    /// ```wgsl
    /// struct VsOut {
    ///     @builtin(position) clip_pos: vec4<f32>,
    ///     @location(0) uv: vec2<f32>,
    /// };
    ///
    /// @vertex
    /// fn vs_main(
    ///     @location(0) in_pos: vec2<f32>,
    ///     @location(1) in_uv: vec2<f32>,
    /// ) -> VsOut {
    ///     var out: VsOut;
    ///     out.clip_pos = vec4<f32>(in_pos, 0.0, 1.0);
    ///     out.uv = in_uv;
    ///     return out;
    /// }
    ///
    /// @group(0) @binding(0) var tex: texture_2d<f32>;
    /// @group(0) @binding(1) var samp: sampler;
    ///
    /// @fragment
    /// fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    ///     return textureSampleLevel(tex, samp, in.uv, 0.0); // <- replace with your effect
    /// }
    /// ```
    ///
    /// Copy that module verbatim and edit only `fs_main`'s body — the
    /// vertex stage and bindings must match exactly, since that's the
    /// contract the renderer's pipeline layout expects. Compile/validation
    /// failure (bad WGSL, wrong entry point names, mismatched bindings) is
    /// caught at `set_effect` time and logged; the layer keeps rendering
    /// without the effect rather than panicking.
    Custom(&'static str),
}

/// A 4x5 color transform: each output channel is a weighted sum of the
/// input `r, g, b, a` plus a constant offset — the same model as SVG's
/// `feColorMatrix`/Android's `ColorMatrix`/CSS's color-affecting filters
/// (several of which are provided as named constructors below, using their
/// spec-defined formulas). Row-major: `values[0..5]` produce the output
/// red channel (`r*values[0] + g*values[1] + b*values[2] + a*values[3] +
/// values[4]`), `values[5..10]` produce green, `[10..15]` blue, `[15..20]`
/// alpha.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorMatrix(pub [f32; 20]);

impl ColorMatrix {
    /// No change.
    pub const IDENTITY: Self = Self([
        1.0, 0.0, 0.0, 0.0, 0.0, //
        0.0, 1.0, 0.0, 0.0, 0.0, //
        0.0, 0.0, 1.0, 0.0, 0.0, //
        0.0, 0.0, 0.0, 1.0, 0.0,
    ]);

    /// Rec. 709 luma-weighted grayscale (alpha untouched).
    pub const fn grayscale() -> Self {
        const R: f32 = 0.2126;
        const G: f32 = 0.7152;
        const B: f32 = 0.0722;
        Self([
            R, G, B, 0.0, 0.0, //
            R, G, B, 0.0, 0.0, //
            R, G, B, 0.0, 0.0, //
            0.0, 0.0, 0.0, 1.0, 0.0,
        ])
    }

    /// Photographic negative (alpha untouched).
    pub const fn invert() -> Self {
        Self([
            -1.0, 0.0, 0.0, 0.0, 1.0, //
            0.0, -1.0, 0.0, 0.0, 1.0, //
            0.0, 0.0, -1.0, 0.0, 1.0, //
            0.0, 0.0, 0.0, 1.0, 0.0,
        ])
    }

    /// Classic sepia tone (CSS Filter Effects spec's `sepia(100%)` matrix).
    pub const fn sepia() -> Self {
        Self([
            0.393, 0.769, 0.189, 0.0, 0.0, //
            0.349, 0.686, 0.168, 0.0, 0.0, //
            0.272, 0.534, 0.131, 0.0, 0.0, //
            0.0, 0.0, 0.0, 1.0, 0.0,
        ])
    }

    /// Saturation adjustment — `amount` of `0.0` is fully grayscale, `1.0`
    /// is unchanged, `> 1.0` oversaturates. The SVG `feColorMatrix
    /// type="saturate"` formula.
    pub fn saturate(amount: f32) -> Self {
        let s = amount;
        Self([
            0.213 + 0.787 * s,
            0.715 - 0.715 * s,
            0.072 - 0.072 * s,
            0.0,
            0.0, //
            0.213 - 0.213 * s,
            0.715 + 0.285 * s,
            0.072 - 0.072 * s,
            0.0,
            0.0, //
            0.213 - 0.213 * s,
            0.715 - 0.715 * s,
            0.072 + 0.928 * s,
            0.0,
            0.0, //
            0.0,
            0.0,
            0.0,
            1.0,
            0.0,
        ])
    }

    /// Brightness adjustment — `amount` of `1.0` is unchanged, `0.0` is
    /// black, `> 1.0` brightens. CSS `filter: brightness()`'s formula (a
    /// pure multiplicative scale).
    pub fn brightness(amount: f32) -> Self {
        Self([
            amount, 0.0, 0.0, 0.0, 0.0, //
            0.0, amount, 0.0, 0.0, 0.0, //
            0.0, 0.0, amount, 0.0, 0.0, //
            0.0, 0.0, 0.0, 1.0, 0.0,
        ])
    }

    /// Contrast adjustment — `amount` of `1.0` is unchanged, `0.0` is flat
    /// mid-gray, `> 1.0` increases contrast. CSS `filter: contrast()`'s
    /// formula (`(input - 0.5) * amount + 0.5` per channel).
    pub fn contrast(amount: f32) -> Self {
        let offset = 0.5 * (1.0 - amount);
        Self([
            amount, 0.0, 0.0, 0.0, offset, //
            0.0, amount, 0.0, 0.0, offset, //
            0.0, 0.0, amount, 0.0, offset, //
            0.0, 0.0, 0.0, 1.0, 0.0,
        ])
    }

    /// Uniformly tint toward `color` by `amount` (`0.0` = untouched, `1.0`
    /// = flat `color`) — a linear blend baked into matrix form (each output
    /// channel is `(1 - amount) * input + amount * color`).
    pub fn tint(color: Color, amount: f32) -> Self {
        let Color::Solid(rgba) = color else {
            debug_assert!(false, "ColorMatrix::tint only supports Color::Solid");
            return Self::IDENTITY;
        };
        let keep = 1.0 - amount;
        let target = [
            rgba[0] as f32 / 255.0,
            rgba[1] as f32 / 255.0,
            rgba[2] as f32 / 255.0,
        ];
        Self([
            keep,
            0.0,
            0.0,
            0.0,
            target[0] * amount, //
            0.0,
            keep,
            0.0,
            0.0,
            target[1] * amount, //
            0.0,
            0.0,
            keep,
            0.0,
            target[2] * amount, //
            0.0,
            0.0,
            0.0,
            1.0,
            0.0,
        ])
    }

    /// Transpose this matrix's row-major `[f32; 20]` into 5 column vectors
    /// (one per input component R/G/B/A plus one for the constant offset)
    /// — the layout the GPU side actually uses. WGSL's uniform address
    /// space pads every element of an `array<f32, N>` to a 16-byte stride,
    /// which would balloon (and misalign) a naive flat upload; `array<vec4
    /// <f32>, 5>` has no such padding (`vec4` is already 16 bytes), and
    /// `output = Σ input[i] * column[i]` is the same weighted-sum identity
    /// as the row-major matrix multiply, just regrouped.
    pub(super) fn to_columns(self) -> [[f32; 4]; 5] {
        let m = self.0;
        let mut cols = [[0.0f32; 4]; 5];
        for (col, slot) in cols.iter_mut().enumerate() {
            *slot = [m[col], m[5 + col], m[10 + col], m[15 + col]];
        }
        cols
    }
}
