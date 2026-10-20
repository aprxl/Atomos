//! [`PathPaint`] — how a [`super::Layer::draw_path`] shape is painted:
//! filled, stroked, or both, mirroring the vocabulary SVG's `fill`/
//! `fill-rule`/`stroke`/`stroke-width`/`stroke-linecap`/`stroke-linejoin`
//! attributes use. Renderer-owned types, not lyon's — same reasoning as
//! [`super::Rounding`] mirroring `lyon::path::builder::BorderRadii`: the
//! tessellation library's vocabulary stays internal to `layer.rs`.

use super::Color;

/// Which rule decides "inside" for overlapping/self-intersecting subpaths —
/// see the SVG `fill-rule` property. Only matters for paths with holes or
/// self-overlap (a plain convex shape looks identical either way).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum FillRule {
    /// A point is inside if the path winds around it a non-zero number of
    /// times. The common default; makes a figure-eight-style
    /// self-overlapping path fully solid.
    #[default]
    NonZero,
    /// A point is inside if a ray to infinity crosses the path an odd
    /// number of times — alternating winding creates "holes" wherever
    /// subpaths overlap (e.g. a donut from two same-direction circles).
    EvenOdd,
}

/// Cap style at the start/end of an open (sub)path — see the SVG
/// `stroke-linecap` property.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum LineCap {
    /// Stroke stops exactly at the endpoint — no extension.
    #[default]
    Butt,
    /// A half-circle of radius `width / 2` extends past the endpoint.
    Round,
    /// A half-square of side `width / 2` extends past the endpoint.
    Square,
}

/// Join style where two segments of a stroked path meet — see the SVG
/// `stroke-linejoin` property.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum LineJoin {
    /// Segments extend to a sharp point, up to `miter_limit` — beyond that
    /// the join falls back to `Bevel` (matches SVG's defined behavior).
    #[default]
    Miter,
    /// A circular arc of radius `width / 2` fills the join.
    Round,
    /// The join is squared off with a flat edge connecting the two
    /// segments' outer corners.
    Bevel,
}

/// Stroke styling for [`PathPaint::Stroke`]/[`PathPaint::FillAndStroke`].
#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    pub color: Color,
    /// Stroke width in pixels, centered on the path outline.
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    /// How far a [`LineJoin::Miter`] point may extend (as a multiple of
    /// `width`) before falling back to a bevel — see the SVG
    /// `stroke-miterlimit` property. Must be `>= 1.0`.
    pub miter_limit: f32,
}

impl Stroke {
    /// A solid stroke of `width` pixels in `color`, with SVG's own
    /// defaults for everything else (butt caps, miter joins, miter limit
    /// 4.0 — [`lyon_tessellation::StrokeOptions::DEFAULT_MITER_LIMIT`]).
    pub fn new(color: Color, width: f32) -> Self {
        Self {
            color,
            width,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 4.0,
        }
    }
}

/// How a [`super::Layer::draw_path`] shape is painted. Only
/// [`Color::Solid`]/[`Color::Gradient`] are accepted for any color here —
/// same restriction as [`super::Layer::draw_circle`], and for the same
/// reason (a path's vertices come from tessellating `d`, not from the
/// caller, so there's nothing to pin a [`Color::PerVertex`] corner to).
#[derive(Clone, Debug, PartialEq)]
pub enum PathPaint {
    Fill {
        color: Color,
        rule: FillRule,
    },
    Stroke(Stroke),
    FillAndStroke {
        fill_color: Color,
        fill_rule: FillRule,
        stroke: Stroke,
    },
}

impl PathPaint {
    /// Shorthand for [`PathPaint::Fill`] with [`FillRule::NonZero`] — the
    /// common case, and what every `draw_path` call used before stroking
    /// existed.
    pub fn fill(color: Color) -> Self {
        Self::Fill {
            color,
            rule: FillRule::NonZero,
        }
    }
}
