//! The declarative builder (see `docs/design/declarative-ui.md`).
//!
//! You describe the UI by calling [`Build`] methods in tree order; each call
//! is compared with the retained node at the cursor and only differences are
//! written. Describing an unchanged tree allocates nothing and dirties
//! nothing, so [`Layout::compute`] stays a no-op.
//!
//! Identity: an element with a nonzero `id` is matched by it among its
//! siblings (and moved if it shifted); otherwise it is matched by position,
//! with a one-step lookahead so a conditional element disappearing does not
//! recreate everything after it.

use super::paint::TextLeaf;
use super::{Layout, Leaf, NodeId, Paint, Style};
use crate::renderer::{Color, Font, FontParameters};

/// What one element is. Plain data, no heap. Most callers never name it:
/// a [`Style`] converts into one (`b.div(Style::row().bg(c), ..)`), which
/// splits the layout half from the look half.
#[derive(Clone, Debug, PartialEq)]
pub struct El {
    style: Style,
    paint: Option<Paint>,
    /// Hit-test tag and reconcile identity. `0` means positional. Siblings
    /// must not share a nonzero id (debug builds assert).
    id: u64,
}

impl From<Style> for El {
    fn from(mut style: Style) -> Self {
        let look = std::mem::take(&mut style.look);
        let id = std::mem::take(&mut style.id);
        let paint = Paint::from_look(&look);
        El { style, paint, id }
    }
}

impl Style {
    /// Splits this style into the layout half (re-solves when it changes)
    /// and the look half (repaints when it changes). Optional: the builder
    /// accepts a `Style` directly.
    pub fn el(self) -> El {
        self.into()
    }
}

impl El {
    pub fn paint(mut self, paint: Paint) -> Self {
        self.paint = Some(paint);
        self
    }

    pub fn id(mut self, id: u64) -> Self {
        self.id = id;
        self
    }
}

/// How a text element looks. Build it once (a theme) and pass it by
/// reference; comparing it per rebuild allocates nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    pub font: Font,
    pub params: FontParameters,
    pub color: Color,
    pub wrap: bool,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self::new(
            Font::Named("monospace".into()),
            FontParameters::new(14.0),
            Color::rgb(0xAB, 0xB2, 0xBF),
        )
    }
}

impl TextStyle {
    pub fn new(font: Font, params: FontParameters, color: Color) -> Self {
        Self {
            font,
            params,
            color,
            wrap: true,
        }
    }

    pub fn no_wrap(mut self) -> Self {
        self.wrap = false;
        self
    }

    /// Hash of every field (floats by bits), with no allocation.
    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.font.hash(&mut h);
        for v in [
            self.params.size,
            self.params.weight,
            self.params.width,
            self.params.tracking,
            self.params.slant,
        ] {
            v.to_bits().hash(&mut h);
        }
        self.color.hash_bits(&mut h);
        self.wrap.hash(&mut h);
        h.finish() | 1
    }
}

pub(super) struct Frame {
    parent: u32,
    pos: usize,
    /// Memo hash to record on `close`; `0` for none.
    memo: u64,
    /// Where this container's parked children start in `Layout::parked`.
    park_from: usize,
    /// Slot in `Layout::id_maps`, built on the first id miss.
    map: Option<usize>,
}

/// The inherited text style stack, kept on the `Layout` so its strings are
/// reused across builds.
pub(super) struct TextStack {
    styles: Vec<TextStyle>,
    prints: Vec<u64>,
}

impl TextStack {
    pub(super) fn new() -> Self {
        let base = TextStyle::default();
        Self {
            prints: vec![base.fingerprint()],
            styles: vec![base],
        }
    }
}

/// A description in progress. Created by [`Layout::build`].
pub struct Build<'a> {
    ui: &'a mut Layout,
    frames: Vec<Frame>,
    depth: usize,
    pending_memo: u64,
    taken_memo: u64,
    created: bool,
}

impl Layout {
    /// Describes the whole tree under the root with `f`. Anything the
    /// description does not mention any more is removed.
    pub fn build(&mut self, f: impl FnOnce(&mut Build<'_>)) {
        let mut frames = std::mem::take(&mut self.build_frames);
        frames.clear();
        self.build_gen = self.build_gen.wrapping_add(1).max(1);
        self.parked.clear();
        self.id_maps_used = 0;
        #[cfg(debug_assertions)]
        self.described_ids.clear();
        frames.push(Frame {
            parent: self.root,
            pos: 0,
            memo: 0,
            park_from: 0,
            map: None,
        });
        let mut b = Build {
            ui: self,
            frames,
            depth: 0,
            pending_memo: 0,
            taken_memo: 0,
            created: false,
        };
        f(&mut b);
        b.close();
        let Build { ui, frames, .. } = b;
        ui.build_frames = frames;
    }
}

impl Build<'_> {
    /// Starts a container; call [`Build::close`] when its children are
    /// described. Returns `true` if a [`Build::memo`] matched and the
    /// children are already right: describe nothing, just `close`.
    /// Prefer [`Build::div`], which handles this.
    pub fn open(&mut self, el: impl Into<El>) -> bool {
        let el = el.into();
        let i = self.reconcile(false, &el);
        let memo = self.taken_memo;
        let hit = memo != 0 && !self.created && self.ui.entries[i as usize].memo == memo;
        if memo == 0 {
            self.ui.entries[i as usize].memo = 0;
        }
        let pos = if hit {
            self.ui.entries[i as usize].children.len()
        } else {
            0
        };
        self.frames.push(Frame {
            parent: i,
            pos,
            memo: if hit { 0 } else { memo },
            park_from: self.ui.parked.len(),
            map: None,
        });
        hit
    }

    /// Ends the container opened last, removing children not described.
    pub fn close(&mut self) {
        let Frame {
            parent,
            pos,
            memo,
            park_from,
            map,
        } = self.frames.pop().expect("close without open");
        if map.is_some() {
            self.ui.id_maps_used -= 1;
        }
        // Children set aside and never reclaimed are gone.
        while self.ui.parked.len() > park_from {
            let e = self.ui.parked.pop().expect("length checked");
            if self.ui.entries[e as usize].parent.is_none() {
                let id = self.ui.id_of(e);
                self.ui.remove(id);
            }
        }
        while self.ui.entries[parent as usize].children.len() > pos {
            let last = *self.ui.entries[parent as usize].children.last().unwrap();
            let id = self.ui.id_of(last);
            self.ui.remove(id);
        }
        if memo != 0 {
            self.ui.entries[parent as usize].memo = memo;
        }
    }

    pub fn div(&mut self, el: impl Into<El>, f: impl FnOnce(&mut Self)) {
        if !self.open(el) {
            f(self);
        }
        self.close();
    }

    /// An empty box.
    pub fn el(&mut self, el: impl Into<El>) {
        let el = el.into();
        self.reconcile(false, &el);
        self.leaf_done();
    }

    /// Where the next child goes: the open container and its position.
    fn cursor(&self) -> (u32, usize) {
        let f = self.frames.last().expect("describe inside build");
        (f.parent, f.pos)
    }

    /// A leaf cannot carry a memo; it has no children to skip.
    fn leaf_done(&self) {
        debug_assert_eq!(self.taken_memo, 0, "memo wraps a container, not a leaf");
    }

    /// Wraps exactly one element (the next `div`/`open`) so that, when
    /// `deps` and the inherited text style are unchanged since the last
    /// build, its children are skipped without being visited. `deps` must
    /// list everything the children read.
    pub fn memo(&mut self, deps: impl std::hash::Hash, f: impl FnOnce(&mut Self)) {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        deps.hash(&mut h);
        self.ui.text_stack.prints[self.depth].hash(&mut h);
        self.pending_memo = h.finish() | 1;
        let (parent, start) = self.cursor();
        f(self);
        // Exactly one container: none leaves the marker unconsumed, a leaf
        // trips the check in `leaf_done`, and two would skip the second.
        debug_assert!(
            self.pending_memo == 0,
            "memo must wrap exactly one container, but it described none"
        );
        debug_assert_eq!(
            self.cursor(),
            (parent, start + 1),
            "memo must wrap exactly one container, but it described several"
        );
        self.pending_memo = 0;
    }

    /// Sets the text style inherited by every `text` below, until `f`
    /// returns. A nested `with_text` replaces it for its own subtree.
    pub fn with_text(&mut self, style: &TextStyle, f: impl FnOnce(&mut Self)) {
        let stack = &mut self.ui.text_stack;
        let depth = self.depth + 1;
        if depth < stack.styles.len() {
            if stack.styles[depth] != *style {
                stack.styles[depth].clone_from(style);
                stack.prints[depth] = style.fingerprint();
            }
        } else {
            stack.styles.push(style.clone());
            stack.prints.push(style.fingerprint());
        }
        let saved = std::mem::replace(&mut self.depth, depth);
        f(self);
        self.depth = saved;
    }

    /// Text in the inherited style (see [`Build::with_text`]).
    pub fn text(&mut self, el: impl Into<El>, text: &str) {
        self.text_in(el.into(), None, text);
    }

    /// Text in `style`, ignoring the inherited one.
    pub fn text_with(&mut self, el: impl Into<El>, style: &TextStyle, text: &str) {
        self.text_in(el.into(), Some(style), text);
    }

    fn text_in(&mut self, el: El, over: Option<&TextStyle>, text: &str) {
        let i = self.reconcile(true, &el);
        let ui = &mut *self.ui;
        let style = over.unwrap_or(&ui.text_stack.styles[self.depth]);
        let node = ui.entries[i as usize].node;
        let (remeasure, recolor) = match ui.tree.get_node_context_mut(node) {
            Some(Leaf::Text(leaf)) => {
                leaf.describe(text, &style.font, &style.params, &style.color, style.wrap)
            }
            _ => (false, false),
        };
        if remeasure {
            let _ = ui.tree.mark_dirty(node);
            ui.invalidate();
        }
        if recolor || remeasure {
            ui.repaint = true;
        }
        self.leaf_done();
    }

    /// Text from `format_args!` in the inherited style, written into a
    /// scratch string first so a modeline like `"{line}:{col}"` does not
    /// allocate when unchanged.
    pub fn text_fmt(&mut self, el: impl Into<El>, args: std::fmt::Arguments<'_>) {
        use std::fmt::Write;
        let mut scratch = std::mem::take(&mut self.ui.build_scratch);
        scratch.clear();
        let _ = scratch.write_fmt(args);
        self.text(el, &scratch);
        self.ui.build_scratch = scratch;
    }

    /// The node id of the element described last, for callers that mix the
    /// builder with the imperative API.
    pub fn last(&self) -> Option<NodeId> {
        let f = self.frames.last()?;
        let child = f.pos.checked_sub(1)?;
        let index = *self.ui.entries[f.parent as usize].children.get(child)?;
        Some(self.ui.id_of(index))
    }

    /// Finds, moves or creates the node for the next child of the open
    /// container, updates it to match `el`, and returns its entry index.
    fn reconcile(&mut self, is_text: bool, el: &El) -> u32 {
        self.taken_memo = std::mem::take(&mut self.pending_memo);
        let (parent, pos) = self.cursor();
        let matches = |ui: &Layout, index: u32| {
            let e = &ui.entries[index as usize];
            e.tag == el.id && ui.is_text(index) == is_text
        };
        let len = self.ui.entries[parent as usize].children.len();
        let at = |ui: &Layout, p: usize| ui.entries[parent as usize].children[p];

        self.created = false;
        let found = if pos < len && matches(self.ui, at(self.ui, pos)) {
            Some(at(self.ui, pos))
        } else if el.id != 0 {
            self.claim(parent, pos, el.id, is_text)
        } else if pos + 1 < len
            && self.ui.entries[at(self.ui, pos) as usize].tag == 0
            && matches(self.ui, at(self.ui, pos + 1))
        {
            // A positional element before this one vanished.
            let gone = self.ui.id_of(at(self.ui, pos));
            self.ui.remove(gone);
            Some(at(self.ui, pos))
        } else {
            None
        };

        let index = match found {
            Some(index) => {
                self.update(index, el);
                index
            }
            None => {
                self.created = true;
                self.create(parent, pos, is_text, el)
            }
        };
        #[cfg(debug_assertions)]
        assert!(
            el.id == 0 || self.ui.described_ids.insert((parent, el.id)),
            "layout: two siblings share id {}",
            el.id
        );
        self.ui.entries[index as usize].seen = self.ui.build_gen;
        self.frames.last_mut().unwrap().pos += 1;
        index
    }

    /// Finds the not-yet-described child (or one parked earlier in this
    /// container) with identity `id` and leaves it at `pos`. Children
    /// skipped on the way are parked, not moved one by one, so deleting
    /// the first row of a long keyed list is one range removal, not a
    /// shift of every later row. A parked child whose id never reappears
    /// is removed when the container closes.
    fn claim(&mut self, parent: u32, pos: usize, id: u64, is_text: bool) -> Option<u32> {
        let slot = self.id_map(parent, pos);
        let e = *self.ui.id_maps[slot].get(&id)?;
        let ent = &self.ui.entries[e as usize];
        if !ent.live
            || ent.seen == self.ui.build_gen
            || ent.tag != id
            || self.ui.is_text(e) != is_text
        {
            return None;
        }
        match ent.parent {
            Some(p) if p == parent => {
                let children = &self.ui.entries[parent as usize].children;
                let from = pos + children[pos..].iter().position(|c| *c == e)?;
                if from > pos {
                    self.ui.park_range(parent, pos, from);
                }
                Some(e)
            }
            None => {
                self.ui.attach_child(parent, pos, e);
                Some(e)
            }
            Some(_) => None,
        }
    }

    /// The id table for the open container, built on first use from the
    /// children not yet described.
    fn id_map(&mut self, parent: u32, pos: usize) -> usize {
        if let Some(slot) = self.frames.last().expect("describe inside build").map {
            return slot;
        }
        let slot = self.ui.id_maps_used;
        self.ui.id_maps_used += 1;
        if slot == self.ui.id_maps.len() {
            self.ui.id_maps.push(Default::default());
        }
        let ui = &mut *self.ui;
        let map = &mut ui.id_maps[slot];
        map.clear();
        for c in &ui.entries[parent as usize].children[pos..] {
            let tag = ui.entries[*c as usize].tag;
            if tag != 0 {
                map.insert(tag, *c);
            }
        }
        self.frames.last_mut().expect("frame").map = Some(slot);
        slot
    }

    fn update(&mut self, index: u32, el: &El) {
        let id = self.ui.id_of(index);
        self.ui.set_layout_style(id, &el.style);
        if self.ui.entries[index as usize].paint.as_deref() != el.paint.as_ref() {
            self.ui.set_paint(id, el.paint.clone());
        }
    }

    fn create(&mut self, parent: u32, pos: usize, is_text: bool, el: &El) -> u32 {
        let parent_id = self.ui.id_of(parent);
        let id = if is_text {
            // Placeholder content; `text` fills it in right after.
            let blank = super::Text::new(
                String::new(),
                Font::Named(String::new()),
                FontParameters::new(1.0),
                Color::rgb(0, 0, 0),
            );
            let leaf = Leaf::Text(Box::new(TextLeaf::new(blank)));
            self.ui.insert(parent_id, el.style.clone(), Some(leaf))
        } else {
            self.ui.insert(parent_id, el.style.clone(), None)
        };
        let len = self.ui.entries[parent as usize].children.len();
        if pos < len - 1 {
            self.ui.move_child(parent, len - 1, pos);
        }
        self.ui.entries[id.index as usize].tag = el.id;
        if el.paint.is_some() {
            self.ui.set_paint(id, el.paint.clone());
        }
        id.index
    }
}

impl Layout {
    fn is_text(&self, index: u32) -> bool {
        matches!(
            self.tree.get_node_context(self.entries[index as usize].node),
            Some(Leaf::Text(_))
        )
    }

    /// Detaches children `from..to` of `parent` (solver and ours) and
    /// parks them for the rest of this container's description.
    fn park_range(&mut self, parent: u32, from: usize, to: usize) {
        let pnode = self.entries[parent as usize].node;
        self.tree
            .remove_children_range(pnode, from..to)
            .expect("range valid");
        let start = self.parked.len();
        self.parked
            .extend(self.entries[parent as usize].children.drain(from..to));
        for c in &self.parked[start..] {
            self.entries[*c as usize].parent = None;
        }
        self.invalidate();
    }

    /// Re-attaches a parked child at `pos`.
    fn attach_child(&mut self, parent: u32, pos: usize, child: u32) {
        let pnode = self.entries[parent as usize].node;
        self.tree
            .insert_child_at_index(pnode, pos, self.entries[child as usize].node)
            .expect("position valid");
        self.entries[parent as usize].children.insert(pos, child);
        self.entries[child as usize].parent = Some(parent);
        self.invalidate();
    }

    /// Moves child `from` of `parent` to position `to`, in both the
    /// solver's order and ours.
    fn move_child(&mut self, parent: u32, from: usize, to: usize) {
        let pnode = self.entries[parent as usize].node;
        let moved = self
            .tree
            .remove_child_at_index(pnode, from)
            .expect("child exists");
        self.tree
            .insert_child_at_index(pnode, to, moved)
            .expect("position valid");
        let children = &mut self.entries[parent as usize].children;
        let index = children.remove(from);
        children.insert(to, index);
        self.invalidate();
    }
}

/// HTML-shaped sugar over [`Build`], to judge how the description reads.
/// It expands to the plain calls; nothing here is required.
///
/// ```ignore
/// ui!(b;
///     div(Style::row().gap(8.0).bg(theme.bar)) {
///         with_text(&theme.label) {
///             text(Style::new(), "Save");
///             for item in &items => {
///                 text(Style::new().id(item.id), &item.name);
///             }
///             if modified => { text(Style::new(), "*"); }
///         }
///     }
/// );
/// ```
#[macro_export]
macro_rules! ui {
    ($b:ident;) => {};
    ($b:ident; div($el:expr) { $($body:tt)* } $($rest:tt)*) => {
        $b.div($el, |$b| { $crate::ui!($b; $($body)*); });
        $crate::ui!($b; $($rest)*);
    };
    ($b:ident; text($el:expr, $text:expr); $($rest:tt)*) => {
        $b.text($el, $text);
        $crate::ui!($b; $($rest)*);
    };
    ($b:ident; text($el:expr, $($fmt:tt)+); $($rest:tt)*) => {
        $b.text_fmt($el, format_args!($($fmt)+));
        $crate::ui!($b; $($rest)*);
    };
    ($b:ident; text_with($el:expr, $style:expr, $text:expr); $($rest:tt)*) => {
        $b.text_with($el, $style, $text);
        $crate::ui!($b; $($rest)*);
    };
    ($b:ident; with_text($style:expr) { $($body:tt)* } $($rest:tt)*) => {
        $b.with_text($style, |$b| { $crate::ui!($b; $($body)*); });
        $crate::ui!($b; $($rest)*);
    };
    ($b:ident; memo($deps:expr) { $($body:tt)* } $($rest:tt)*) => {
        $b.memo($deps, |$b| { $crate::ui!($b; $($body)*); });
        $crate::ui!($b; $($rest)*);
    };
    ($b:ident; el($el:expr); $($rest:tt)*) => {
        $b.el($el);
        $crate::ui!($b; $($rest)*);
    };
    ($b:ident; for $p:pat in $it:expr => { $($body:tt)* } $($rest:tt)*) => {
        for $p in $it { $crate::ui!($b; $($body)*); }
        $crate::ui!($b; $($rest)*);
    };
    ($b:ident; if $c:expr => { $($body:tt)* } $($rest:tt)*) => {
        if $c { $crate::ui!($b; $($body)*); }
        $crate::ui!($b; $($rest)*);
    };
}
