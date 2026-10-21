//! Prototype checks for the declarative builder: reconcile semantics and the
//! zero-allocation steady state (UI layer only; renderer repaint excluded).

use std::alloc::{GlobalAlloc, Layout as AllocLayout, System};
use std::cell::Cell;

use atomos::layout::{Build, Layout, Measurer, Rect, Style, TextStyle, px};
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
        div(Style::new().el()) {
            div(Style::row().height(px(30.0)).gap(8.0).el()) {
                text(Style::new().el(), t, "Save");
                if modified => { text(Style::new().el(), t, "*"); }
            }
            div(Style::new().flex(1.0).el()) {
                for it in items => {
                    text(Style::new().el().id(it.id), t, &it.name);
                }
            }
            text(Style::new().height(px(16.0)).el(), t, "{}:{}", line, 7);
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
    assert_eq!(ui.len(), 1 + 1 + 2 + 1 + 5 + 1 + 1 - 1 + 1 - 1 + 0 + 1 - 1);
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
