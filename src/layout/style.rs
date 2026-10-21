//! [`Style`] — the CSS-named, builder-style description of one element.
//!
//! This is an Atomos type that converts to the solver's own style, so
//! callers never name the backend and it can be swapped without breaking
//! them: `Style::new().width(px(80.0)).grow(1.0)`.

use crate::renderer::Color;
use taffy::style::{
    AlignContent, AlignItems, AlignSelf, Dimension, Display, FlexDirection, FlexWrap,
    JustifyContent, LengthPercentage, LengthPercentageAuto, Overflow as Flow, Position,
};

/// A length: logical pixels, a fraction of the parent, or content-sized.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Len {
    /// Logical pixels.
    Px(f32),
    /// Fraction of the parent's size, `0.0..=1.0` (so `0.5` is 50%).
    Pct(f32),
    /// Sized by content, or by the available space for stretching items.
    Auto,
}

/// Logical pixels. `width(px(80.0))`.
pub const fn px(value: f32) -> Len {
    Len::Px(value)
}

/// A fraction of the parent, `0.0..=1.0`. `width(pct(0.5))` is half.
pub const fn pct(fraction: f32) -> Len {
    Len::Pct(fraction)
}

/// Content-sized.
pub const fn auto() -> Len {
    Len::Auto
}

impl From<f32> for Len {
    fn from(value: f32) -> Self {
        Len::Px(value)
    }
}

impl Len {
    fn dimension(self) -> Dimension {
        match self {
            Len::Px(v) => Dimension::length(v),
            Len::Pct(v) => Dimension::percent(v),
            Len::Auto => Dimension::auto(),
        }
    }

    fn lpa(self) -> LengthPercentageAuto {
        match self {
            Len::Px(v) => LengthPercentageAuto::length(v),
            Len::Pct(v) => LengthPercentageAuto::percent(v),
            Len::Auto => LengthPercentageAuto::auto(),
        }
    }

    /// `Auto` has no meaning for padding or gap; it reads as zero.
    fn lp(self) -> LengthPercentage {
        match self {
            Len::Px(v) => LengthPercentage::length(v),
            Len::Pct(v) => LengthPercentage::percent(v),
            Len::Auto => LengthPercentage::length(0.0),
        }
    }
}

/// Where children go along the main axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Justify {
    Start,
    End,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

/// Where children go on the cross axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Start,
    End,
    Center,
    Stretch,
    Baseline,
}

impl Align {
    fn items(self) -> AlignItems {
        match self {
            Align::Start => AlignItems::FLEX_START,
            Align::End => AlignItems::FLEX_END,
            Align::Center => AlignItems::CENTER,
            Align::Stretch => AlignItems::STRETCH,
            Align::Baseline => AlignItems::BASELINE,
        }
    }

    fn own(self) -> AlignSelf {
        self.items()
    }
}

/// What happens to content that overflows an element's box.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overflow {
    /// Painted outside the box.
    #[default]
    Visible,
    /// Clipped to the box by [`Layout::paint`](super::Layout::paint).
    Hidden,
}

/// Everything the solver knows about one element. The default is a
/// column that stacks its children (like a `div`), stretches them across,
/// and is `auto`-sized.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    pub(super) inner: taffy::Style,
    pub(super) clip: bool,
    /// How the box looks. Not part of what the solver sees: the builder
    /// splits it off (`Style::el`) so a colour change never re-solves.
    pub(super) look: Look,
    /// Identity for hit-testing and for the builder's reconcile. `0` = none.
    pub(super) id: u64,
}

/// The look half of a [`Style`]: what paints, not what lays out.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Look {
    pub(super) background: Option<Color>,
    pub(super) border: Option<(f32, Color)>,
    pub(super) radius: f32,
}

impl Default for Style {
    fn default() -> Self {
        let inner = taffy::Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Column,
            ..taffy::Style::default()
        };
        Self {
            inner,
            clip: false,
            look: Look::default(),
            id: 0,
        }
    }
}

macro_rules! sides {
    ($(#[$doc:meta])* $all:ident, $x:ident, $y:ident, $top:ident, $right:ident,
     $bottom:ident, $left:ident, $field:ident, $conv:ident) => {
        $(#[$doc])*
        pub fn $all(mut self, value: impl Into<Len>) -> Self {
            let v = value.into().$conv();
            self.inner.$field.left = v;
            self.inner.$field.right = v;
            self.inner.$field.top = v;
            self.inner.$field.bottom = v;
            self
        }
        pub fn $x(mut self, value: impl Into<Len>) -> Self {
            let v = value.into().$conv();
            self.inner.$field.left = v;
            self.inner.$field.right = v;
            self
        }
        pub fn $y(mut self, value: impl Into<Len>) -> Self {
            let v = value.into().$conv();
            self.inner.$field.top = v;
            self.inner.$field.bottom = v;
            self
        }
        pub fn $top(mut self, value: impl Into<Len>) -> Self {
            self.inner.$field.top = value.into().$conv();
            self
        }
        pub fn $right(mut self, value: impl Into<Len>) -> Self {
            self.inner.$field.right = value.into().$conv();
            self
        }
        pub fn $bottom(mut self, value: impl Into<Len>) -> Self {
            self.inner.$field.bottom = value.into().$conv();
            self
        }
        pub fn $left(mut self, value: impl Into<Len>) -> Self {
            self.inner.$field.left = value.into().$conv();
            self
        }
    };
}

impl Style {
    /// A column that stacks its children, like a `div`.
    pub fn new() -> Self {
        Self::default()
    }

    /// A column. Same as [`Style::new`], spelled out.
    pub fn column() -> Self {
        Self::default()
    }

    /// Fill colour. Applied by the builder; see [`Style::el`].
    pub fn bg(mut self, color: Color) -> Self {
        self.look.background = Some(color);
        self
    }

    /// Corner radius in logical pixels.
    pub fn radius(mut self, radius: f32) -> Self {
        self.look.radius = radius;
        self
    }

    /// A border of `width` logical pixels in `color`. Unlike
    /// [`Paint::border`](super::Paint::border) it also takes `width` of
    /// space in layout on every side, so the box model matches the picture.
    pub fn border(mut self, width: f32, color: Color) -> Self {
        let w = LengthPercentage::length(width.max(0.0));
        self.inner.border = taffy::Rect {
            left: w,
            right: w,
            top: w,
            bottom: w,
        };
        self.look.border = Some((width, color));
        self
    }

    /// Identity: events, `Layout::rect_of` and the builder's reconcile
    /// find the element by it. Keep it unique among siblings.
    pub fn id(mut self, id: u64) -> Self {
        self.id = id;
        self
    }

    /// A row.
    pub fn row() -> Self {
        let mut s = Self::default();
        s.inner.flex_direction = FlexDirection::Row;
        s
    }

    /// Lays children out in reverse order along the main axis.
    pub fn reverse(mut self) -> Self {
        self.inner.flex_direction = match self.inner.flex_direction {
            FlexDirection::Row | FlexDirection::RowReverse => FlexDirection::RowReverse,
            _ => FlexDirection::ColumnReverse,
        };
        self
    }

    /// Lets children wrap onto further lines.
    pub fn wrap(mut self) -> Self {
        self.inner.flex_wrap = FlexWrap::Wrap;
        self
    }

    /// A hidden element takes no space and is skipped by the solver; it
    /// keeps whatever rect it last had.
    pub fn hidden(mut self, hidden: bool) -> Self {
        self.inner.display = if hidden { Display::None } else { Display::Flex };
        self
    }

    pub fn width(mut self, value: impl Into<Len>) -> Self {
        self.inner.size.width = value.into().dimension();
        self
    }

    pub fn height(mut self, value: impl Into<Len>) -> Self {
        self.inner.size.height = value.into().dimension();
        self
    }

    /// `width` and `height` together.
    pub fn size(self, width: impl Into<Len>, height: impl Into<Len>) -> Self {
        self.width(width).height(height)
    }

    pub fn min_width(mut self, value: impl Into<Len>) -> Self {
        self.inner.min_size.width = value.into().lpa();
        self
    }

    pub fn min_height(mut self, value: impl Into<Len>) -> Self {
        self.inner.min_size.height = value.into().lpa();
        self
    }

    pub fn max_width(mut self, value: impl Into<Len>) -> Self {
        self.inner.max_size.width = value.into().lpa();
        self
    }

    pub fn max_height(mut self, value: impl Into<Len>) -> Self {
        self.inner.max_size.height = value.into().lpa();
        self
    }

    /// Width over height, applied when one of the two is auto.
    pub fn aspect_ratio(mut self, ratio: f32) -> Self {
        self.inner.aspect_ratio = Some(ratio);
        self
    }

    sides!(
        /// Space inside the box.
        padding, padding_x, padding_y, padding_top, padding_right, padding_bottom, padding_left,
        padding, lp
    );
    sides!(
        /// Space outside the box; `auto` margins absorb free space.
        margin, margin_x, margin_y, margin_top, margin_right, margin_bottom, margin_left,
        margin, lpa
    );
    sides!(
        /// Offsets of an [`absolute`](Style::absolute) element from its parent's edges.
        inset, inset_x, inset_y, top, right, bottom, left,
        inset, lpa
    );

    /// Space between children, on both axes.
    pub fn gap(mut self, value: impl Into<Len>) -> Self {
        let v = value.into().lp();
        self.inner.gap.width = v;
        self.inner.gap.height = v;
        self
    }

    /// Share of leftover main-axis space, by weight, on top of the item's
    /// own basis (its `width`/`height`/`basis`, else its content). CSS
    /// `flex-grow`. For "split the whole line by weight" use
    /// [`Style::flex`].
    pub fn grow(mut self, weight: f32) -> Self {
        self.inner.flex_grow = weight;
        self
    }

    /// CSS `flex: weight`: grow by `weight` from a zero basis, so
    /// siblings split the *whole* line in proportion to their weights
    /// regardless of content. Overrides any basis set earlier.
    pub fn flex(mut self, weight: f32) -> Self {
        self.inner.flex_grow = weight;
        self.inner.flex_basis = Dimension::length(0.0);
        self
    }

    /// Willingness to give up space when children overflow; `0.0` is rigid.
    pub fn shrink(mut self, weight: f32) -> Self {
        self.inner.flex_shrink = weight;
        self
    }

    pub fn basis(mut self, value: impl Into<Len>) -> Self {
        self.inner.flex_basis = value.into().dimension();
        self
    }

    /// An exact width: size, minimum and maximum all `extent`, so it never
    /// grows, shrinks, or is floored by content, whichever way the parent
    /// lays out. Touches the width only.
    pub fn fixed_width(mut self, extent: f32) -> Self {
        self.inner.size.width = Dimension::length(extent);
        self.inner.min_size.width = LengthPercentageAuto::length(extent);
        self.inner.max_size.width = LengthPercentageAuto::length(extent);
        self
    }

    /// [`Style::fixed_width`] for the height.
    pub fn fixed_height(mut self, extent: f32) -> Self {
        self.inner.size.height = Dimension::length(extent);
        self.inner.min_size.height = LengthPercentageAuto::length(extent);
        self.inner.max_size.height = LengthPercentageAuto::length(extent);
        self
    }

    pub fn justify(mut self, how: Justify) -> Self {
        self.inner.justify_content = match how {
            Justify::Start => JustifyContent::FLEX_START,
            Justify::End => JustifyContent::FLEX_END,
            Justify::Center => JustifyContent::CENTER,
            Justify::SpaceBetween => JustifyContent::SPACE_BETWEEN,
            Justify::SpaceAround => JustifyContent::SPACE_AROUND,
            Justify::SpaceEvenly => JustifyContent::SPACE_EVENLY,
        };
        self
    }

    pub fn items(mut self, how: Align) -> Self {
        self.inner.align_items = how.items();
        self
    }

    pub fn items_center(self) -> Self {
        self.items(Align::Center)
    }

    /// This element's own cross-axis alignment, overriding its parent's.
    pub fn align_self(mut self, how: Align) -> Self {
        self.inner.align_self = Some(how.own());
        self
    }

    /// How wrapped lines distribute cross-axis space.
    pub fn align_content(mut self, how: Justify) -> Self {
        self.inner.align_content = match how {
            Justify::Start => AlignContent::FLEX_START,
            Justify::End => AlignContent::FLEX_END,
            Justify::Center => AlignContent::CENTER,
            Justify::SpaceBetween => AlignContent::SPACE_BETWEEN,
            Justify::SpaceAround => AlignContent::SPACE_AROUND,
            Justify::SpaceEvenly => AlignContent::SPACE_EVENLY,
        };
        self
    }

    /// Takes the element out of flow; it is placed with
    /// [`inset`](Style::inset) and friends relative to its parent. For
    /// popups and overlays.
    pub fn absolute(mut self) -> Self {
        self.inner.position = Position::Absolute;
        self
    }

    /// With `Hidden`, content past the box is clipped when painted.
    pub fn overflow(mut self, how: Overflow) -> Self {
        let o = match how {
            Overflow::Visible => Flow::Visible,
            Overflow::Hidden => Flow::Hidden,
        };
        self.inner.overflow = taffy::Point { x: o, y: o };
        self.clip = how == Overflow::Hidden;
        self
    }
}
