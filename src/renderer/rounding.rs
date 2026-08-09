//! [`Rounding`] — per-corner rectangle corner radii, in pixels.

/// Corner radii for `draw_rectangle`, one value per corner so a rectangle
/// can be rounded on some corners and sharp on others. Maps directly onto
/// lyon's [`lyon::path::builder::BorderRadii`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rounding {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

impl Rounding {
    /// Sharp rectangle — every corner radius is zero.
    pub const NONE: Self = Self {
        top_left: 0.0,
        top_right: 0.0,
        bottom_right: 0.0,
        bottom_left: 0.0,
    };

    /// The same radius on all four corners.
    pub const fn uniform(radius: f32) -> Self {
        Self {
            top_left: radius,
            top_right: radius,
            bottom_right: radius,
            bottom_left: radius,
        }
    }

    /// `true` if every corner radius is zero (a plain sharp rectangle can
    /// skip lyon's rounded-rect tessellation entirely).
    pub fn is_none(&self) -> bool {
        self.top_left == 0.0
            && self.top_right == 0.0
            && self.bottom_right == 0.0
            && self.bottom_left == 0.0
    }

    /// Every radius multiplied by `factor` — used to convert a caller's
    /// logical-pixel [`Rounding`] to physical pixels at the DPI scale
    /// factor in effect when `draw_rectangle` was called.
    pub(super) fn scaled(&self, factor: f32) -> Self {
        Self {
            top_left: self.top_left * factor,
            top_right: self.top_right * factor,
            bottom_right: self.bottom_right * factor,
            bottom_left: self.bottom_left * factor,
        }
    }
}

impl Default for Rounding {
    fn default() -> Self {
        Self::NONE
    }
}
