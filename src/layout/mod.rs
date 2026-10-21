//! Flexbox layout: declare elements the way you declare `div`s, get their
//! rects, then draw, clip and hit-test from those same rects.
//!
//! ```ignore
//! use atomos::layout::*;
//!
//! let mut ui = Layout::new();
//! let bar = ui.add(ui.root(), Style::row().height(px(60.0)).items_center().padding_x(20.0).gap(12.0));
//! let logo = ui.add(bar, Style::new().size(30.0, 30.0).shrink(0.0));
//! let search = ui.add(bar, Style::new().grow(1.0).max_width(px(340.0)).height(px(36.0)));
//! let title = ui.add_text(bar, Style::new(), Text::new("Untitled", font, FontParameters::new(14.0), color));
//!
//! ui.compute(Rect::new(0.0, 0.0, 1280.0, 800.0), &layer);   // solves
//! ui.compute(Rect::new(0.0, 0.0, 1280.0, 800.0), &layer);   // no-op
//! let r = ui.rect(search);                                   // absolute, logical px
//! ```
//!
//! # Model
//!
//! - **Geometry is separate from paint.** [`Layout`] computes rects. An
//!   optional [`Paint`] per node (background, border, radius) and
//!   [`Layout::paint`] draw them through the ordinary `Layer` calls;
//!   callers that want full control just read [`Layout::rect`].
//! - **Retained, solved on demand.** [`Layout::compute`] does nothing unless
//!   a style, a text leaf or the viewport changed, and the solver caches per
//!   node, so unchanged subtrees are not re-solved.
//! - **Pixel-exact.** Rects are absolute, in logical pixels, snapped to the
//!   *physical* pixel grid by edge (adjacent siblings share an edge exactly),
//!   so an unrounded `overflow: hidden` clip is a free hardware scissor.
//! - **Moved-rect report.** [`Layout::moved`] says which nodes' rects changed
//!   in the last solve, to drive "redraw if my rect moved" rules.
//! - **Text is measured by the renderer.** A [`Measurer`] (implemented by
//!   `Layer`) is passed to `compute`; text leaves wrap to the width they are
//!   given, and the flex solver sees min-content, max-content and definite
//!   sizes.
//!
//! Semantics follow CSS flexbox with one deliberate difference: [`Style::new`]
//! is a *column* (a `div` stacks its children), not a row.

mod paint;
mod style;
#[cfg(test)]
mod title_bar_port;
#[cfg(test)]
mod tests;

use taffy::style::AvailableSpace;
use taffy::prelude::TaffyMinContent;
use taffy::{Size, TaffyTree};

use crate::renderer::{Font, FontParameters, Layer};

pub use paint::{Paint, Text};
pub use style::{Align, Justify, Len, Overflow, Style, auto, pct, px};

/// A rectangle in logical pixels — the same space every `Layer::draw_*`
/// method and `Input::mouse_position` use, so a rect is directly drawable
/// and directly hit-testable.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn position(&self) -> (f32, f32) {
        (self.x, self.y)
    }

    pub fn size(&self) -> (f32, f32) {
        (self.width, self.height)
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    pub fn contains(&self, (px, py): (f32, f32)) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    /// Shrinks by `by` on every side, clamped at zero extent.
    pub fn inset(&self, by: f32) -> Self {
        Self {
            x: self.x + by,
            y: self.y + by,
            width: (self.width - by * 2.0).max(0.0),
            height: (self.height - by * 2.0).max(0.0),
        }
    }
}

/// Measures text for the solver. `Layer` implements it; tests use a fake.
pub trait Measurer {
    /// Physical pixels per logical pixel.
    fn scale_factor(&self) -> f32 {
        1.0
    }

    /// Size of `text`, wrapped to `max_width` if given.
    fn text_size(
        &self,
        text: &str,
        font: &Font,
        params: &FontParameters,
        max_width: Option<f32>,
    ) -> (f32, f32);

    /// The narrowest the text can be without breaking inside a word: the
    /// widest word. Flex items are floored at this. The default measures
    /// each distinct word once (the result is memoised per leaf); the
    /// renderer's shape cache evicts those entries when unused.
    fn min_content_width(&self, text: &str, font: &Font, params: &FontParameters) -> f32 {
        // Non-breaking spaces do not break lines, so they do not split words.
        text.split(|c: char| c.is_whitespace() && c != '\u{a0}' && c != '\u{202f}')
            .filter(|word| !word.is_empty())
            .map(|word| self.text_size(word, font, params, None).0)
            .fold(0.0, f32::max)
    }
}

impl Measurer for Layer {
    fn scale_factor(&self) -> f32 {
        Layer::scale_factor(self)
    }

    fn text_size(
        &self,
        text: &str,
        font: &Font,
        params: &FontParameters,
        max_width: Option<f32>,
    ) -> (f32, f32) {
        match max_width {
            Some(width) => self.get_wrapped_text_size(text, width, font, params),
            None => self.get_text_size(text, font, params),
        }
    }
}

/// How much room a custom leaf is offered along one axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Space {
    Definite(f32),
    MinContent,
    MaxContent,
}

impl From<AvailableSpace> for Space {
    fn from(space: AvailableSpace) -> Self {
        match space {
            AvailableSpace::Definite(v) => Space::Definite(v),
            AvailableSpace::MinContent => Space::MinContent,
            AvailableSpace::MaxContent => Space::MaxContent,
        }
    }
}

/// What the solver asks a custom leaf (see [`Layout::add_measured`]).
#[derive(Clone, Copy, Debug)]
pub struct Constraint {
    /// A dimension the solver has already decided; answer with it.
    pub known: (Option<f32>, Option<f32>),
    pub available: (Space, Space),
}

/// A scale factor the solver can divide by; the previous one if `scale` is
/// zero, negative or not finite.
fn sane_scale(scale: f32, previous: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        previous
    }
}

/// A viewport with finite, non-negative extents (non-finite values read as
/// zero). Infinite or NaN input would otherwise produce infinite rects or
/// a solve on every call.
fn sane_rect(r: Rect) -> Rect {
    let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
    Rect::new(
        finite(r.x),
        finite(r.y),
        finite(r.width).max(0.0),
        finite(r.height).max(0.0),
    )
}

/// Handle to a node. Stale handles (to removed nodes) panic with a clear
/// message instead of silently addressing a reused slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId {
    index: u32,
    generation: u32,
}

type MeasureFn = Box<dyn FnMut(Constraint) -> (f32, f32)>;

/// What a leaf carries into the solver.
enum Leaf {
    Text(Box<paint::TextLeaf>),
    Measured(MeasureFn),
}

struct Entry {
    generation: u32,
    live: bool,
    node: taffy::NodeId,
    parent: Option<u32>,
    children: Vec<u32>,
    /// Absolute, snapped, logical pixels. Kept while hidden.
    rect: Rect,
    moved: bool,
    tag: u32,
    clip: bool,
    paint: Option<Box<Paint>>,
}

/// The tree. Built by the app, mutated freely, solved on demand.
pub struct Layout {
    tree: TaffyTree<Leaf>,
    entries: Vec<Entry>,
    free: Vec<u32>,
    root: u32,
    viewport: Rect,
    scale: f32,
    dirty: bool,
    /// `min_content` result, valid until something invalidates it.
    min_cache: Option<(f32, f32)>,
    solves: u32,
    moved_count: usize,
    stack: Vec<(u32, f32, f32)>,
    scratch: Vec<u32>,
}

impl Default for Layout {
    fn default() -> Self {
        Self::new()
    }
}

impl Layout {
    /// A tree with a root that fills the viewport. The root is a column;
    /// edit it with [`Layout::set_style`] like any other node (its size is
    /// always the viewport's).
    pub fn new() -> Self {
        let mut tree = TaffyTree::new();
        tree.disable_rounding();
        let node = tree
            .new_leaf(Style::new().inner)
            .expect("taffy allocation cannot fail");
        let root = Entry {
            generation: 0,
            live: true,
            node,
            parent: None,
            children: Vec::new(),
            rect: Rect::default(),
            moved: false,
            tag: 0,
            clip: false,
            paint: None,
        };
        Self {
            tree,
            entries: vec![root],
            free: Vec::new(),
            root: 0,
            viewport: Rect::default(),
            scale: 1.0,
            // Nothing has been solved yet, so the first `compute` must run.
            dirty: true,
            min_cache: None,
            solves: 0,
            moved_count: 0,
            stack: Vec::new(),
            scratch: Vec::new(),
        }
    }

    pub fn root(&self) -> NodeId {
        NodeId {
            index: self.root,
            generation: self.entries[self.root as usize].generation,
        }
    }

    fn entry(&self, id: NodeId) -> &Entry {
        let e = &self.entries[id.index as usize];
        assert!(
            e.live && e.generation == id.generation,
            "layout: stale NodeId (node was removed)"
        );
        e
    }

    fn entry_mut(&mut self, id: NodeId) -> &mut Entry {
        let e = &mut self.entries[id.index as usize];
        assert!(
            e.live && e.generation == id.generation,
            "layout: stale NodeId (node was removed)"
        );
        e
    }

    /// Something that affects the solve changed.
    fn invalidate(&mut self) {
        self.dirty = true;
        self.min_cache = None;
    }

    /// Text measurements and the solver's caches depend on the scale
    /// factor, so a change drops both. Every path that learns the scale
    /// goes through here.
    fn set_scale(&mut self, scale: f32) {
        if scale == self.scale {
            return;
        }
        self.scale = scale;
        for e in &self.entries {
            if e.live {
                if let Some(Leaf::Text(leaf)) = self.tree.get_node_context_mut(e.node) {
                    leaf.forget();
                }
                let _ = self.tree.mark_dirty(e.node);
            }
        }
        self.invalidate();
    }

    fn id_of(&self, index: u32) -> NodeId {
        NodeId {
            index,
            generation: self.entries[index as usize].generation,
        }
    }

    fn insert(&mut self, parent: NodeId, style: Style, leaf: Option<Leaf>) -> NodeId {
        let parent_node = self.entry(parent).node;
        assert!(
            self.tree.get_node_context(parent_node).is_none(),
            "layout: text and measured leaves cannot have children"
        );
        let node = match leaf {
            Some(leaf) => self.tree.new_leaf_with_context(style.inner, leaf),
            None => self.tree.new_leaf(style.inner),
        }
        .expect("taffy allocation cannot fail");
        self.tree
            .add_child(parent_node, node)
            .expect("parent exists");
        let entry = Entry {
            generation: 0,
            live: true,
            node,
            parent: Some(parent.index),
            children: Vec::new(),
            rect: Rect::default(),
            moved: false,
            tag: 0,
            clip: style.clip,
            paint: None,
        };
        let index = if let Some(index) = self.free.pop() {
            let generation = self.entries[index as usize].generation + 1;
            self.entries[index as usize] = Entry { generation, ..entry };
            index
        } else {
            self.entries.push(entry);
            (self.entries.len() - 1) as u32
        };
        self.entries[parent.index as usize].children.push(index);
        self.invalidate();
        self.id_of(index)
    }

    /// Adds a container (or empty box) as the last child of `parent`.
    pub fn add(&mut self, parent: NodeId, style: Style) -> NodeId {
        self.insert(parent, style, None)
    }

    /// Adds a text leaf. It wraps to the width the solver gives it (unless
    /// [`Text::no_wrap`]), and [`Layout::paint`] draws it.
    pub fn add_text(&mut self, parent: NodeId, style: Style, text: Text) -> NodeId {
        self.insert(
            parent,
            style,
            Some(Leaf::Text(Box::new(paint::TextLeaf::new(text)))),
        )
    }

    /// Adds a leaf whose size you compute (an image, a canvas, an editor
    /// surface). The closure may be called several times per solve with
    /// different constraints, so keep it cheap or cache inside it. Call
    /// [`Layout::mark_dirty`] when its answer would change.
    pub fn add_measured(
        &mut self,
        parent: NodeId,
        style: Style,
        measure: impl FnMut(Constraint) -> (f32, f32) + 'static,
    ) -> NodeId {
        self.insert(parent, style, Some(Leaf::Measured(Box::new(measure))))
    }

    /// Removes `id` and everything under it. The root cannot be removed.
    pub fn remove(&mut self, id: NodeId) {
        assert_ne!(id.index, self.root, "layout: the root cannot be removed");
        if let Some(parent) = self.entry(id).parent {
            self.entries[parent as usize]
                .children
                .retain(|c| *c != id.index);
        }
        let mut doomed = std::mem::take(&mut self.scratch);
        doomed.clear();
        doomed.push(id.index);
        let mut next = 0;
        while next < doomed.len() {
            let children = std::mem::take(&mut self.entries[doomed[next] as usize].children);
            doomed.extend(children);
            next += 1;
        }
        // Leaves first, the subtree root last (the solver detaches it from
        // its parent).
        for index in doomed.iter().rev() {
            let e = &mut self.entries[*index as usize];
            e.live = false;
            e.paint = None;
            let _ = self.tree.remove(e.node);
            self.free.push(*index);
        }
        self.scratch = doomed;
        self.invalidate();
    }

    /// Edits a node's style, marking the tree dirty **only if the edit
    /// changed something**: an animation may write the same width every
    /// frame and costs a comparison once it settles.
    ///
    /// The closure takes the current style and returns the new one, so the
    /// builder methods chain: `ui.set_style(id, |s| s.width(px(80.0)))`.
    pub fn set_style(&mut self, id: NodeId, edit: impl FnOnce(Style) -> Style) {
        let node = self.entry(id).node;
        let clip = self.entry(id).clip;
        let before = Style {
            inner: self.tree.style(node).expect("node exists").clone(),
            clip,
        };
        let mut after = edit(before.clone());
        if id.index == self.root {
            // The root's size is the viewport's; it is not the caller's.
            after.inner.size = before.inner.size;
        }
        if after != before {
            self.entry_mut(id).clip = after.clip;
            self.tree.set_style(node, after.inner).expect("node exists");
            self.invalidate();
        }
    }

    /// The node's style as last set.
    pub fn style(&self, id: NodeId) -> Style {
        let e = self.entry(id);
        Style {
            inner: self.tree.style(e.node).expect("node exists").clone(),
            clip: e.clip,
        }
    }

    /// Replaces a text leaf's string; a no-op if it is unchanged. Does
    /// nothing on a node that is not a text leaf.
    pub fn set_text(&mut self, id: NodeId, text: &str) {
        let node = self.entry(id).node;
        let changed = match self.tree.get_node_context_mut(node) {
            Some(Leaf::Text(leaf)) => leaf.set_text(text),
            _ => false,
        };
        if changed {
            let _ = self.tree.mark_dirty(node);
            self.invalidate();
        }
    }

    /// Replaces a text leaf's font parameters; a no-op if unchanged.
    pub fn set_text_style(&mut self, id: NodeId, font: Font, params: FontParameters) {
        let node = self.entry(id).node;
        let changed = match self.tree.get_node_context_mut(node) {
            Some(Leaf::Text(leaf)) => leaf.set_font(font, params),
            _ => false,
        };
        if changed {
            let _ = self.tree.mark_dirty(node);
            self.invalidate();
        }
    }

    /// Forces `id` (and its ancestors) to be re-measured next `compute`.
    pub fn mark_dirty(&mut self, id: NodeId) {
        let node = self.entry(id).node;
        let _ = self.tree.mark_dirty(node);
        self.invalidate();
    }

    /// A user tag for hit-testing: `0` means none.
    pub fn set_tag(&mut self, id: NodeId, tag: u32) {
        self.entry_mut(id).tag = tag;
    }

    pub fn tag(&self, id: NodeId) -> u32 {
        self.entry(id).tag
    }

    /// The solved rect: absolute, logical pixels, snapped to the physical
    /// pixel grid. A hidden node keeps the rect it had before it hid.
    pub fn rect(&self, id: NodeId) -> Rect {
        self.entry(id).rect
    }

    /// Did `id`'s rect change in the last solve? Meaningful right after a
    /// [`Layout::compute`] that returned `true`; a no-op `compute` clears it.
    pub fn moved(&self, id: NodeId) -> bool {
        self.entry(id).moved
    }

    /// How many rects changed in the last solve.
    pub fn moved_count(&self) -> usize {
        self.moved_count
    }

    /// Solves since creation — for tests and status lines.
    pub fn solves(&self) -> u32 {
        self.solves
    }

    /// Children of `id`, in order.
    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.entry(id).children.iter().map(|c| self.id_of(*c))
    }

    /// Number of live nodes, root included.
    pub fn len(&self) -> usize {
        self.entries.len() - self.free.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Solves the tree into `viewport`, or does nothing if neither the
    /// styles, the text, nor the viewport have changed since the last solve.
    /// Returns whether it actually solved.
    pub fn compute(&mut self, viewport: Rect, measurer: &dyn Measurer) -> bool {
        let scale = sane_scale(measurer.scale_factor(), self.scale);
        let viewport = sane_rect(viewport);
        self.set_scale(scale);
        if !self.dirty && viewport == self.viewport {
            if self.moved_count > 0 {
                // Nothing moved *this* frame; do not leave last frame's
                // flags for a "redraw if moved" poll to find again.
                self.moved_count = 0;
                for e in &mut self.entries {
                    e.moved = false;
                }
            }
            return false;
        }
        self.viewport = viewport;
        self.dirty = false;
        self.solves += 1;

        let root_node = self.entries[self.root as usize].node;
        let mut style = self.tree.style(root_node).expect("root exists").clone();
        style.size.width = taffy::style::Dimension::length(viewport.width);
        style.size.height = taffy::style::Dimension::length(viewport.height);
        if self.tree.style(root_node).map(|s| s != &style).unwrap_or(true) {
            self.tree.set_style(root_node, style).expect("root exists");
        }

        self.run(
            Size {
                width: AvailableSpace::Definite(viewport.width),
                height: AvailableSpace::Definite(viewport.height),
            },
            measurer,
        );
        self.snap(viewport);
        true
    }

    /// The smallest the whole tree can be without content overflowing: a
    /// window floor. Runs one extra min-content pass and marks the tree
    /// dirty so the next [`Layout::compute`] re-solves for the real
    /// viewport (the solver's per-node cache keeps that cheap).
    ///
    /// The answer is cached until something invalidates it, so asking every
    /// frame costs nothing while the tree is static.
    pub fn min_content(&mut self, measurer: &dyn Measurer) -> (f32, f32) {
        self.set_scale(sane_scale(measurer.scale_factor(), self.scale));
        if let Some(cached) = self.min_cache {
            return cached;
        }
        let root_node = self.entries[self.root as usize].node;
        let mut style = self.tree.style(root_node).expect("root exists").clone();
        let saved = style.clone();
        style.size = Size {
            width: taffy::style::Dimension::auto(),
            height: taffy::style::Dimension::auto(),
        };
        self.tree.set_style(root_node, style).expect("root exists");
        self.run(Size::MIN_CONTENT, measurer);
        let size = self.tree.layout(root_node).expect("root exists").size;
        self.tree.set_style(root_node, saved).expect("root exists");
        // The stored layouts are min-content ones now: the next `compute`
        // must re-solve for the real viewport (the cache makes that cheap).
        self.dirty = true;
        let result = (size.width, size.height);
        self.min_cache = Some(result);
        result
    }

    fn run(&mut self, available: Size<AvailableSpace>, measurer: &dyn Measurer) {
        let scale = self.scale;
        let root_node = self.entries[self.root as usize].node;
        self.tree
            .compute_layout_with_measure(root_node, available, |input, _, context, style| {
                taffy::compute_leaf_layout(input, style, |_, _| 0.0, |known, available| {
                    let Some(leaf) = context else {
                        return Size::ZERO;
                    };
                    let (w, h) = match leaf {
                        Leaf::Text(text) => text.measure(
                            measurer,
                            scale,
                            (known.width, known.height),
                            available.width.into(),
                        ),
                        Leaf::Measured(measure) => measure(Constraint {
                            known: (known.width, known.height),
                            available: (available.width.into(), available.height.into()),
                        }),
                    };
                    Size {
                        width: known.width.unwrap_or(w),
                        height: known.height.unwrap_or(h),
                    }
                })
            })
            .expect("root exists");
    }

    /// Absolute origins from the solver's relative ones, snapped by edge to
    /// the physical grid, diffed against the previous rects.
    fn snap(&mut self, viewport: Rect) {
        let scale = self.scale;
        // Round to the nearest physical pixel. The small bias makes exact
        // ties round the same way for both of two sums that differ only by
        // float noise, so a sibling's far edge and its neighbour's near
        // edge always land on the same grid line.
        let snap = |v: f32| (v * scale + 0.502).floor() / scale;
        self.moved_count = 0;
        let mut stack = std::mem::take(&mut self.stack);
        stack.clear();
        stack.push((self.root, viewport.x, viewport.y));
        while let Some((index, ox, oy)) = stack.pop() {
            let node = self.entries[index as usize].node;
            let layout = *self.tree.layout(node).expect("node exists");
            // The root's origin is the viewport's; children are relative.
            let (x, y) = if index == self.root {
                (ox, oy)
            } else {
                (ox + layout.location.x, oy + layout.location.y)
            };
            let (left, top) = (snap(x), snap(y));
            let rect = Rect::new(
                left,
                top,
                snap(x + layout.size.width) - left,
                snap(y + layout.size.height) - top,
            );
            let entry = &mut self.entries[index as usize];
            entry.moved = entry.rect != rect;
            if entry.moved {
                self.moved_count += 1;
            }
            entry.rect = rect;
            for ci in (0..entry.children.len()).rev() {
                let child = self.entries[index as usize].children[ci];
                let child_node = self.entries[child as usize].node;
                let hidden = self
                    .tree
                    .style(child_node)
                    .map(|s| s.display == taffy::style::Display::None)
                    .unwrap_or(false);
                if hidden {
                    self.clear_moved(child);
                } else {
                    stack.push((child, x, y));
                }
            }
        }
        self.stack = stack;
    }

    fn clear_moved(&mut self, index: u32) {
        self.entries[index as usize].moved = false;
        for ci in 0..self.entries[index as usize].children.len() {
            let child = self.entries[index as usize].children[ci];
            self.clear_moved(child);
        }
    }

    /// The deepest node containing `point`, honouring `overflow: hidden`
    /// clips and later siblings painting above earlier ones.
    pub fn hit(&self, point: (f32, f32)) -> Option<NodeId> {
        self.hit_from(self.root, point).map(|i| self.id_of(i))
    }

    fn hit_from(&self, index: u32, point: (f32, f32)) -> Option<u32> {
        let e = &self.entries[index as usize];
        let hidden = self
            .tree
            .style(e.node)
            .map(|s| s.display == taffy::style::Display::None)
            .unwrap_or(false);
        if hidden {
            return None;
        }
        let inside = e.rect.contains(point);
        if e.clip && !inside {
            return None;
        }
        for child in e.children.iter().rev() {
            if let Some(hit) = self.hit_from(*child, point) {
                return Some(hit);
            }
        }
        inside.then_some(index)
    }

    /// Like [`Layout::hit`], but climbs to the nearest ancestor (or self)
    /// that has a nonzero tag, and returns that tag.
    pub fn hit_tag(&self, point: (f32, f32)) -> Option<u32> {
        let mut at = self.hit(point)?.index;
        loop {
            let e = &self.entries[at as usize];
            if e.tag != 0 {
                return Some(e.tag);
            }
            at = e.parent?;
        }
    }
}
