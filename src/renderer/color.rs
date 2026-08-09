//! [`Color`] — how a drawable's fill is colored: one flat color, a two-stop
//! gradient, or an explicit color per vertex.

/// Straight (non-premultiplied) 8-bit-per-channel RGBA.
pub type Rgba = [u8; 4];

/// Which screen axis a [`Color::Gradient`] is interpolated along.
// Not constructed anywhere yet — every current caller (the layer spike)
// uses `Color::Solid`; `Gradient`/`PerVertex` are exercised once
// `draw_polygon`/`draw_path`/`draw_image` land.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GradientDirection {
    /// Left edge of the shape's bounding box gets `stops[0]`, right edge
    /// gets `stops[1]`.
    Horizontal,
    /// Top edge of the shape's bounding box gets `stops[0]`, bottom edge
    /// gets `stops[1]`.
    Vertical,
}

/// A drawable's fill color. Every draw method accepts a `Color`; some
/// restrict which variants they accept (documented on the method itself) —
/// e.g. `draw_circle` and `draw_text` have no caller-supplied vertices to
/// pin colors to, so they only accept [`Color::Solid`]/[`Color::Gradient`].
///
/// Resolution to a per-vertex color happens *after* tessellation, purely as
/// a function of each output vertex's `(x, y)` position relative to the
/// shape's axis-aligned bounding box:
/// - [`Color::Solid`] ignores position, every vertex gets the same color.
/// - [`Color::Gradient`] linearly interpolates `stops` along the chosen
///   axis of the bounding box.
/// - [`Color::PerVertex`] bilinearly blends the four bounding-box corner
///   colors (`[top_left, top_right, bottom_right, bottom_left]`).
///
/// Resolving by position instead of by original-vertex-index means a shape
/// that gets extra vertices during tessellation (e.g. a rounded rectangle's
/// arcs) still shades smoothly, with no dependency on lyon preserving
/// vertex identity.
#[derive(Clone, Debug, PartialEq)]
pub enum Color {
    /// One flat color for the entire shape.
    Solid(Rgba),
    /// A two-stop linear gradient across the shape's bounding box. Not
    /// constructed anywhere yet — see [`GradientDirection`]'s note.
    #[allow(dead_code)]
    Gradient {
        stops: [Rgba; 2],
        direction: GradientDirection,
    },
    /// Explicit `[top_left, top_right, bottom_right, bottom_left]` corner
    /// colors, bilinearly blended across the shape's bounding box. Not
    /// constructed anywhere yet — see [`GradientDirection`]'s note.
    #[allow(dead_code)]
    PerVertex([Rgba; 4]),
}

impl Color {
    /// Shorthand for [`Color::Solid`] from separate `r, g, b, a` channels.
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self::Solid([r, g, b, a])
    }

    /// Shorthand for [`Color::Solid`] with full alpha.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::Solid([r, g, b, 0xFF])
    }

    pub const fn gradient(first: Rgba, second: Rgba, direction: GradientDirection) -> Self {
        Self::Gradient {
            stops: [first, second],
            direction,
        }
    }

    /// Resolve this color at a single point, given the axis-aligned
    /// bounding box (`min`, `max`) of the shape being colored. See the type
    /// doc for how each variant behaves; `pos` is expected to lie within
    /// `[min, max]` (points from a shape's own tessellation always do) but
    /// is clamped defensively so a slightly-out-of-range point (tessellator
    /// rounding) can't produce a wildly wrong color.
    pub fn resolve(&self, pos: [f32; 2], min: [f32; 2], max: [f32; 2]) -> [f32; 4] {
        let unit = |c: Rgba| {
            [
                c[0] as f32 / 255.0,
                c[1] as f32 / 255.0,
                c[2] as f32 / 255.0,
                c[3] as f32 / 255.0,
            ]
        };
        let lerp4 = |a: [f32; 4], b: [f32; 4], t: f32| {
            [
                a[0] + (b[0] - a[0]) * t,
                a[1] + (b[1] - a[1]) * t,
                a[2] + (b[2] - a[2]) * t,
                a[3] + (b[3] - a[3]) * t,
            ]
        };
        let axis_t = |p: f32, lo: f32, hi: f32| {
            if hi > lo {
                ((p - lo) / (hi - lo)).clamp(0.0, 1.0)
            } else {
                0.0
            }
        };

        match self {
            Color::Solid(c) => unit(*c),
            Color::Gradient { stops, direction } => {
                let t = match direction {
                    GradientDirection::Horizontal => axis_t(pos[0], min[0], max[0]),
                    GradientDirection::Vertical => axis_t(pos[1], min[1], max[1]),
                };
                lerp4(unit(stops[0]), unit(stops[1]), t)
            }
            Color::PerVertex([top_left, top_right, bottom_right, bottom_left]) => {
                let tx = axis_t(pos[0], min[0], max[0]);
                let ty = axis_t(pos[1], min[1], max[1]);
                let top = lerp4(unit(*top_left), unit(*top_right), tx);
                let bottom = lerp4(unit(*bottom_left), unit(*bottom_right), tx);
                lerp4(top, bottom, ty)
            }
        }
    }

    /// Hash this color's contents. Not a `Hash` impl because `Rgba` is a
    /// plain `[u8; 4]` (fine to derive `Hash` on its own) but `Color` as a
    /// whole has no floats to worry about *yet* — kept as an explicit
    /// method (rather than `#[derive(Hash)]`) so it reads the same way as
    /// `DrawCommand`'s manual impl, which does need to worry about floats.
    pub(super) fn hash_bits<H: std::hash::Hasher>(&self, state: &mut H) {
        use std::hash::Hash;
        match self {
            Color::Solid(rgba) => {
                state.write_u8(0);
                rgba.hash(state);
            }
            Color::Gradient { stops, direction } => {
                state.write_u8(1);
                stops[0].hash(state);
                stops[1].hash(state);
                state.write_u8(*direction as u8);
            }
            Color::PerVertex(corners) => {
                state.write_u8(2);
                for c in corners {
                    c.hash(state);
                }
            }
        }
    }
}
