//! Prototype checks for the declarative builder: reconcile semantics and the
//! zero-allocation steady state (UI layer only; renderer repaint excluded).

use std::alloc::{GlobalAlloc, Layout as AllocLayout, System};
use std::cell::Cell;

use atomos::layout::{Build, Layout, Measurer, Rect, Style, TextStyle, px};
use std::cell::Cell as Counter;
use atomos::renderer::{Color, Font, FontParameters};
use atomos::ui;

thread_local! { static ARMED: Cell<bool> = const { Cell::new(false) }; static COUNT: Cell<usize> = const { Cell::new(0) }; }

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: AllocLayout) -> *mut u8 {
        if ARMED.with(|a| a.get()) { COUNT.with(|c| c.set(c.get() + 1)); }
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocLayout) { unsafe { System.dealloc(p, l) } }
    unsafe fn realloc(&self, p: *mut u8, l: AllocLayout, n: usize) -> *mut u8 {
        if ARMED.with(|a| a.get()) { COUNT.with(|c| c.set(c.get() + 1)); }
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static A: Counting = Counting;

fn allocations(f: impl FnOnce()) -> usize {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    f();
    ARMED.with(|a| a.set(false));
    COUNT.with(|c| c.get())
}

struct Cells;
impl Measurer for Cells {
    fn text_size(&self, t: &str, _: &Font, _: &FontParameters, _: Option<f32>) -> (f32, f32) {
        (t.chars().count() as f32 * 8.0, 16.0)
    }
}

const VIEW: Rect = Rect::new(0.0, 0.0, 400.0, 300.0);

fn theme() -> TextStyle {
    TextStyle::new(Font::Named("mono".into()), FontParameters::new(14.0), Color::rgb(255, 255, 255)).no_wrap()
}

struct Item { id: u32, name: String }

fn view(b: &mut Build, t: &TextStyle, items: &[Item], line: usize, modified: bool) {
    ui!(b;
        with_text(t) {
            div(Style::new()) {
                div(Style::row().height(px(30.0)).gap(8.0)) {
                    text(Style::new(), "Save");
                    if modified => { text(Style::new(), "*"); }
                }
                div(Style::new().flex(1.0)) {
                    for it in items => {
                        text(Style::new().id(it.id as u64), &it.name);
                    }
                }
                text(Style::new().height(px(16.0)), "{}:{}", line, 7);
            }
        }
    );
}

fn items(n: u32) -> Vec<Item> {
    (1..=n).map(|id| Item { id, name: format!("item {id}") }).collect()
}

#[test]
fn unchanged_rebuild_allocates_nothing_and_does_not_resolve() {
    let (t, its) = (theme(), items(200));
    let mut ui = Layout::new();
    for _ in 0..2 {
        ui.build(|b| view(b, &t, &its, 3, false));
        ui.compute(VIEW, &Cells);
    }
    let solves = ui.solves();
    let n = allocations(|| {
        ui.build(|b| view(b, &t, &its, 3, false));
        assert!(!ui.compute(VIEW, &Cells));
    });
    assert_eq!(n, 0, "unchanged rebuild allocated");
    assert_eq!(ui.solves(), solves);
}

#[test]
fn changed_text_in_place_allocates_nothing_once_warm() {
    let (t, its) = (theme(), items(50));
    let mut ui = Layout::new();
    for line in [1, 2] {
        ui.build(|b| view(b, &t, &its, line, false));
        ui.compute(VIEW, &Cells);
    }
    let build = allocations(|| ui.build(|b| view(b, &t, &its, 3, false)));
    let solve = allocations(|| {
        ui.compute(VIEW, &Cells);
    });
    eprintln!("build {build}, solve {solve}");
    assert_eq!(build, 0, "rebuild with changed text allocated");
}

#[test]
fn keyed_reorder_moves_nodes_instead_of_recreating() {
    let (t, mut its) = (theme(), items(5));
    let mut ui = Layout::new();
    ui.build(|b| view(b, &t, &its, 1, false));
    ui.compute(VIEW, &Cells);
    let before: Vec<_> = ui.children(ui.children(ui.root()).next().unwrap()).collect();
    let list = before[1];
    let ids_before: Vec<_> = ui.children(list).collect();
    its.swap(0, 4);
    ui.build(|b| view(b, &t, &its, 1, false));
    ui.compute(VIEW, &Cells);
    let ids_after: Vec<_> = ui.children(list).collect();
    assert_eq!(ids_after[0], ids_before[4]);
    assert_eq!(ids_after[4], ids_before[0]);
    // root, outer div, bar, "Save", list, 5 items, modeline text
    assert_eq!(ui.len(), 11);
}

#[test]
fn conditional_element_appearing_and_vanishing_keeps_siblings() {
    let (t, its) = (theme(), items(2));
    let mut ui = Layout::new();
    ui.build(|b| view(b, &t, &its, 1, false));
    let bar = ui.children(ui.children(ui.root()).next().unwrap()).next().unwrap();
    let save = ui.children(bar).next().unwrap();
    ui.build(|b| view(b, &t, &its, 1, true));
    assert_eq!(ui.children(bar).count(), 2);
    assert_eq!(ui.children(bar).next().unwrap(), save);
    ui.build(|b| view(b, &t, &its, 1, false));
    assert_eq!(ui.children(bar).count(), 1);
}

#[test]
fn removing_items_trims_the_tail_and_resolves() {
    let (t, its) = (theme(), items(10));
    let mut ui = Layout::new();
    ui.build(|b| view(b, &t, &its, 1, false));
    ui.compute(VIEW, &Cells);
    let len = ui.len();
    ui.build(|b| view(b, &t, &its[..3], 1, false));
    assert_eq!(ui.len(), len - 7);
    assert!(ui.compute(VIEW, &Cells));
}

#[test]
fn look_change_does_not_resolve_but_requests_repaint() {
    let mut ui = Layout::new();
    let red = Color::rgb(255, 0, 0);
    let blue = Color::rgb(0, 0, 255);
    let describe = |ui: &mut Layout, c: Color| {
        ui.build(|b| b.div(Style::new().height(px(40.0)).bg(c).radius(4.0), |_| {}));
    };
    describe(&mut ui, red.clone());
    ui.compute(VIEW, &Cells);
    assert!(ui.take_repaint());
    let solves = ui.solves();
    describe(&mut ui, red);
    assert!(!ui.compute(VIEW, &Cells));
    assert!(!ui.take_repaint(), "unchanged look asked for a repaint");
    describe(&mut ui, blue);
    assert!(!ui.compute(VIEW, &Cells), "a colour change re-solved");
    assert_eq!(ui.solves(), solves);
    assert!(ui.take_repaint());
}

#[test]
fn border_takes_space_in_layout() {
    let mut ui = Layout::new();
    ui.build(|b| {
        b.div(Style::new().flex(1.0).border(3.0, Color::rgb(1, 2, 3)), |b| {
            b.el(Style::new().flex(1.0));
        })
    });
    ui.compute(VIEW, &Cells);
    let outer = ui.children(ui.root()).next().unwrap();
    let inner = ui.children(outer).next().unwrap();
    assert_eq!(ui.rect(inner), Rect::new(3.0, 3.0, VIEW.width - 6.0, VIEW.height - 6.0));
}

fn memo_view(b: &mut Build, runs: &Counter<u32>, rev: u32, text: &TextStyle) {
    b.with_text(text, |b| {
        b.memo(rev, |b| {
            b.div(Style::new(), |b| {
                runs.set(runs.get() + 1);
                b.text(Style::new(), "memo'd child");
            });
        });
    });
}

#[test]
fn memo_skips_children_until_deps_change() {
    let (t, runs) = (theme(), Counter::new(0));
    let mut ui = Layout::new();
    ui.build(|b| memo_view(b, &runs, 1, &t));
    ui.build(|b| memo_view(b, &runs, 1, &t));
    assert_eq!(runs.get(), 1, "unchanged deps ran the children again");
    ui.build(|b| memo_view(b, &runs, 2, &t));
    assert_eq!(runs.get(), 2);
    let n = allocations(|| ui.build(|b| memo_view(b, &runs, 2, &t)));
    assert_eq!(n, 0);
}

#[test]
fn memo_reruns_when_inherited_text_changes() {
    let (t, runs) = (theme(), Counter::new(0));
    let mut ui = Layout::new();
    ui.build(|b| memo_view(b, &runs, 1, &t));
    assert!(ui.take_repaint());
    let bigger = TextStyle::new(Font::Named("mono".into()), FontParameters::new(20.0), Color::rgb(9, 9, 9));
    ui.build(|b| memo_view(b, &runs, 1, &bigger));
    assert_eq!(runs.get(), 2, "a theme change left a memo'd subtree stale");
    // The memo'd child really took the new colour.
    assert!(ui.take_repaint());
}

#[test]
fn inherited_text_style_reaches_nested_text() {
    let (t, mut ui) = (theme(), Layout::new());
    ui.build(|b| {
        b.with_text(&t, |b| {
            b.div(Style::row(), |b| {
                b.text(Style::new(), "abcd");
                b.text_with(Style::new(), &TextStyle::new(Font::Named("x".into()), FontParameters::new(9.0), Color::rgb(0, 0, 0)).no_wrap(), "ab");
            })
        })
    });
    ui.compute(VIEW, &Cells);
    let div = ui.children(ui.root()).next().unwrap();
    let kids: Vec<_> = ui.children(div).collect();
    assert_eq!(ui.rect(kids[0]).width, 32.0);
    assert_eq!(ui.rect(kids[1]).width, 16.0);
}

#[cfg(debug_assertions)]
mod misuse {
    use super::*;

    #[test]
    #[should_panic(expected = "share id")]
    fn duplicate_sibling_ids_assert() {
        let mut ui = Layout::new();
        ui.build(|b| {
            b.el(Style::new().id(5));
            b.el(Style::new().id(5));
        });
    }

    #[test]
    #[should_panic(expected = "not a leaf")]
    fn memo_on_a_leaf_asserts() {
        let mut ui = Layout::new();
        ui.build(|b| b.memo(1, |b| b.el(Style::new())));
    }

    #[test]
    #[should_panic(expected = "described none")]
    fn memo_with_nothing_asserts() {
        let mut ui = Layout::new();
        ui.build(|b| b.memo(1, |_| {}));
    }

    #[test]
    #[should_panic(expected = "described several")]
    fn memo_with_two_containers_asserts() {
        let mut ui = Layout::new();
        ui.build(|b| {
            b.memo(1, |b| {
                b.div(Style::new(), |_| {});
                b.div(Style::new(), |_| {});
            })
        });
    }
}

#[test]
fn positional_element_vanishing_before_an_identified_one() {
    let mut ui = Layout::new();
    let describe = |ui: &mut Layout, first: bool| {
        ui.build(|b| {
            if first {
                b.el(Style::new().height(px(10.0)));
            }
            b.el(Style::new().id(9).height(px(20.0)));
            b.el(Style::new().height(px(30.0)));
        })
    };
    describe(&mut ui, true);
    let kids: Vec<_> = ui.children(ui.root()).collect();
    describe(&mut ui, false);
    let after: Vec<_> = ui.children(ui.root()).collect();
    assert_eq!(after.len(), 2);
    assert_eq!(after[0], kids[1], "the identified element must survive");
    assert_eq!(ui.tag(after[0]), 9);
    // Appearing again inserts in front without recreating the others.
    describe(&mut ui, true);
    let again: Vec<_> = ui.children(ui.root()).collect();
    assert_eq!(&again[1..], &after[..]);
}

#[test]
fn large_keyed_reversal_keeps_every_node() {
    let (t, mut its) = (theme(), items(2000));
    let mut ui = Layout::new();
    ui.build(|b| view(b, &t, &its, 1, false));
    let list = ui.children(ui.children(ui.root()).next().unwrap()).nth(1).unwrap();
    let before: Vec<_> = ui.children(list).collect();
    its.reverse();
    ui.build(|b| view(b, &t, &its, 1, false));
    let after: Vec<_> = ui.children(list).collect();
    let mut expect = before.clone();
    expect.reverse();
    assert_eq!(after, expect);
    assert_eq!(ui.len(), 2000 + 6);
}

#[test]
fn imperative_api_honours_look_and_id() {
    let mut ui = Layout::new();
    let c = Color::rgb(10, 20, 30);
    let n = ui.add(ui.root(), Style::new().height(px(20.0)).bg(c.clone()).radius(3.0).id(77));
    assert_eq!(ui.tag(n), 77);
    assert!(ui.take_repaint());
    ui.compute(VIEW, &Cells);
    let solves = ui.solves();
    ui.set_style(n, |s| s.bg(Color::rgb(1, 1, 1)));
    assert!(!ui.compute(VIEW, &Cells), "a look edit re-solved");
    assert_eq!(ui.solves(), solves);
    assert!(ui.take_repaint());
    ui.set_style(n, |s| s.id(0));
    assert_eq!(ui.tag(n), 0);
}
