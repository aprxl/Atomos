//! PROTOTYPE of the declarative builder (see `docs/design/declarative-ui.md`).
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

/// What one element is. Plain data, no heap.
#[derive(Clone, Debug, PartialEq)]
pub struct El {
    pub style: Style,
    pub paint: Option<Paint>,
    /// Hit-test tag and reconcile identity. `0` means positional.
    pub id: u32,
}

impl From<Style> for El {
    fn from(style: Style) -> Self {
        El {
            style,
            paint: None,
            id: 0,
        }
    }
}

impl Style {
    pub fn el(self) -> El {
        self.into()
    }
}

impl El {
    pub fn paint(mut self, paint: Paint) -> Self {
        self.paint = Some(paint);
        self
    }

    pub fn id(mut self, id: u32) -> Self {
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
}

pub(super) struct Frame {
    parent: u32,
    pos: usize,
}

/// A description in progress. Created by [`Layout::build`].
pub struct Build<'a> {
    ui: &'a mut Layout,
    frames: Vec<Frame>,
}

impl Layout {
    /// Describes the whole tree under the root with `f`. Anything the
    /// description does not mention any more is removed.
    pub fn build(&mut self, f: impl FnOnce(&mut Build<'_>)) {
        let mut frames = std::mem::take(&mut self.build_frames);
        frames.clear();
        frames.push(Frame {
            parent: self.root,
            pos: 0,
        });
        let mut b = Build { ui: self, frames };
        f(&mut b);
        b.close();
        let Build { ui, frames } = b;
        ui.build_frames = frames;
    }
}

impl Build<'_> {
    /// Starts a container; call [`Build::close`] when its children are
    /// described. Prefer [`Build::div`].
    pub fn open(&mut self, el: impl Into<El>) {
        let el = el.into();
        let i = self.reconcile(false, &el);
        self.frames.push(Frame { parent: i, pos: 0 });
    }

    /// Ends the container opened last, removing children not described.
    pub fn close(&mut self) {
        let Frame { parent, pos } = self.frames.pop().expect("close without open");
        while self.ui.entries[parent as usize].children.len() > pos {
            let last = *self.ui.entries[parent as usize].children.last().unwrap();
            let id = self.ui.id_of(last);
            self.ui.remove(id);
        }
    }

    pub fn div(&mut self, el: impl Into<El>, f: impl FnOnce(&mut Self)) {
        self.open(el);
        f(self);
        self.close();
    }

    /// An empty box.
    pub fn el(&mut self, el: impl Into<El>) {
        let el = el.into();
        self.reconcile(false, &el);
    }

    pub fn text(&mut self, el: impl Into<El>, style: &TextStyle, text: &str) {
        let el = el.into();
        let i = self.reconcile(true, &el);
        let node = self.ui.entries[i as usize].node;
        let changed = match self.ui.tree.get_node_context_mut(node) {
            Some(Leaf::Text(leaf)) => {
                leaf.describe(text, &style.font, &style.params, &style.color, style.wrap)
            }
            _ => false,
        };
        if changed {
            let _ = self.ui.tree.mark_dirty(node);
            self.ui.invalidate();
        }
    }

    /// Text from `format_args!`, written into a scratch string first so a
    /// modeline like `"{line}:{col}"` does not allocate when unchanged.
    pub fn text_fmt(
        &mut self,
        el: impl Into<El>,
        style: &TextStyle,
        args: std::fmt::Arguments<'_>,
    ) {
        use std::fmt::Write;
        let mut scratch = std::mem::take(&mut self.ui.build_scratch);
        scratch.clear();
        let _ = scratch.write_fmt(args);
        self.text(el, style, &scratch);
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
        let (parent, pos) = {
            let f = self.frames.last().expect("describe inside build");
            (f.parent, f.pos)
        };
        let matches = |ui: &Layout, index: u32| {
            let e = &ui.entries[index as usize];
            e.tag == el.id && ui.is_text(index) == is_text
        };
        let len = self.ui.entries[parent as usize].children.len();
        let at = |ui: &Layout, p: usize| ui.entries[parent as usize].children[p];

        let found = if pos < len && matches(self.ui, at(self.ui, pos)) {
            Some(at(self.ui, pos))
        } else if el.id != 0 {
            // Shifted: look for the same id further along and move it up.
            let hit = (pos + 1..len).find(|p| matches(self.ui, at(self.ui, *p)));
            hit.map(|from| {
                let index = at(self.ui, from);
                self.ui.move_child(parent, from, pos);
                index
            })
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
            None => self.create(parent, pos, is_text, el),
        };
        self.frames.last_mut().unwrap().pos += 1;
        index
    }

    fn update(&mut self, index: u32, el: &El) {
        let id = self.ui.id_of(index);
        let node = self.ui.entries[index as usize].node;
        let same_style = self.ui.tree.style(node).map(|s| s == &el.style.inner).unwrap_or(false)
            && self.ui.entries[index as usize].clip == el.style.clip;
        if !same_style {
            // The root's size belongs to the viewport; leave it alone.
            let style = el.style.clone();
            self.ui.set_style(id, |_| style);
        }
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
///     div(Style::row().gap(8.0).el()) {
///         text(Style::new().el(), &theme.label, "Save");
///         for item in &items => {
///             text(Style::new().el().id(item.id), &theme.label, &item.name);
///         }
///         if modified { text(Style::new().el(), &theme.dim, "*"); }
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
    ($b:ident; text($el:expr, $style:expr, $text:expr); $($rest:tt)*) => {
        $b.text($el, $style, $text);
        $crate::ui!($b; $($rest)*);
    };
    ($b:ident; text($el:expr, $style:expr, $($fmt:tt)+); $($rest:tt)*) => {
        $b.text_fmt($el, $style, format_args!($($fmt)+));
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
    ($b:ident; let $($stmt:tt)+) => { compile_error!("let must end with `;`") };
}
