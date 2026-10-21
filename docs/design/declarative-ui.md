# Declarative UI for Atomos: builder, events, scrolling, virtual lists

Status: **proposal, awaiting owner decisions (section 8); builder prototype done (section 10)**. Date: 2026-10-20. Builds on
`docs/design/flexbox-layout.md` (the retained `Layout`, phase 1 done) and
`docs/design/painter-order-and-clip-scopes.md` (direct-replay layers, scoped clips). Replaces the "sugar,
phase 2" sketch in flexbox-layout.md section 3b: the builder becomes **the** way to write Atomos UI, and the
retained `Layout` becomes its storage and escape hatch.

Anything marked **[estimate]** was not measured. Anything marked **[verify]** is an assumption the prototype
must check. Line numbers refer to the files as of this date.

---

## 1. Requirements and non-goals

**Target.** An Emacs-like editor (Typewritter): a few large custom text surfaces with their own line
virtualisation, many panes and buffers, a modeline per window, a minibuffer, completion popups, file trees,
and lists of tens of thousands of rows (completion candidates, search results).

**Requirements, in priority order.**

1. **Idle costs one branch.** A frame where nothing changed checks one pending flag and returns. No tree walk,
   no rebuild, no solve, no paint.
2. **No allocation in the steady state.** Rebuilding an unchanged tree allocates nothing. Scrolling inside an
   already-warm list allocates nothing. Hover changes allocate nothing in the UI layer. (Layer repaint still
   allocates inside the renderer; see 6.4.)
3. **Memory proportional to what is visible**, not to data size. A 50k-row list holds about one viewport of
   rows as nodes, plus at most a few bytes per row for heights (variable-height lists only).
4. **Scrolling never re-solves flexbox.** Offsets apply after the solve.
5. **Developer experience.** One way to describe UI, readable nesting, no `NodeId` bookkeeping, no closures
   that must be `'static`, no `Rc<RefCell>`. App state stays in the app's own structs.

**Non-goals for v1.**

- A built-in text-input widget. The minibuffer is a buffer drawn by the text surface. Atomos routes keys, text
  and preedit to the focused node; it does not edit text.
- Keymaps. Emacs-style prefix keys and mode maps belong to the app. Atomos reports the focus path so the app
  can resolve them.
- Accessibility (AccessKit), sticky headers, grid layout, CSS cascade, touch gestures beyond what winit
  reports as wheel deltas, momentum physics.
- A proc macro (see 2, option D).

---

## 2. Options compared

| | Idle frame | Rebuild of unchanged UI | Memory | Ergonomics | Borrow friction |
|---|---|---|---|---|---|
| **A** Retained imperative (today) | one branch | n/a (no rebuild) | lowest | poor: every dynamic element needs a stored `NodeId` and hand-written add/remove/diff | none |
| **B** Closure builder, cursor reconcile | one branch | O(visited nodes) compares, zero alloc | A + ~24 B/node | good: reads like markup, plain Rust control flow | low: `view(&self, ui)` borrows app immutably |
| **C** Value tree (vdom) + diff | one branch | allocates the whole description (or an arena), then a second pass to diff | A + a full description per build | good | low to moderate (owned props, `'static` handlers) |
| **D** Proc-macro rsx over B or C | as B/C | as B/C | as B/C | familiar to web devs; worse errors and IDE support | as B/C |
| **E** Fine-grained signals | one branch | none needed (targeted updates) | one heap node per signal and subscription | good for small state; awkward for a rope or a 50k-item `Vec` | high: signals cloned into `'static` closures |
| **F** Pure immediate mode | rebuild every frame while active | full rebuild and re-layout each frame | low | very good | low |

- **A, status quo** (`examples/layout_demo.rs:41-107`). Fastest possible and it stays as the storage layer,
  but the demo already shows the cost: every node is created once by hand, and a dynamic list would need its
  own diff. Keep as the escape hatch (`Build::layout()`), not as the way to write UI.
- **B, cursor reconcile.** Each call compares its description with the node at the cursor and writes only
  differences. This is immediate-mode syntax with retained storage. It works with the existing `Layout`
  because `set_style` already compares before dirtying (`src/layout/mod.rs:461-478`) and `set_text` already
  compares bytes and reuses capacity (`src/layout/paint.rs:141-149`). The weak spot is the cost of visiting
  unchanged subtrees, which per-element memo (3.5) removes.
- **C, vdom.** Dioxus renders `rsx!` into VNodes and hoists static template parts so that only dynamic parts
  are diffed. Iced rebuilds `view()` into an `Element` tree after every `update` and reconciles widget state
  by position and tag, with `lazy` to skip unchanged parts. Xilem builds a typed view tree and diffs it
  against the previous one into the retained Masonry widget tree; static typing keeps most of the tree off the
  heap, at the cost of very large generic types. Blitz, the Dioxus native renderer, lays out with taffy (to my
  knowledge with Stylo for CSS). **Rejected:** a description that is built and then diffed costs two passes
  and either allocates or needs an arena plus lifetimes. B does the compare during the one pass. **Taken from
  Iced:** the `update` / `view(&self)` split and events as values instead of closures. **Taken from Iced and
  Xilem:** memoisation as an explicit opt-in (`lazy` / `memoize`).
- **D, rsx macro.** Dioxus `rsx!`, Leptos `view!`, and Slint's own DSL (compiled by a macro or build script)
  show that markup syntax is liked. Over B a macro would only expand to the same calls, so it buys syntax and
  nothing else. It costs a `syn`/`quote` proc-macro crate (compile time), degraded rust-analyzer completion
  and diagnostics inside the macro, and a second thing to document. **Rejected for v1.** If nesting turns out
  noisy after a real port, a small `macro_rules!` layer can come later without changing B.
- **E, signals.** Leptos, Floem and Slint propagate changes to exactly the bound properties. I am not certain
  which reactive runtime Floem uses today. **Rejected as the primary model:** each signal and subscription is
  a heap node, handlers capture cloned signals into `'static` closures, and the editor's real state (ropes,
  buffer lists, a 50k-entry candidate `Vec`) is not signal-shaped. **Taken:** the goal of targeted
  invalidation. Here it is explicit (memo dependencies and revision counters), not tracked.
- **F, immediate mode.** egui rebuilds every frame it draws, keeps per-widget state keyed by hashed `Id`, and
  virtualises with `ScrollArea::show_rows` (uniform rows). Single-pass layout means content-sized things
  settle a frame late, which flexbox cannot accept. **Rejected:** rebuilding, re-solving and re-tessellating
  while anything animates. **Taken:** hashed ids, response polling (`clicked(id)` rather than callbacks), and
  `show_rows`-style "build only the visible range".
- **Zed GPUI.** Taffy for layout, app state in entities, handlers as closures (to my knowledge allocated per
  render into a per-frame arena), and two list primitives: `uniform_list` (fixed height, renders the visible
  range) and `list` (variable height, measured, with a state object that keeps scroll position). **Taken:**
  the two-tier list design (uniform fast path, measured variable path), and the idea that scroll position
  lives in a state object, not in the description.
- **Slint.** `ListView` over a `Model` trait (row count, row data, change notifications). **Taken:** the list
  asks the app for a count and for rows by index; it never copies the data.

**Recommendation: B, with per-element memo, polled events, and Atomos-owned scroll and list state. No macro.**

---

## 3. Recommendation and public API

### 3.1 Frame flow

```text
ui.set_viewport(rect, scale)        compares with the stored values; sets a pending bit only on change
ui.handle_input(&input)             hit-test, hover/press/drag/focus, wheel -> scroll; fills ui.events()
if ui.idle() { return }             THE idle branch: nothing pending, nothing to do
app.update(&mut ui, &input)         match on ui.events(); mutate app state; bump revisions; ui.request_build()
for _ in 0..2 {                     at most one extra pass (5.2: a list learnt its height)
    if ui.needs_build() { ui.build(|b| app.view(b)) }      reconcile; memo skips unchanged elements
    ui.compute(&layer)              taffy solve only if dirty; row islands; snap; content sizes
    if !ui.needs_build() { break }
}
ui.tick(now) -> Tick                advance scroll glides; re-snap scrolled subtrees; wake hint
for slot in ui.dirty_slots() { layers[slot].clear(); ui.paint(slot, &layers[slot]) }
```

`Ui` keeps one `pending: u32` bitset (viewport or scale changed, input events, build requested, layout dirty,
glide running, slot dirty). `ui.idle()` is `pending == 0`, so the handler returns after that one branch.
Resize and DPI changes never pass through `Input`, which is why `set_viewport` exists: called every frame (or
on `Resized`/`ScaleFactorChanged`), it is a compare. This is what makes requirement 1 true: `Layout::compute`
alone is not one branch today (it runs `set_scale` and `sane_rect`, and the first no-op after a solve walks
every entry to clear `moved`, `mod.rs:572-584`).

### 3.2 Types

```rust
// src/ui/id.rs
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Id(pub u64);                    // 0 = none. Replaces the u32 tag.
impl Id {
    pub const NONE: Id = Id(0);
    pub const fn new(v: u64) -> Id;
    pub fn of(v: impl Hash) -> Id;         // FxHash-style, inline, no dependency
    pub fn child(self, k: u64) -> Id;      // Id::CANDIDATES.child(row_key)
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Sense(u8);                      // CLICK | DRAG | HOVER | SCROLL | FOCUS | TEXT, BitOr

pub struct El {                            // what one element is; all plain data, no heap
    pub style: Style,                      // geometry (existing type)
    pub paint: Option<Paint>,              // background, border, radius, + state variants (3.7)
    pub id: Id,                            // for events, rect lookup, focus
    pub key: u64,                          // reconcile identity among siblings; 0 = positional
    pub sense: Sense,
    pub slot: Option<u8>,                  // paint boundary: this subtree paints into layer `slot`
}
impl Style { pub fn el(self) -> El; }      // and impl From<Style> for El
impl El { pub fn id(self, Id) -> Self; pub fn key(self, u64) -> Self; pub fn paint(self, Paint) -> Self;
          pub fn sense(self, Sense) -> Self; pub fn slot(self, u8) -> Self; }

#[derive(Clone, Copy, PartialEq, Eq)] pub struct TextStyleId(u16);
pub struct TextStyle { pub font: Font, pub params: FontParameters, pub color: Color, pub wrap: bool }
pub struct Span<'a> { pub text: &'a str, pub style: TextStyleId }
```

`TextStyleId` exists because `Font::Named(String)` (`src/renderer/font.rs:14`) makes every `Font` clone
allocate. An API taking `Font` or `Text` by value cannot be zero-alloc on rebuild. Styles are interned once
(`ui.text_style(TextStyle) -> TextStyleId`, deduplicated by equality) at startup or on a theme change, and
compared as a `u16`.

```rust
// src/ui/mod.rs
pub struct Ui { /* Layout, TextStyles, Router, scroll + list side tables, scratch */ }
impl Ui {
    pub fn new() -> Self;
    pub fn text_style(&mut self, s: TextStyle) -> TextStyleId;        // interned once
    pub fn handle_input(&mut self, input: &Input);  pub fn events(&self) -> &[UiEvent];
    pub fn request_build(&mut self);  pub fn needs_build(&self) -> bool;
    pub fn build(&mut self, f: impl FnOnce(&mut Build<'_>));
    pub fn set_viewport(&mut self, viewport: Rect, scale: f32);   // compare; pending bit on change
    pub fn compute(&mut self, m: &dyn Measurer) -> bool;
    pub fn tick(&mut self, now: Instant) -> Tick;     // Tick { animating: bool, wake_in: Option<Duration> }
    pub fn idle(&self) -> bool;  pub fn dirty_slots(&self) -> SlotMask;   // SlotMask: u64
    pub fn paint(&mut self, slot: u8, layer: &Layer);  // clears the slot's dirty bit
    pub fn rect(&self, id: Id) -> Option<Rect>;       // visual rect (scroll applied)
    pub fn focused(&self) -> Option<Id>;  pub fn focus_path(&self) -> &[Id];  pub fn set_focus(&mut self, id: Id);
    pub fn scroll_offset(&self, id: Id) -> (f32, f32);
    pub fn scroll_to(&mut self, id: Id, to: ScrollTo, animate: bool);   // ScrollTo::{Offset, Reveal(Rect)}
    pub fn scroll_by(&mut self, id: Id, delta: ScrollDelta);           // lines, pages, px: for keymaps
    pub fn set_content_size(&mut self, id: Id, w: f32, h: f32);        // app-virtualised surfaces
    pub fn set_ime_caret(&mut self, id: Id, rect_in_node: Rect);  pub fn sync_ime(&self, input: &mut Input);
    pub fn set_offset(&mut self, id: Id, dx: f32, dy: f32);  pub fn set_opacity(&mut self, id: Id, a: f32);
    pub fn layout(&mut self) -> &mut Layout;          // escape hatch
}

// src/ui/build.rs
pub struct Build<'u> { /* &mut Ui, cursor stack (reused Vec on Ui) */ }
impl Build<'_> {
    // Primitive pair. Everything else is sugar over these.
    pub fn open(&mut self, el: impl Into<El>);
    pub fn close(&mut self);

    pub fn div(&mut self, el: impl Into<El>, f: impl FnOnce(&mut Self));      // open + f + close
    pub fn el(&mut self, el: impl Into<El>);                                  // empty box
    pub fn text(&mut self, el: impl Into<El>, style: TextStyleId, text: &str);
    pub fn text_fmt(&mut self, el: impl Into<El>, style: TextStyleId, args: fmt::Arguments<'_>);
    pub fn spans(&mut self, el: impl Into<El>, spans: &[Span<'_>]);
    pub fn surface(&mut self, el: impl Into<El>);                             // app paints it (5.5)
    pub fn scroll(&mut self, el: impl Into<El>, axes: Axes, f: impl FnOnce(&mut Self));
    pub fn list(&mut self, el: impl Into<El>, list: List<'_>, row: impl FnMut(&mut Self, usize));
    pub fn memo(&mut self, deps: impl Hash, f: impl FnOnce(&mut Self));      // exactly one element
    pub fn layout(&mut self) -> &mut Layout;
}

pub struct List<'a> { count: usize, height: RowHeight, key: Option<&'a dyn Fn(usize) -> u64>,
                      revision: u64, overscan: u16, anchor: Anchor<'a> }
pub enum RowHeight { Uniform(f32), Estimated(f32) }
pub enum Anchor<'a> { Top, End /* stick to end, for logs */, Key(&'a dyn Fn(u64) -> Option<usize>) }
impl<'a> List<'a> {
    pub fn uniform(count: usize, h: f32) -> Self;
    pub fn variable(count: usize, estimate: f32) -> Self;
    pub fn keys(self, f: &'a dyn Fn(usize) -> u64) -> Self;
    pub fn revision(self, r: u64) -> Self;    // bump when row contents change
    pub fn overscan(self, rows: u16) -> Self;
    pub fn anchor(self, a: Anchor<'a>) -> Self;
}
```

`text_fmt` writes `format_args!` into a scratch `String` kept on `Ui` and then compares, so `ui.text_fmt(el,
s, format_args!("{line}:{col}"))` in a modeline does not allocate.

### 3.3 Worked example: toolbar, completion list, text surface

```rust
const SAVE: Id = Id::new(1);
const SEARCH: Id = Id::new(2);
const CANDS: Id = Id::new(3);
const EDITOR: Id = Id::new(4);

struct App {
    theme: Theme,                    // holds TextStyleIds and colours, built once
    buffer: Buffer,                  // rope, revision counter, caret, ...
    query: String,
    cands: Vec<Candidate>,           // 50k rows; each has a stable `id: u64`
    cands_rev: u64,
    selected: usize,
    toolbar_rev: u64,
}

impl App {
    fn update(&mut self, ui: &mut Ui, input: &Input) {
        for ev in ui.events() {
            match (ev.id, ev.kind) {
                (SAVE, EventKind::Click { .. }) => { self.buffer.save(); self.toolbar_rev += 1; }
                (_, EventKind::Click { .. }) if ev.row.is_some_and(|r| r.list == CANDS) => {
                    self.selected = ev.row.unwrap().index as usize;
                    self.cands_rev += 1;
                }
                (EDITOR, EventKind::Press { pos, clicks, .. }) => self.buffer.place_caret(pos, clicks),
                (EDITOR, EventKind::Drag { pos, .. }) => self.buffer.extend_selection(pos),
                (EDITOR, EventKind::Input(i)) => self.buffer.handle(&input.events()[i]),  // keys, text, preedit
                _ => {}
            }
        }
        if self.changed() { ui.request_build(); }
    }

    fn view(&self, ui: &mut Build) {
        let t = &self.theme;
        ui.div(Style::new().el().slot(0), |ui| {
            // Toolbar: rebuilt only when something it reads changes (deps must list them all, R3).
            ui.memo((self.toolbar_rev, self.buffer.modified(), &self.query), |ui| {
                ui.div(Style::row().height(px(36.0)).items_center().gap(8.0).padding_x(10.0)
                        .el().paint(t.bar), |ui| {
                    ui.text(Style::new().el().id(SAVE).sense(Sense::CLICK).paint(t.button),
                            t.label, if self.buffer.modified() { "Save*" } else { "Save" });
                    ui.text(Style::new().flex(1.0).el().id(SEARCH).sense(Sense::CLICK), t.dim, &self.query);
                });
            });
            ui.div(Style::row().flex(1.0).el(), |ui| {
                // Text surface: Atomos owns wheel/glide/scrollbar; the app draws the lines.
                ui.surface(Style::new().flex(1.0).el().id(EDITOR).slot(1)
                    .sense(Sense::CLICK | Sense::DRAG | Sense::SCROLL | Sense::FOCUS | Sense::TEXT));
                // 50k candidates, ~40 rows built.
                let key = |i: usize| self.cands[i].id;
                ui.list(Style::new().fixed_width(320.0).el().id(CANDS).slot(2),
                        List::uniform(self.cands.len(), 22.0).keys(&key).revision(self.cands_rev),
                        |ui, i| {
                    let c = &self.cands[i];
                    let paint = if i == self.selected { t.row_selected } else { t.row };
                    ui.div(Style::row().height(px(22.0)).items_center().padding_x(8.0)
                            .el().sense(Sense::CLICK).paint(paint), |ui| {
                        ui.text(Style::new().flex(1.0).el(), t.label, &c.label);
                        ui.text(Style::new().el(), t.dim, &c.detail);
                    });
                });
            });
            ui.memo(self.buffer.modeline_rev(), |ui| {
                ui.div(Style::row().height(px(22.0)).padding_x(8.0).el().paint(t.modeline), |ui| {
                    ui.text_fmt(Style::new().el(), t.label,
                        format_args!("{}  {}:{}", self.buffer.name(), self.buffer.line(), self.buffer.col()));
                });
            });
        });
    }
}

// In RedrawRequested (self.ui and self.app are separate fields: disjoint borrows):
// ui.set_viewport(view, scale); ui.handle_input(&input); if ui.idle() { return; }
// app.update(&mut ui, &input);
// for _ in 0..2 { if ui.needs_build() { ui.build(|b| app.view(b)); } ui.compute(&layers[0]); if !ui.needs_build() { break } }
// let tick = ui.tick(Instant::now());
// if ui.dirty_slots().has(1) { layers[1].clear(); app.paint_buffer(&layers[1], ui.rect(EDITOR), ui.scroll_offset(EDITOR)); }
// for s in ui.dirty_slots().except(1) { layers[s].clear(); ui.paint(s, &layers[s]); }
// if tick.animating { scheduler.request_redraw() } ; if let Some(d) = tick.wake_in { scheduler.request_redraw_in(d) }
```

The closures borrow `&self` (shared), and `ui` is a separate value, so there is no borrow conflict and nothing
needs to be `'static` or boxed.

### 3.4 State

- **App state lives in the app.** `view(&self, ..)` reads it; only `update` mutates it. This is the Elm/Iced
  split: one consistent snapshot per build, and no mutation during description.
- **Interaction state lives in Atomos side tables** keyed by node: hover, pressed, drag capture, focus, scroll
  offset and glide, list windows and row pools, IME caret. The app never stores it; it reads it through `Ui`
  queries or receives it as events. Because it is keyed by node, the node must survive rebuilds: an element
  with a nonzero `Id` is matched by that `Id` (3.6), so a pane's scroll position cannot be reset by a sibling
  appearing before it.
- **`Id -> entry` map.** `rect(id)`, `scroll_offset(id)`, `set_offset(id)` and events resolve through a
  `HashMap<Id, u32>` on `Ui`, updated on create, remove and row recycle (rows use `Id::child(key)`). It is
  never shrunk, so it stops allocating once warm. Duplicate Ids are a debug assertion.
- **No generic per-node `state::<T>()` slots in v1** (question 8.6). They need type erasure (a `Box<dyn Any>`
  per slot) and hide state from the app. Items that need app state per row key it by the row's own id in an
  app-side map.

### 3.5 Rebuild trigger and scope

- **Triggered, never per frame.** `needs_build()` is true when the app called `request_build()`, or when a
  list's visible window moved onto rows that have not been built (5.2), or after a viewport change that grew a
  list.
- **Scoped by memo.** `memo(deps, f)` hashes `deps` (a revision counter, or a tuple of them) and compares with
  the hash stored on the element `f` produced last time. Equal: the cursor steps over that element and its
  whole subtree without calling `f`, O(1). Different: `f` runs and reconciles. Taffy 0.15 has no
  `Display::Contents` (`taffy/src/style/mod.rs:251-266`), so a memo cannot be a transparent wrapper node; it
  attaches to exactly the one element its closure emits (asserted in debug builds).
- **Lists punch through memo.** If a list's window moved, a "needs visit" bit is set on the list and its
  ancestors; a memo with that bit runs its closure even when its deps are equal. Everything inside reconciles
  as unchanged (compares only, no writes).
- **Whole-tree rebuild is still cheap.** Without memo a build of the shell (~15 regions), modelines, and a
  popup is a few hundred nodes; at an estimated 20-60 ns per unchanged node (one 248-byte `Style` compare, one
  `Paint` compare, a text byte compare) that is 10-30 us **[estimate]**. Memo exists for the panes and lists,
  not to make the common case viable.

### 3.6 Keying and reconcile algorithm

The cursor is a stack of `(parent entry, next child position)` kept in a `Vec` on `Ui` (reused). For each
`open` or leaf call with `(kind, key)`. The identity used is `key` if nonzero, else the element's `Id` if
nonzero, else none (positional):

1. Look at the parent's child at `pos`. If its kind and identity match, **reuse**: compare and write style,
   paint, text, id, sense; advance.
2. Else, if the identity is not positional, search the remaining siblings for it. Linear scan for up to 16 remaining
   children; above that, a scratch map `key -> position` is built once for this parent in this build (reused
   `HashMap`, cleared, so no allocation once warm). If found, **move** it to `pos`
   (`TaffyTree::remove_child_at_index` + `insert_child_at_index`, `taffy/src/tree/taffy_tree.rs:745,798`) and
   reuse.
3. Else, if positional, the child at `pos` is positional too, and the child at `pos + 1` matches, the child
   at `pos` was removed: drop it and reuse `pos + 1` (the common "a conditional element disappeared" case).
   A child that has an identity is never dropped by this lookahead; it is only removed by the trailing trim
   if nothing claimed it, so an insertion misread as a deletion cannot recreate an `Id`'d pane.
4. Else **create** a node and insert it at `pos`. Node slots come from the existing free list
   (`mod.rs:384-391`), so after warm-up creation reuses storage.

On `close`, children from `pos` to the end were not described this build: remove them with one
`remove_children_range` (`taffy_tree.rs:818`) and one `Vec::truncate`, not one `remove` each (today `remove`
does a `retain` on the parent's children per call, `mod.rs:428-432`, which is quadratic when trimming many).

Complexity: O(visited nodes) when the structure is stable; O(n) per parent for keyed reorders with the scratch
map; moves are rare. Guidance in the docs: use `key` for anything in a loop over data that can insert or
delete; toggle `Style::hidden(true)` instead of omitting an element that flips often (it keeps the node, its
caches, and its last rect, `style.rs:214-219`).

**Prop comparison.**

- `Style`: `PartialEq` on the wrapped `taffy::Style`. With `grid` off, every heap-carrying field
  (`GridTrackVec`, `taffy/src/style/mod.rs:840-863`) is compiled out: phase 1 measured
  `size_of::<taffy::Style>()` at 248 bytes, against taffy's own size assertions of 528/560 bytes with grid
  on (`mod.rs:1808,1814`). So the compare and the clone are plain memory. Comparing in place (`tree.style(node) != &el.style.inner`) avoids
  the clone that `set_style` does today (`mod.rs:464-468`). Turning on `grid` later would make `Style`
  heap-carrying; the builder would then need a compare-before-clone path (it already has one).
- `Paint`: `PartialEq`, small, no heap.
- Text: `&str` against the stored `String` bytes; on change `clear` + `push_str` reuses capacity
  (`paint.rs:141-149`). Spans: compare run lengths and `TextStyleId`s, then bytes.
- Id, key, sense, slot: integer compares.

### 3.7 Events: polled, targeted by `Id`, no stored closures

**Recommendation: no handler closures at all.** `handle_input` writes `UiEvent`s into a `Vec` on `Ui`
(cleared, not freed, each frame), and the app matches on them in `update`.

```rust
#[derive(Clone, Copy)] pub struct UiEvent { pub id: Id, pub node: NodeId, pub row: Option<RowRef>, pub kind: EventKind }
#[derive(Clone, Copy)] pub struct RowRef { pub list: Id, pub index: u32, pub key: u64 }
#[derive(Clone, Copy)]
pub enum EventKind {
    Press { button: MouseButton, pos: (f32, f32), clicks: u8, mods: ModifiersState },
    Release { button: MouseButton, pos: (f32, f32) },
    Click { button: MouseButton, pos: (f32, f32), clicks: u8 },
    DragStart { pos: (f32, f32) }, Drag { pos: (f32, f32), delta: (f32, f32) }, DragEnd { pos: (f32, f32) }, DragCancel,
    HoverEnter, HoverLeave, Scroll { delta: (f32, f32) }, FocusIn, FocusOut,
    Input(usize),                       // index into Input::events(): Key, Text, Preedit
}
```

Why not stored closures: a closure per handler per build is a box per build unless arena-allocated, it must
capture app state (`&mut` conflicts with the `&self` view, or `Rc<RefCell>`), and it hides control flow from
the app, which for an editor is the keymap. Polling costs a small `Vec` scan per frame that has input.
`EventKind` is `Copy`: text and preedit are referenced by index into `Input::events()` because
`InputEvent::Text`/`Preedit` own `String`s (`src/input.rs:107-114`) and copying them would allocate.

**Routing rules.**

- **Hit-testing** uses visual rects (scroll applied, section 4) and clips, as `hit` does today
  (`mod.rs:733-757`), plus culling: children of a clipping node whose rect misses the clip are skipped, and
  list rows are found by index arithmetic (O(1) uniform, O(log n) variable). The target is the deepest node
  with a matching `Sense`; events carry the nearest ancestor `Id`, the way `hit_tag` climbs today
  (`mod.rs:761-770`).
- **Hover:** `HoverEnter`/`HoverLeave` on change only. Hover visuals do not need a rebuild: `Paint` gains
  optional `hover`, `pressed`, `focused` variants, resolved at paint time, so a hover change dirties one paint
  slot.
- **Press, click, drag, capture:** press on a `CLICK` or `DRAG` node captures the pointer until release;
  motion beyond 3 px (default) turns it into a drag (`DragStart` then `Drag`), a release without that is a
  `Click`. Capture survives the pointer leaving the node or the window. Focus loss
  (`InputEvent::Focus(false)`, which already force-releases buttons, `input.rs:420-427`) ends a capture with
  `DragCancel`. Click count (double, triple) uses a 400 ms window and 4 px radius (defaults) and is reported
  on `Press` too, because editors select words on the second press, not on release.
- **Wheel:** goes to the innermost scroll container under the pointer that can still move in that direction;
  no chaining to ancestors once at the end (default, question 8.7). A node with `Sense::SCROLL` that is not a
  scroll container (for example a surface whose content size was never set) receives `Scroll` itself.
- **Keyboard and IME:** one focused node per `Ui`. Click on a `FOCUS` node focuses it. Every `Key`, `Text` and
  `Preedit` input event is delivered, in order, as `Input(i)` targeted at the focused id. Atomos binds no
  keys. `focus_path()` gives root-to-focus ids for buffer-local, mode, and global keymap lookup. The focused
  node with `Sense::TEXT` reports its caret with `set_ime_caret`; `sync_ime` converts it to the visual window
  rect and calls `Input::set_ime_cursor_area` (`input.rs:547-553`), which already only talks to the OS on
  change.

### 3.8 What allocates, and when

| When | Allocates |
|---|---|
| Idle frame | nothing |
| Rebuild, unchanged tree | nothing |
| Rebuild with text that grew past its capacity | that string, once |
| First build, new elements beyond the free list, a list whose window grows past its pool | node storage, taffy nodes, children `Vec`s, row pool entries |
| Scroll inside a warm list (rows recycled from the pool) | nothing in the UI layer |
| Interning a new `TextStyle` | once |
| Layer repaint (renderer) | draw commands own their text (6.4) |

---

## 4. Scrolling

**Node.** `Build::scroll(el, axes, f)` creates a container with `Overflow::Scroll` mapped to taffy's
`Overflow::Scroll` with `scrollbar_width = 0` (in taffy 0.15 a zero width makes `Scroll` behave like `Hidden`,
`taffy/src/style/mod.rs:467-470`; it also gives the container a zero automatic minimum size, which is what a
scroller wants). Scrollbars overlay the content instead of reserving space (question 8.8). The container clips
(it is `clip = true` for paint and hit).

**Offset in a side table.** `scrolls: Vec<ScrollState>` indexed from a `u32` side index on the entry.
`ScrollState` holds two `Glide`s (`timing.rs:136`), the content size, the viewport size, and a drag state for
the thumb.

**Applied at snap, not at solve.** `snap` (`mod.rs:672-721`) already walks the tree with accumulated origins;
for a scroll container it subtracts the snapped offset from the origin it passes to children. So `rect()`,
`hit()` and `paint()` all see visual rects with no other change. A scroll-only change calls a new
`Layout::resnap(container)` that re-walks only that subtree from the stored taffy layouts: no solve, no
`compute`. Offsets are rounded to the physical pixel grid so text stays crisp and the clip stays a scissor.

**Moved flags and Manual layers.** A re-snap would mark every scrolled descendant `moved`. The rule becomes:
`resnap` marks the container's **paint slot** dirty once and does not report per-node `moved` for descendants
(their layout did not change; only the slot needs a repaint). That keeps the "redraw if my rect moved" logic
meaningful and makes a scroll cost one slot repaint.

**Content size.** Taffy's `content_size` feature is off (`Cargo.toml:16`), so taffy does not report it.
**Default: compute it in our snap pass**, as the union of the children's margin boxes relative to the
container's content box (the pass already visits every child, so it adds a max per child, no extra traversal).
The alternative is enabling the feature, which adds a `Size<f32>` to each stored layout
(`taffy/src/tree/layout.rs:475,589`) and computes scrollable overflow inside the solver; it is only worth it
if we need its exact CSS semantics (question 8.9). Lists and surfaces do not use either: their content size
comes from the row model or `set_content_size`.

**Input.**

- Wheel lines: `glide.aim(target + lines * line_px)` where `target` is `Glide::target()` (`timing.rs:215`), so
  fast notches accumulate.
- Touchpad pixel deltas must not be eased (they are already smooth). Today `Input` converts them to lines at
  `PIXELS_PER_LINE = 20` and drops the unit (`input.rs:42-44, 329-341`; its comment already flags this).
  Change: `InputEvent::Scroll { delta, unit: ScrollUnit::{Lines, Pixels} }`; pixels call `glide.settle(value +
  delta)`.
- Keyboard: none bound by Atomos. The app's keymap calls `ui.scroll_by(id, ScrollDelta::Pages(1.0))` or
  `scroll_to(id, ScrollTo::Reveal(rect), animate)` (minimal scroll so `rect` is visible; the caret-follow
  primitive).
- Thumb drag: press on the scrollbar captures; motion maps to offset with `settle` (stays under the pointer).

**Scrollbars** are painted by `Ui::paint` after the container's children, inside the container's slot: thin
overlay bars when content exceeds the viewport, with hover and pressed paint variants. Auto-hide after idle is
a later polish (one `Animation` per container, asks for frames only while fading). **Glide integration.**
`ui.tick(now)` advances only containers in a small `active: Vec<u32>` list (pushed on aim, removed on
landing), re-snaps each one whose value changed, and returns `animating = !active.is_empty()`. Idle cost is
one `is_empty` check inside the pending bitset.

---

## 5. Virtualised lists

### 5.1 Shape

`Build::list(el, List, row)` creates one container node in the main tree. Flex lays out **the container** (its
size comes from its own style and its parent, like any element). **Rows are not taffy children of the main
tree.** Each built row is a *row island*: a detached taffy subtree (no taffy parent) solved on its own with
`compute_layout_with_measure` called on the row root (`taffy_tree.rs:967`) at definite width = the container's
content width and height = the row height (uniform) or auto (variable). **[verify]** that a detached root's
cache is independent of the main root's and that `mark_dirty` on a row stops at the row root.

In our own `Entry` tree the rows are children of the list node, so paint, hit, and the moved report work
unchanged; `snap` places a row at `container content origin + row_offset(index) - scroll offset`. So 50k rows
cost one window of row nodes, and main-tree solves never touch rows.

Since the list node has no taffy children, an `auto` height would solve to zero, which breaks a completion
popup that should be `min(count * h, max_height)` tall. The list node is therefore a data leaf (`Leaf::List`,
no closure) whose max-content height is `count * h` (uniform) or the estimated total (variable), and whose
min-content height is zero. A `max_height` on its style clamps it the normal flexbox way, and a `flex(1.0)`
list in a pane ignores it. The leaf is re-measured only when `count` or the height model changes.

### 5.2 Window and pool

- **Window** = rows intersecting `[offset - overscan, offset + viewport + overscan]`, overscan default 4 rows.
- **Uniform:** `first = floor(offset / h)`, `last = ceil((offset + vh) / h)`, `row_offset(i) = i * h`, content
  height `count * h`. O(1), no per-row memory.
- **Pool:** `Vec<RowSlot { key: u64, index: u32, root: u32, built_rev: u64 }>` sized to the peak window. Rows
  that leave the window go to a free list with their nodes intact. A row entering the window reuses a free
  slot and its description reconciles against the recycled subtree; a candidate row with the same structure
  updates two texts and nothing else. Slots are found by key with a linear scan (windows are ~50 rows) or a
  reused map above 128.
- **When `row` is called:** for rows entering the window, and for every visible row when `revision` changed.
  Rows that stay visible with the same key and revision are skipped wholesale. So a smooth scroll inside the
  window calls `row` zero times; crossing one row boundary calls it once.
- **Two-pass sizing.** The window depends on the container's height, which is known only after `compute`. On
  the first frame or when the container grows, `compute` sets `needs_build` and the app builds once more in
  the same frame (bounded to one extra pass, default; question 8.10).

### 5.3 Keys and anchoring

- Without `keys`, the key is the index: inserts shift row contents (fine for rebuild-on-filter lists such as
  completion).
- With `keys`, a row keeps its slot (and hover or pressed state) across inserts and deletes.
- **Anchor.** Scroll position is stored as `(anchor index, offset into that row)`, not raw pixels.
  `Anchor::Top` keeps the index (content inserted above pushes the view). `Anchor::Key(f)` re-finds the anchor
  row by key after a revision change, for stable views over changing data. `Anchor::End` keeps the view pinned
  to the bottom while it is at the bottom (compilation output, logs).

### 5.4 Variable heights (phase 5)

- Heights: `Vec<f32>` initialised to the estimate, plus a Fenwick tree over it. 8 bytes per row: 400 KB for
  50k rows **[estimate]**.
- A built row's island solve yields its true height; `update(i, h - old)` is O(log n). `row_offset(i)` is a
  prefix sum, O(log n); "row at y" is a Fenwick descent, O(log n).
- Count or splice changes rebuild the tree in O(n) (50k adds, tens of microseconds **[estimate]**). A B-tree
  of chunks (GPUI's `list` uses a sum tree, to my knowledge) is the upgrade if profiles show splices.
- Anchoring by row keeps the view stable while estimates above it are corrected. The scrollbar uses the
  estimated total and may drift slightly, which is the usual trade-off.

### 5.5 The text surface is the extreme case

The editor surface is `Build::surface(el)`: one leaf, no children, no measure function (flex gives it its
size). Once the app sets its content size (`set_content_size(EDITOR, width, lines * line_height)`) it is a
scroll container with no children; Atomos handles wheel, glide, scrollbar and thumb drag exactly as for lists,
and the app reads `scroll_offset(EDITOR)` when drawing the lines in view. Its own line virtualisation, caret
blink (`Stepped`, `timing.rs:38`), selection and preedit drawing stay app code. The surface's slot is its own
layer, so a caret blink repaints only it. This is the same contract as a list (content size from a model,
offset applied by Atomos, only the visible window is drawn) with drawing instead of building.

---

## 6. Paint, invalidation, and changes to `src/layout`

### 6.1 Slots map to Layers

`El::slot(n)` marks a paint boundary: that subtree paints into layer `n`. Each entry stores its inherited slot
(a `u8`). The app owns the `Layer`s (created `Manual` with `Renderer::new_layer_top`, `layer.rs:92-104`) and
passes the right one to `ui.paint(slot, &layer)`. This matches what Typewritter does today (one `Manual` layer
per region, flexbox-layout.md:24).

A slot is dirty when, in it: a node was created or removed, a paint or text changed, a rect moved in a solve,
a scroll re-snap happened, a hover, pressed or focus paint variant changed, or an override (section 7)
changed. `dirty_slots()` is a `u64` mask (64 slots, default). Z-order is the app's layer creation order;
popups go in a slot whose layer was created last.

### 6.2 Paint pass

`paint(slot)` walks only that slot's subtrees (the list of slot roots is kept on `Ui`), in tree order as today
(`paint.rs:232-268`). New in the walk: scroll containers emit `with_clip` (rectangle, scissor path,
`layer.rs:3792`) and **cull** children whose visual rect misses the clip; lists iterate only their window;
paint variants resolve from interaction state; overrides add offset and multiply alpha. Painter order inside a
layer is preserved by the renderer (painter-order doc, section 8).

### 6.3 Concrete changes to `src/layout`

1. `Entry` (`mod.rs:225-237`): widen `tag: u32` to `id: u64`; add `key: u64`, `kind: u8`, `sense: u8`, `slot:
   u8`, `flags: u8` (scroll, list, island root, memo), `side: u32` (index into scroll, list or memo tables).
   From 80 bytes to about 104 **[estimate]**.
2. Child operations without per-call scans: `insert_child_at(parent, pos)`, `move_child(parent, from, to)`,
   `truncate_children(parent, len)` using taffy's index APIs and `remove_children_range`.
3. In-place setters for the builder: `set_style_value(id, &Style)` compares against `tree.style(node)` without
   cloning; `set_id`, `set_sense`.
4. `Leaf::Measured(Box<dyn FnMut>)` (`mod.rs:217-222`) cannot be re-described every build without allocating.
   The builder's `surface` uses a data leaf (`Leaf::Surface`, size from style only); custom measurement goes
   through a `measure(id, Constraint)` method on `Measurer` with a default of `(0, 0)`. `add_measured` stays
   for imperative use.
5. Text leaves store a `TextStyleId` (or span runs) instead of a full `Text` with an owned `Font`;
   `TextStyles` interner in `src/layout/text_style.rs`.
6. Scroll: `Overflow::Scroll` in `style.rs:108-116`; offset subtraction and content-size accumulation in
   `snap`; new `resnap(subtree)`.
7. Row islands: entries whose taffy node is detached; `compute` runs a second pass solving dirty islands (and
   all visible islands when the container width changed) after the main solve. The list node itself becomes
   a `Leaf::List` data leaf sized from its row model (5.1).
8. Lazy `moved` reset: store `moved_gen: u32` per entry and a solve generation on `Layout`; `moved(id)` is
   `moved_gen == solves`. Removes the O(n) clear in `compute` (`mod.rs:575-584`).
9. Hit-test culling inside clips, O(1) row lookup for lists, and a reused path buffer so the router can report
   the ancestor `Id`.
10. Paint storage: `set_paint` boxes on `None -> Some` (`paint.rs:221-226`). Replace with a dense `Vec<Paint>`
    pool plus free list (entry holds an index), and assign in place on `Some -> Some`.
11. Border paint: `rounded_rect_path` uses `format!` and the ring is built on every paint (`paint.rs:87-107,
    283`). Cache the path string per node, regenerated only when the rect or radius changes; or add a ring
    primitive to `Layer`.
12. Text paint clones `Font` (`paint.rs:321, 331`), which allocates for `Font::Named`. Moves to the renderer
    item in 6.4.

### 6.4 Renderer-side allocation (out of scope here, listed so it is not lost)

`Layer::draw_text` stores an owned copy of the text in each command, and `push_styled_text` copies the spans
with `to_vec()` (`layer.rs:3721`). A repaint therefore allocates in proportion to what it draws. This only
happens on frames that repaint, never idle ones. Fix later in the renderer: a per-layer text arena (one
`String`, ranges per command, cleared with capacity on `clear`) and `Font` behind an `Arc` or an interned id.
Until then the zero-alloc claims cover the UI layer only, and the allocation test says so.

---

## 7. Animation and the cost model

**Three ways to animate, cheapest first.**

1. **Paint-only overrides**, for opacity and translation (popup fade and slide, a flash): `ui.set_offset(id,
   dx, dy)`, `ui.set_opacity(id, a)` from the app's `Animation` each frame. No build, no solve; the slot
   repaints. Overrides are not part of the description, so a rebuild does not reset them (they live in a side
   table until cleared).
2. **Scroll glides**, owned by Atomos (section 4): re-snap plus slot repaint.
3. **Layout animations** (a sidebar width, a panel collapsing): the view reads `self.sidebar.value()` and the
   memo deps include its bits (`value().to_bits()`), so only that component rebuilds each frame, followed by a
   solve whose cost scales with the dirty path (phase 1 measured a one-leaf change on 201 nodes at 7 us).

**Wake-ups.** `ui.tick` returns `animating` for running glides and fades, and `wake_in` for scheduled steps;
the app forwards them to `FrameScheduler::request_redraw` and `request_redraw_in` (`frame.rs:139, 151`). Caret
blink stays with the app's `Stepped` and its `wake_in` (`timing.rs:116`).

**Cost model** (per frame, UI layer only; renderer repaint excluded):

| Event | Build | Solve | Snap | Paint |
|---|---|---|---|---|
| Nothing | no | no | no | no (one branch) |
| Hover change | no | no | no | one slot |
| Scroll inside window | no | no | re-snap the container subtree | one slot |
| Scroll crossing rows | entering rows only | new islands only | container subtree | one slot |
| Modeline text change | memo scope (~20 nodes) | dirty path | whole tree (today's snap) | slots with moved nodes |
| Completion query typed | list visible rows (revision bump) | visible islands | container subtree | popup slot |
| Window resize | none, unless a list grows | full (61 us per 200 nodes measured) | full | all slots |
| Layout animation | one memo scope | dirty path | full | affected slots |

Snap is a full-tree walk after any solve today. With hundreds of nodes it is a few microseconds
**[estimate]**; if it shows up, snap can start from the highest dirty ancestor, which taffy does not report,
so we would track it.

---

## 8. Open questions and risks, with proposed defaults

1. **Closure builder B as the only description API.** Default: yes, with `open`/`close` underneath and
   `Build::layout()` as the escape hatch.
2. **Syntax layer.** Default: plain builder calls are the stable API; the `ui!` `macro_rules!` layer (prototyped, section 10) ships with it as the markup-like way to write views; a proc-macro `rsx!` is decided after porting one real component, because it would expand to the same calls. April asked for an HTML-like feel, so this is the question to answer first.
3. **Events.** Default: polled `UiEvent`s targeted by `Id`; no stored handler closures.
4. **Hover/pressed/focus visuals.** Default: `Paint` variants resolved at paint time, no rebuild.
5. **Memo granularity.** Default: one element per memo; deps hashed with an inline FxHash.
6. **Per-node generic state slots.** Default: not in v1; app-owned state.
7. **Wheel chaining at scroll ends.** Default: no chaining (innermost scrollable that can move).
8. **Scrollbars.** Default: overlay, 6 px, always shown when content overflows; auto-hide later.
9. **Content size for `scroll` containers.** Default: computed in our snap pass; leave taffy's `content_size`
   feature off.
10. **Extra build pass on first frame or list growth.** Default: allow one, same frame; never loop.
11. **Variable-height lists.** Default: uniform in v1 (phase 4), Fenwick variable lists in phase 5.
12. **Paint slots.** Default: at most 64, app-owned layers, z-order by layer creation. Risk: a pane opened
    later lands above an existing popup layer unless the app keeps a fixed overlay layer created last. A
    `Renderer::reorder_layer` would remove the constraint.
13. **Scroll unit in `Input`.** Default: add `ScrollUnit` to `InputEvent::Scroll` (a small public change to
    `src/input.rs`).
14. **Renderer allocation on repaint (6.4).** Default: accept for now; open a renderer task for the text
    arena.
15. **`Id` width.** Default: `u64` (hash-combined row ids need it); `tag: u32` and `hit_tag` become
    `id`/`hit_id`, a breaking change before any external user.

**Risks.**

- **R1, detached islands in taffy.** If taffy's caching or dirty propagation assumes one root, islands need
  their own `TaffyTree` (one per list). The prototype checks this first.
- **R2, two sources of truth for one rect.** Visual rect (scroll applied) and layout rect differ inside
  scrollers. `rect()` is always visual; a `layout_rect()` is provided for the rare caller that needs the
  other.
- **R3, memo misuse.** Deps that miss a field show stale UI. Debug builds can run memoised closures anyway
  every N builds and assert that nothing changed (cheap with the compare-only reconcile).
- **R4, `Style` growth.** Enabling `grid` puts `Vec`s into `taffy::Style`; the zero-alloc test will catch it.

---

## 9. Implementation plan

Sizes are lines of code **[estimate]**. Each phase keeps `cargo check` clean with no warnings, and keeps the
existing layout tests green.

**Phase 0: layout prep (S, ~300).** `src/layout/mod.rs`, `paint.rs`, `style.rs`, new
`src/layout/text_style.rs`. Items 6.3.1-6.3.5, 8, 10. Tests: child insert/move/truncate keep taffy and `Entry`
children in sync; `moved` generation semantics equal today's (port the existing moved tests);
`set_style_value` no-op does not dirty; paint pool reuse.

**Phase 1: builder and reconcile (M, ~600).** New `src/ui/{mod,id,build}.rs`, `src/lib.rs` (`pub mod ui`).
`Ui`, `Build`, `El`, cursor reconcile, keys, lookahead, trailing trim, memo, `text_fmt`, pending bitset.
Tests: unchanged rebuild produces zero writes and zero taffy dirties; keyed reorder moves nodes instead of
recreating (node ids survive); conditional element removal via lookahead; memo skip leaves the subtree
untouched; debug assertion on a memo emitting two elements. **Allocation test** `tests/alloc_steady_state.rs`:
an integration test with its own `#[global_allocator]` wrapping `System`, counting `alloc`, `alloc_zeroed`
and `realloc` (string growth goes through `realloc`) only while a thread-local flag is set (the harness runs
tests on several threads, so a global counter would pick up their allocations). The flag is
`thread_local! { static ARMED: Cell<bool> = const { Cell::new(false) } }`; the `const` initializer avoids a
lazy-init allocation re-entering the allocator. Public API only, with a fake `Measurer`. Warm up with two builds and computes, then arm and
assert zero allocations for: an idle frame; a forced rebuild of an unchanged 200-node tree with memo bypassed;
`text_fmt` producing the same string. Later phases add cases to the same file. Layer repaint is excluded
(6.4). Bench (`#[ignore]`, release, same style as the layout numbers): rebuild of an unchanged 1000-node tree,
with and without memo; reconcile of a keyed 200-child reorder.

**Phase 2: input routing (M, ~500).** `src/ui/route.rs`, `src/layout/paint.rs` (paint variants),
`src/input.rs` (`ScrollUnit`). Hover, press, click count, drag threshold and capture, `DragCancel` on focus
loss, focus and focus path, `Input(i)` delivery, IME caret sync. Tests: synthetic `Input` sequences (the
existing input tests build winit events the same way, `input.rs:655-668`); capture across leaving the node;
double-click timing; hover change dirties exactly one slot. Alloc test: hover change, click.

**Phase 3: scroll containers (M, ~500).** `src/ui/scroll.rs`, `snap`/`resnap` in `mod.rs`, `Overflow::Scroll`,
scrollbars in `paint.rs`. Tests: scrolling does not increase `solves()`; visual rects shift by the snapped
offset; hit-test respects the offset and the clip; content size equals the children's union; glide lands and
stops asking for frames; pixel deltas settle without easing. Alloc test: wheel scroll for 30 frames.

**Phase 4: uniform virtual lists (M-L, ~700).** `src/ui/list.rs`, islands in `mod.rs`. Window, pool, keys,
revision, overscan, anchors, two-pass sizing. Tests: 50k-row list creates at most `window + overscan` rows;
scrolling inside the window calls `row` zero times; crossing one row calls it once; keyed insert above keeps
the anchored row in place; `Anchor::End` follows appends. Alloc test: scroll across 100 rows with a warm pool.
Bench: build, compute and paint-walk of a 50k list; scroll step cost.

**Phase 5: variable-height lists (M, ~400).** Fenwick heights, measured islands, key anchoring under estimate
correction. Tests: offsets match a brute-force prefix sum on random heights; anchor row stays at the same
visual y while rows above are measured.

**Phase 6: slots, surface, demo (S-M, ~400).** Slot dirty mask, per-slot paint, `surface` with
`set_content_size`, overrides. New `examples/ui_demo.rs`: the section 3.3 toolbar, a 50k candidate list, and a
surface drawing numbered lines with a blinking caret on its own layer. Tests: caret blink dirties only the
surface slot; override changes repaint without a build.

**What the prototype must prove** (phases 1, 3 and a minimal 4, in that order):

1. A rebuild of an unchanged tree allocates nothing and writes nothing (the allocation test, idle and
   forced-rebuild cases).
2. Row islands work in one `TaffyTree` (R1), or the fallback is chosen.
3. Scrolling a 50k uniform list never calls the solver for the main tree, creates at most one window of rows,
   and allocates nothing once warm.
4. The section 3.3 example compiles as written with no borrow-checker workarounds (`view(&self)` plus disjoint
   field borrows in the build call).

---

## 10. Prototype results (`src/layout/build.rs`, `tests/builder.rs`, `examples/builder_demo.rs`)

Built after the design, on the existing `Layout` (about 330 lines, no change to the solver). Covers: `Layout::build`,
`Build::{open, close, div, el, text, text_fmt}`, `El` (style, paint, id), `TextStyle`, reconcile by id or position with
lookahead removal, keyed moves and tail trim, and the `ui!` macro. Not covered: memo, events, scroll, lists, slots.

**Measured with a counting global allocator (`tests/builder.rs`, debug build, fake measurer):**

| Case (about 210 nodes) | Result |
|---|---|
| Rebuild of an unchanged tree, plus `compute` | 0 allocations, `compute` returns false, `solves()` unchanged |
| Rebuild where one `text_fmt` string changed | 0 allocations in the build |
| The solve that follows that change | **11 allocations, inside taffy** (not the builder) |
| Keyed swap of first and last item | node handles survive, nothing recreated |
| Conditional element appears and vanishes | its siblings keep their nodes |
| 10 items cut to 3 | tail trimmed, one re-solve |

Findings that change the plan:

1. **The zero-allocation claim holds for describing the UI, not for solving it.** Taffy allocates (11 times here) per
   solve. Phase 1's allocation test must assert "build = 0" and separately track "solve = N", and a later task should
   find which taffy call allocates (likely per-container scratch vectors) and whether `compute_layout` can reuse them.
   Idle frames are unaffected because they never solve.
2. **`macro_rules!` can express the markup but not Rust-looking syntax.** `for x in expr {` is rejected because an
   `expr` fragment may not be followed by `{`, so the prototype uses `for x in expr => { .. }` and `if cond => { .. }`.
   `let` and arbitrary statements need their own arms. This is the concrete argument for a proc macro if April wants
   views to look like normal Rust control flow inside markup.
3. **The call-site noise is `.el()` and the style chain, not the closures.** Compare the same line:

   ```text
   builder: ui.text(Style::new().flex(1.0).el().id(SEARCH), &t.dim, &self.query);
   ui!:     text(Style::new().flex(1.0).el().id(SEARCH), &t.dim, &self.query);
   rsx!:    text { flex: 1, id: SEARCH, style: dim, "{self.query}" }      // proc-macro mock, not built
   ```
   A proc macro would let attributes map straight to `Style` methods (`flex: 1`, `gap: 8`, `padding_x: 10`) and
   remove `.el()`; that is where an HTML-like feel actually comes from. Cost is the one noted in section 2, D.
4. `describe` compares fonts by reference, so `TextStyle` held in a theme gives zero-allocation rebuilds without the
   interner (`TextStyleId`) from section 3.2. The interner is still needed if text styles must be copied cheaply per
   span; the plan can defer it to the spans phase.
5. Creating an `Id`-less keyed list (items without `.id(..)`) reconciles by position, as designed; the demo uses ids on
   the dynamic sidebar items.

**Still to prove, in this order:** row islands in one taffy tree (R1), scroll without re-solve, a 50k-row uniform list.
