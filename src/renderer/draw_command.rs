//! [`DrawCommand`] — a pending draw call a [`super::Layer`] hasn't built
//! into GPU geometry yet. Internal to the renderer: callers only see
//! `Layer::draw_*` methods, never this enum directly.

use std::hash::{Hash, Hasher};

use super::Color;
use super::Pixels;
use super::Rounding;
use super::TextSpan;
use super::path_paint::{PathPaint, Stroke};
use super::{Alignment, Font, FontParameters};

/// One `Layer::draw_*` call, queued until the layer is next (re)built.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum DrawCommand {
    Rectangle {
        top_left: [f32; 2],
        size: [f32; 2],
        color: Color,
        rounding: Rounding,
    },
    /// Vertices in order, implicitly closed (the last connects back to the
    /// first).
    Polygon {
        vertices: Vec<[f32; 2]>,
        color: Color,
    },
    Circle {
        center: [f32; 2],
        radius: f32,
        color: Color,
    },
    /// `d` is kept as the original SVG path-data string (not a pre-parsed
    /// geometry) so this variant stays trivially `Hash`/`PartialEq`/`Clone`
    /// — it's re-parsed on every rebuild, same cost model as re-tessellating
    /// every other variant on every rebuild. `scale` is the DPI scale
    /// factor at `draw_path` call time (baked in here, rather than scaling
    /// `d`'s parsed coordinates up front, for the same "re-derive from the
    /// source string every rebuild" reason `d` itself isn't pre-parsed).
    Path {
        d: String,
        position: [f32; 2],
        scale: f32,
        paint: PathPaint,
    },
    /// Re-uploaded to the GPU on every rebuild, same as every other
    /// variant is re-tessellated — for a large, frequently-redrawn
    /// (`Automatic`) image this means hashing/re-uploading its full pixel
    /// buffer every frame it's unchanged; fine for icon-sized images, a
    /// real cost for anything bigger (a 1080p image is ~8 MB hashed per
    /// frame). Known deferred cost — revisit when images beyond icons
    /// actually appear: back `Pixels` with an `Rc` so clones share the
    /// allocation and hash by pointer identity (the same trick
    /// `Font::Bytes` uses), and cache uploaded textures across rebuilds
    /// keyed by that identity.
    Image {
        pixels: Pixels,
        top_left: [f32; 2],
        size: [f32; 2],
        tint: Color,
    },
    /// Re-shaped (and its glyphs re-prepared into the shared text atlas)
    /// every frame, regardless of whether it's actually changed — see
    /// `text_stack.rs`'s module doc for why text doesn't get the same
    /// "skip rebuild when unchanged" treatment as every other variant.
    Text {
        text: String,
        position: [f32; 2],
        font: Font,
        font_parameters: FontParameters,
        alignment: Alignment,
        color: Color,
    },
    /// Multiple independently-styled [`TextSpan`]s shaped together as one
    /// unit — see `Layer::draw_styled_text`. Same "always re-shaped every
    /// frame" cost model as `Text` above, for the same reason.
    StyledText {
        spans: Vec<TextSpan>,
        position: [f32; 2],
        alignment: Alignment,
    },
}

/// Manual `Hash`, since `f32` (in positions/sizes/colors) isn't `Hash` —
/// every float is hashed by its bit pattern instead. Used by
/// [`super::LayerInvalidation::Automatic`] layers to detect whether this
/// frame's queued content differs from last frame's without keeping the
/// previous content around to compare against directly.
impl Hash for DrawCommand {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            DrawCommand::Rectangle {
                top_left,
                size,
                color,
                rounding,
            } => {
                state.write_u8(0); // variant tag
                hash_f32_pair(top_left, state);
                hash_f32_pair(size, state);
                color.hash_bits(state);
                hash_f32_pair(&[rounding.top_left, rounding.top_right], state);
                hash_f32_pair(&[rounding.bottom_right, rounding.bottom_left], state);
            }
            DrawCommand::Polygon { vertices, color } => {
                state.write_u8(1);
                state.write_usize(vertices.len());
                for v in vertices {
                    hash_f32_pair(v, state);
                }
                color.hash_bits(state);
            }
            DrawCommand::Circle {
                center,
                radius,
                color,
            } => {
                state.write_u8(2);
                hash_f32_pair(center, state);
                state.write_u32(radius.to_bits());
                color.hash_bits(state);
            }
            DrawCommand::Path {
                d,
                position,
                scale,
                paint,
            } => {
                state.write_u8(3);
                d.hash(state);
                hash_f32_pair(position, state);
                state.write_u32(scale.to_bits());
                hash_path_paint(paint, state);
            }
            DrawCommand::Image {
                pixels,
                top_left,
                size,
                tint,
            } => {
                state.write_u8(4);
                pixels.width.hash(state);
                pixels.height.hash(state);
                pixels.data.hash(state);
                hash_f32_pair(top_left, state);
                hash_f32_pair(size, state);
                tint.hash_bits(state);
            }
            DrawCommand::Text {
                text,
                position,
                font,
                font_parameters,
                alignment,
                color,
            } => {
                state.write_u8(5);
                text.hash(state);
                hash_f32_pair(position, state);
                font.hash(state);
                state.write_u32(font_parameters.size.to_bits());
                state.write_u32(font_parameters.weight.to_bits());
                state.write_u32(font_parameters.width.to_bits());
                state.write_u32(font_parameters.tracking.to_bits());
                alignment.hash(state);
                color.hash_bits(state);
            }
            DrawCommand::StyledText {
                spans,
                position,
                alignment,
            } => {
                state.write_u8(6);
                state.write_usize(spans.len());
                for span in spans {
                    span.text.hash(state);
                    span.font.hash(state);
                    state.write_u32(span.font_parameters.size.to_bits());
                    state.write_u32(span.font_parameters.weight.to_bits());
                    state.write_u32(span.font_parameters.width.to_bits());
                    state.write_u32(span.font_parameters.tracking.to_bits());
                    span.color.hash_bits(state);
                }
                hash_f32_pair(position, state);
                alignment.hash(state);
            }
        }
    }
}

fn hash_f32_pair<H: Hasher>(pair: &[f32; 2], state: &mut H) {
    state.write_u32(pair[0].to_bits());
    state.write_u32(pair[1].to_bits());
}

fn hash_stroke<H: Hasher>(stroke: &Stroke, state: &mut H) {
    stroke.color.hash_bits(state);
    state.write_u32(stroke.width.to_bits());
    state.write_u8(stroke.cap as u8);
    state.write_u8(stroke.join as u8);
    state.write_u32(stroke.miter_limit.to_bits());
}

fn hash_path_paint<H: Hasher>(paint: &PathPaint, state: &mut H) {
    match paint {
        PathPaint::Fill { color, rule } => {
            state.write_u8(0);
            color.hash_bits(state);
            state.write_u8(*rule as u8);
        }
        PathPaint::Stroke(stroke) => {
            state.write_u8(1);
            hash_stroke(stroke, state);
        }
        PathPaint::FillAndStroke {
            fill_color,
            fill_rule,
            stroke,
        } => {
            state.write_u8(2);
            fill_color.hash_bits(state);
            state.write_u8(*fill_rule as u8);
            hash_stroke(stroke, state);
        }
    }
}
