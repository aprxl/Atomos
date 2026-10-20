//! [`TextSpan`] — one uniformly-styled run of text within a
//! [`super::Layer::draw_styled_text`] call.

use super::{Color, Font, FontParameters};

/// One contiguous run of text sharing a single font/color/size/weight/
/// width — e.g. one syntax-highlighted token. Every span passed to
/// [`super::Layer::draw_styled_text`] together is shaped as a single
/// cosmic-text rich-text buffer, so a highlighted line with a dozen
/// spans still costs one shape operation, not a dozen — see
/// `text_stack.rs`'s `shape_spans` for how.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSpan {
    pub text: String,
    pub font: Font,
    pub font_parameters: FontParameters,
    /// Only [`Color::Solid`] is accepted, same restriction as
    /// [`super::Layer::draw_text`].
    pub color: Color,
}

impl TextSpan {
    pub fn new(
        text: impl Into<String>,
        font: Font,
        font_parameters: FontParameters,
        color: Color,
    ) -> Self {
        Self {
            text: text.into(),
            font,
            font_parameters,
            color,
        }
    }
}
