//! The optional paint pass and text leaves.
//!
//! Layout stays geometry; this module draws what a node was told to look
//! like through the ordinary `Layer` calls, in tree order (parents under
//! children, earlier siblings under later ones), and turns
//! `overflow: hidden` into `Layer::with_clip`.

use taffy::style::Display;

use super::{Layout, Measurer, NodeId, Rect, Space};
use crate::renderer::{
    Alignment, ClipShape, Color, FillRule, Font, FontParameters, Layer, PathPaint, Rounding,
};

/// How a node looks. Everything is optional; a node with no `Paint` costs
/// one `None`.
#[derive(Clone, Debug, PartialEq)]
pub struct Paint {
    pub background: Option<Color>,
    /// Border width in logical pixels and colour. Drawn inside the box;
    /// the node's `border` style is not set by this, so reserve the space
    /// with padding.
    pub border: Option<(f32, Color)>,
    /// Corner radius in logical pixels.
    pub radius: f32,
}

impl Paint {
    pub fn background(color: Color) -> Self {
        Self {
            background: Some(color),
            border: None,
            radius: 0.0,
        }
    }

    pub fn radius(mut self, radius: f32) -> Self {
        self.radius = radius;
        self
    }

    pub fn border(mut self, width: f32, color: Color) -> Self {
        self.border = Some((width, color));
        self
    }
}

fn rounding(radius: f32) -> Rounding {
    Rounding {
        top_left: radius,
        top_right: radius,
        bottom_right: radius,
        bottom_left: radius,
    }
}

/// A run of text to lay out and draw. Single style; styled spans arrive
/// with wrapped styled text in the renderer.
#[derive(Clone, Debug)]
pub struct Text {
    pub text: String,
    pub font: Font,
    pub params: FontParameters,
    pub color: Color,
    pub wrap: bool,
}

impl Text {
    pub fn new(text: impl Into<String>, font: Font, params: FontParameters, color: Color) -> Self {
        Self {
            text: text.into(),
            font,
            params,
            color,
            wrap: true,
        }
    }

    /// One line, however wide: the node is as wide as the text.
    pub fn no_wrap(mut self) -> Self {
        self.wrap = false;
        self
    }
}

/// An SVG path for a rounded rectangle in absolute logical pixels.
fn rounded_rect_path(r: Rect, radius: f32) -> String {
    let (x, y, w, h) = (r.x, r.y, r.width, r.height);
    let rad = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    if rad < 0.01 {
        return format!("M{x} {y} H{} V{} H{x} Z", x + w, y + h);
    }
    format!(
        "M{} {y} H{} A{rad} {rad} 0 0 1 {} {} V{} A{rad} {rad} 0 0 1 {} {} H{} A{rad} {rad} 0 0 1 {x} {} V{} A{rad} {rad} 0 0 1 {} {y} Z",
        x + rad,
        x + w - rad,
        x + w,
        y + rad,
        y + h - rad,
        x + w - rad,
        y + h,
        x + rad,
        y + h - rad,
        y + rad,
        x + rad
    )
}

const MEMO: usize = 4;
const UNWRAPPED: u32 = u32::MAX;
const MINIMUM: u32 = u32::MAX - 1;

/// A text leaf plus a tiny memo of recent measurements. The renderer
/// already caches shaped buffers, but hashing the string and looking up
/// per probe is not free, and a solve probes min-, max- and definite sizes.
/// The memo keys on width quantised to the physical pixel and stores only
/// sizes, so it holds no buffers.
pub(super) struct TextLeaf {
    text: Text,
    memo: [Option<(u32, (f32, f32))>; MEMO],
    next: usize,
    min_width: Option<f32>,
}

impl TextLeaf {
    pub(super) fn new(text: Text) -> Self {
        Self {
            text,
            memo: [None; MEMO],
            next: 0,
            min_width: None,
        }
    }

    pub(super) fn forget(&mut self) {
        self.memo = [None; MEMO];
        self.next = 0;
        self.min_width = None;
    }

    pub(super) fn set_text(&mut self, text: &str) -> bool {
        if self.text.text == text {
            return false;
        }
        self.text.text.clear();
        self.text.text.push_str(text);
        self.forget();
        true
    }

    /// Makes the leaf show `text` in `font`/`params`/`color`/`wrap`,
    /// comparing first so an unchanged description allocates and
    /// invalidates nothing. Returns whether measurement must re-run.
    pub(super) fn describe(
        &mut self,
        text: &str,
        font: &Font,
        params: &FontParameters,
        color: &Color,
        wrap: bool,
    ) -> (bool, bool) {
        let mut remeasure = self.set_text(text);
        if self.text.font != *font || self.text.params != *params || self.text.wrap != wrap {
            self.text.font = font.clone();
            self.text.params = *params;
            self.text.wrap = wrap;
            self.forget();
            remeasure = true;
        }
        let recolor = self.text.color != *color;
        if recolor {
            self.text.color = color.clone();
        }
        (remeasure, recolor)
    }

    pub(super) fn set_font(&mut self, font: Font, params: FontParameters) -> bool {
        if self.text.font == font && self.text.params == params {
            return false;
        }
        self.text.font = font;
        self.text.params = params;
        self.forget();
        true
    }

    fn lookup(&mut self, key: u32, compute: impl FnOnce(&Text) -> (f32, f32)) -> (f32, f32) {
        if let Some((_, size)) = self.memo.iter().flatten().find(|(k, _)| *k == key) {
            return *size;
        }
        let size = compute(&self.text);
        self.memo[self.next] = Some((key, size));
        self.next = (self.next + 1) % MEMO;
        size
    }

    /// Size for the solver: `known.0` is a width it already decided, else
    /// `available` says what to wrap to.
    pub(super) fn measure(
        &mut self,
        m: &dyn Measurer,
        scale: f32,
        known: (Option<f32>, Option<f32>),
        available: Space,
    ) -> (f32, f32) {
        if !self.text.wrap {
            return self.lookup(UNWRAPPED, |t| m.text_size(&t.text, &t.font, &t.params, None));
        }
        // `floor` to the physical pixel: wrapping at a slightly narrower
        // width than offered can never overflow it.
        let quantise = |w: f32| (w * scale).floor().max(0.0);
        let width = match (known.0, available) {
            (Some(w), _) | (None, Space::Definite(w)) => Some(quantise(w)),
            (None, Space::MinContent) => None,
            (None, Space::MaxContent) => return self.lookup(UNWRAPPED, |t| {
                m.text_size(&t.text, &t.font, &t.params, None)
            }),
        };
        match width {
            Some(w) => self.lookup(w as u32, |t| {
                m.text_size(&t.text, &t.font, &t.params, Some(w / scale))
            }),
            None => {
                let min = match self.min_width {
                    Some(min) => min,
                    None => {
                        let min = m.min_content_width(
                            &self.text.text,
                            &self.text.font,
                            &self.text.params,
                        );
                        self.min_width = Some(min);
                        min
                    }
                };
                // One word per line at the narrowest width.
                self.lookup(MINIMUM, |t| {
                    m.text_size(&t.text, &t.font, &t.params, Some(min.max(1.0 / scale)))
                })
            }
        }
    }
}

impl Layout {
    /// Sets (or clears) how `id` looks when painted.
    pub fn set_paint(&mut self, id: NodeId, paint: Option<Paint>) {
        let slot = &mut self.entry_mut(id).paint;
        if slot.as_deref() != paint.as_ref() {
            match (slot.as_deref_mut(), paint) {
                // Assign in place: no allocation when a paint changes.
                (Some(old), Some(new)) => *old = new,
                (_, new) => *slot = new.map(Box::new),
            }
            self.repaint = true;
        }
    }

    /// Draws every node in tree order into `layer`: backgrounds and
    /// borders, text leaves, children clipped by `overflow: hidden`
    /// ancestors. Hidden nodes are skipped. Call between `Layer::clear` and
    /// the end of the frame, after [`Layout::compute`].
    pub fn paint(&self, layer: &Layer) {
        self.paint_node(self.root, layer);
    }

    fn paint_node(&self, index: u32, layer: &Layer) {
        let e = &self.entries[index as usize];
        if self
            .tree
            .style(e.node)
            .map(|s| s.display == Display::None)
            .unwrap_or(true)
        {
            return;
        }
        let draw = |layer: &Layer| {
            self.paint_own(index, layer);
            for child in &e.children {
                self.paint_node(*child, layer);
            }
        };
        if e.clip {
            let r = e.rect;
            let clip = ClipShape::Rectangle {
                top_left: r.position(),
                size: r.size(),
                rounding: e
                    .paint
                    .as_ref()
                    .map_or(Rounding::NONE, |p| rounding(p.radius)),
            };
            // `with_clip` only fails on an unparsable path; a rectangle
            // cannot.
            let _ = layer.with_clip(clip, draw);
        } else {
            draw(layer);
        }
    }

    fn paint_own(&self, index: u32, layer: &Layer) {
        let e = &self.entries[index as usize];
        let r = e.rect;
        if let Some(p) = &e.paint {
            if let Some(bg) = &p.background {
                layer.draw_rectangle(r.position(), r.size(), bg.clone(), rounding(p.radius));
            }
            if let Some((width, color)) = &p.border
                && *width > 0.0
            {
                // A ring (outer minus inner, even-odd), so a translucent
                // background or border is never painted over itself.
                let inner = r.inset(*width);
                let d = format!(
                    "{} {}",
                    rounded_rect_path(r, p.radius),
                    rounded_rect_path(inner, (p.radius - width).max(0.0))
                );
                let _ = layer.draw_path(
                    &d,
                    (0.0, 0.0),
                    PathPaint::Fill {
                        color: color.clone(),
                        rule: FillRule::EvenOdd,
                    },
                );
            }
        }
        if let Some(super::Leaf::Text(leaf)) = self.tree.get_node_context(e.node) {
            let l = self.tree.layout(e.node).expect("node exists");
            let inset_left = l.padding.left + l.border.left;
            let inset_right = l.padding.right + l.border.right;
            let inset_top = l.padding.top + l.border.top;
            let content = Rect::new(
                r.x + inset_left,
                r.y + inset_top,
                (r.width - inset_left - inset_right).max(0.0),
                0.0,
            );
            let t = &leaf.text;
            if t.wrap {
                // A physical pixel of slack: the box was sized from text
                // wrapped at a width floored to the pixel, so wrapping at
                // the snapped width must never come out narrower.
                let slack = 1.0 / self.scale;
                layer.draw_wrapped_text(
                    &t.text,
                    content.position(),
                    content.width + slack,
                    t.color.clone(),
                    Alignment::default(),
                    t.font.clone(),
                    t.params,
                );
            } else {
                layer.draw_text(
                    &t.text,
                    content.position(),
                    t.color.clone(),
                    Alignment::default(),
                    t.font.clone(),
                    t.params,
                );
            }
        }
    }
}
