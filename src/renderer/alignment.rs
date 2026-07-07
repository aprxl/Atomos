//! [`Alignment`] — which point of a drawable's own bounding box a caller's
//! position anchors to. Used by `draw_text` so a caller can e.g. center a
//! label on a point instead of always anchoring its top-left.

/// Horizontal anchor within a bounding box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum HorizontalAlign {
    #[default]
    Left,
    Center,
    #[allow(dead_code)] // not exercised by the current demo
    Right,
}

/// Vertical anchor within a bounding box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum VerticalAlign {
    #[default]
    Top,
    Center,
    #[allow(dead_code)] // not exercised by the current demo
    Bottom,
}

/// Combined horizontal/vertical anchor. A caller's `position` is this point
/// of the drawable's own bounding box — e.g. `Alignment::CENTER` means
/// `position` is the center of the text block, not its top-left corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Alignment {
    pub horizontal: HorizontalAlign,
    pub vertical: VerticalAlign,
}

impl Alignment {
    pub const TOP_LEFT: Self = Self {
        horizontal: HorizontalAlign::Left,
        vertical: VerticalAlign::Top,
    };
    pub const CENTER: Self = Self {
        horizontal: HorizontalAlign::Center,
        vertical: VerticalAlign::Center,
    };
}
