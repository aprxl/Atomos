use super::*;
use crate::renderer::{Color, Font, FontParameters};

/// 8x16 cells; greedy word wrap on spaces, a long word is never broken.
struct Cells {
    scale: f32,
}

impl Measurer for Cells {
    fn scale_factor(&self) -> f32 {
        self.scale
    }

    fn text_size(
        &self,
        text: &str,
        _: &Font,
        _: &FontParameters,
        max_width: Option<f32>,
    ) -> (f32, f32) {
        let cols = |s: &str| s.chars().count() as f32;
        let Some(max) = max_width else {
            return (cols(text) * 8.0, 16.0);
        };
        let (mut lines, mut line, mut widest) = (1.0f32, 0.0f32, 0.0f32);
        for word in text.split_whitespace() {
            let w = cols(word) * 8.0;
            let needed = if line == 0.0 { w } else { line + 8.0 + w };
            if line > 0.0 && needed > max {
                lines += 1.0;
                line = w;
            } else {
                line = needed;
            }
            widest = widest.max(line);
        }
        (widest, lines * 16.0)
    }
}

const M: Cells = Cells { scale: 1.0 };
const VIEW: Rect = Rect::new(0.0, 0.0, 400.0, 300.0);

fn text(s: &str) -> Text {
    Text::new(
        s,
        Font::Named("test".into()),
        FontParameters::new(14.0),
        Color::rgb(255, 255, 255),
    )
}

#[test]
fn column_is_the_default_and_stacks() {
    let mut ui = Layout::new();
    let a = ui.add(ui.root(), Style::new().height(px(50.0)));
    let b = ui.add(ui.root(), Style::new().height(px(70.0)));
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(a), Rect::new(0.0, 0.0, 400.0, 50.0));
    assert_eq!(ui.rect(b), Rect::new(0.0, 50.0, 400.0, 70.0));
}

#[test]
fn row_with_fixed_and_weighted_grow() {
    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().height(px(40.0)).gap(10.0));
    let tree = ui.add(row, Style::new().fixed(100.0));
    let a = ui.add(row, Style::new().grow(1.0));
    let b = ui.add(row, Style::new().grow(3.0));
    ui.compute(VIEW, &M);
    // 400 - 100 - 2 gaps of 10 = 280, split 1:3.
    assert_eq!(ui.rect(tree), Rect::new(0.0, 0.0, 100.0, 40.0));
    assert_eq!(ui.rect(a), Rect::new(110.0, 0.0, 70.0, 40.0));
    assert_eq!(ui.rect(b), Rect::new(190.0, 0.0, 210.0, 40.0));
}

#[test]
fn padding_and_cross_axis_centering() {
    let mut ui = Layout::new();
    let bar = ui.add(
        ui.root(),
        Style::row().height(px(60.0)).items_center().padding_x(20.0),
    );
    let icon = ui.add(bar, Style::new().size(30.0, 30.0));
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(icon), Rect::new(20.0, 15.0, 30.0, 30.0));
}

#[test]
fn fixed_is_exact_even_when_content_is_wider() {
    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().height(px(20.0)));
    let panel = ui.add(row, Style::new().fixed(10.0));
    ui.add_text(panel, Style::new(), text("a very long unbreakable label").no_wrap());
    let rest = ui.add(row, Style::new().grow(1.0));
    ui.compute(VIEW, &M);
    // Not floored at the content's width: a collapse animation can drive
    // a panel below its minimum without it snapping.
    assert_eq!(ui.rect(panel).width, 10.0);
    assert_eq!(ui.rect(rest).x, 10.0);
}

#[test]
fn compute_is_a_no_op_until_something_changes() {
    let mut ui = Layout::new();
    let a = ui.add(ui.root(), Style::new().height(px(10.0)));
    assert!(ui.compute(VIEW, &M));
    assert!(!ui.compute(VIEW, &M));
    ui.set_style(a, |s| *s = s.clone().height(px(10.0)));
    assert!(!ui.compute(VIEW, &M), "an identical style must not dirty");
    ui.set_style(a, |s| *s = s.clone().height(px(20.0)));
    assert!(ui.compute(VIEW, &M));
    assert!(ui.compute(Rect::new(0.0, 0.0, 500.0, 300.0), &M));
    assert_eq!(ui.solves(), 3);
}

#[test]
fn moved_reports_only_what_changed() {
    let mut ui = Layout::new();
    let a = ui.add(ui.root(), Style::new().height(px(10.0)));
    let b = ui.add(ui.root(), Style::new().height(px(10.0)));
    let c = ui.add(ui.root(), Style::new().height(px(10.0)));
    ui.compute(VIEW, &M);
    ui.set_style(b, |s| *s = s.clone().height(px(30.0)));
    ui.compute(VIEW, &M);
    assert!(!ui.moved(a));
    assert!(ui.moved(b), "b grew");
    assert!(ui.moved(c), "c was pushed down");
    assert_eq!(ui.moved_count(), 2);
}

#[test]
fn text_wraps_to_its_column_and_sets_the_height() {
    let mut ui = Layout::new();
    let col = ui.add(ui.root(), Style::new().width(px(100.0)));
    let t = ui.add_text(col, Style::new(), text("aaaa bbbb cccc dddd"));
    ui.compute(VIEW, &M);
    // 100px fits "aaaa bbbb" (72px) but not a third word: two lines.
    assert_eq!(ui.rect(t).height, 32.0);
    ui.set_text(t, "aaaa");
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(t).height, 16.0);
}

#[test]
fn flex_item_with_text_is_not_squeezed_below_its_longest_word() {
    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().width(px(60.0)));
    let label = ui.add_text(row, Style::new(), text("abcdefghij kl"));
    ui.compute(VIEW, &M);
    // min-content is the 80px word; the row is narrower, so the item
    // stays at its minimum rather than shrinking into the word.
    assert_eq!(ui.rect(label).width, 80.0);
}

#[test]
fn min_content_is_the_window_floor() {
    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row());
    ui.add(row, Style::new().fixed(200.0));
    ui.add(row, Style::new().width(px(120.0)).shrink(0.0));
    let (w, _) = ui.min_content(&M);
    assert_eq!(w, 320.0);
    // The next compute re-solves for the real viewport.
    assert!(ui.compute(VIEW, &M));
}

#[test]
fn hidden_node_takes_no_space_and_keeps_its_rect() {
    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().height(px(20.0)));
    let panel = ui.add(row, Style::new().fixed(100.0));
    let rest = ui.add(row, Style::new().grow(1.0));
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(rest).x, 100.0);
    ui.set_style(panel, |s| *s = s.clone().hidden(true));
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(rest).x, 0.0);
    assert_eq!(ui.rect(panel).width, 100.0, "keeps its last rect");
    assert!(!ui.moved(panel));
}

#[test]
fn siblings_share_edges_at_fractional_scale() {
    let m = Cells { scale: 1.5 };
    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().height(px(10.0)));
    let kids: Vec<_> = (0..3).map(|_| ui.add(row, Style::new().grow(1.0))).collect();
    ui.compute(Rect::new(0.0, 0.0, 100.0, 50.0), &m);
    let rects: Vec<_> = kids.iter().map(|k| ui.rect(*k)).collect();
    assert_eq!(rects[0].right(), rects[1].x);
    assert_eq!(rects[1].right(), rects[2].x);
    assert_eq!(rects[2].right(), 100.0);
    for r in &rects {
        // Every edge is on the physical grid (multiples of 1/1.5).
        let on_grid = |v: f32| ((v * 1.5).round() - v * 1.5).abs() < 1e-4;
        assert!(on_grid(r.x) && on_grid(r.right()), "{r:?}");
    }
}

#[test]
fn absolute_children_leave_the_flow() {
    let mut ui = Layout::new();
    let flow = ui.add(ui.root(), Style::new().height(px(40.0)));
    let popup = ui.add(
        ui.root(),
        Style::new().absolute().top(px(100.0)).right(px(10.0)).size(80.0, 50.0),
    );
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(flow).height, 40.0);
    assert_eq!(ui.rect(popup), Rect::new(310.0, 100.0, 80.0, 50.0));
}

#[test]
fn hit_finds_the_deepest_topmost_node_and_respects_clips() {
    let mut ui = Layout::new();
    let panel = ui.add(
        ui.root(),
        Style::new().size(100.0, 100.0).overflow(Overflow::Hidden),
    );
    let button = ui.add(panel, Style::new().size(50.0, 50.0));
    let spill = ui.add(
        panel,
        Style::new().absolute().top(px(90.0)).left(px(0.0)).size(50.0, 50.0),
    );
    ui.set_tag(button, 7);
    ui.compute(VIEW, &M);
    assert_eq!(ui.hit((10.0, 10.0)), Some(button));
    assert_eq!(ui.hit_tag((10.0, 10.0)), Some(7));
    assert_eq!(ui.hit((10.0, 95.0)), Some(spill));
    assert_eq!(ui.hit((10.0, 120.0)), Some(ui.root()), "clipped spill is not hit");
    assert_eq!(ui.hit_tag((200.0, 200.0)), None);
}

#[test]
fn removing_a_subtree_frees_it_and_stales_handles() {
    let mut ui = Layout::new();
    let a = ui.add(ui.root(), Style::new().height(px(10.0)));
    let inner = ui.add(a, Style::new());
    let b = ui.add(ui.root(), Style::new().height(px(10.0)));
    assert_eq!(ui.len(), 4);
    ui.remove(a);
    assert_eq!(ui.len(), 2);
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(b).y, 0.0);
    let c = ui.add(ui.root(), Style::new());
    assert_ne!(c, a, "a reused slot gets a new generation");
    let stale = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ui.rect(inner)));
    assert!(stale.is_err());
}

#[test]
fn custom_leaf_is_asked_with_constraints() {
    let mut ui = Layout::new();
    let col = ui.add(ui.root(), Style::new().width(px(200.0)));
    let canvas = ui.add_measured(col, Style::new(), |c| {
        // 2:1 for whatever width it is given.
        let w = c.known.0.unwrap_or(100.0);
        (w, w / 2.0)
    });
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(canvas), Rect::new(0.0, 0.0, 200.0, 100.0));
}

/// `cargo test --release --lib layout::tests::numbers -- --ignored --nocapture`
#[test]
#[ignore = "prints numbers; run in release"]
fn numbers() {
    use std::time::Instant;
    println!("size_of taffy::Style      = {}", std::mem::size_of::<taffy::Style>());
    println!("size_of layout::Entry     = {}", std::mem::size_of::<Entry>());
    println!("size_of Option<Box<Paint>>= {}", std::mem::size_of::<Option<Box<Paint>>>());

    // ~200 nodes: 20 rows of (icon, label, grow, button, button, ...).
    let build = || {
        let mut ui = Layout::new();
        let mut labels = Vec::new();
        for _ in 0..20 {
            let row = ui.add(ui.root(), Style::row().height(px(24.0)).items_center().gap(4.0));
            for _ in 0..4 {
                ui.add(row, Style::new().size(16.0, 16.0).shrink(0.0));
            }
            labels.push(ui.add_text(row, Style::new().grow(1.0), text("some label text that wraps maybe")));
            for _ in 0..4 {
                ui.add(row, Style::new().size(20.0, 20.0).shrink(0.0));
            }
        }
        (ui, labels)
    };
    let (mut ui, labels) = build();
    println!("nodes = {}", ui.len());
    let t = Instant::now();
    ui.compute(VIEW, &M);
    println!("cold solve              = {:?}", t.elapsed());

    let n = 2000;
    let t = Instant::now();
    for i in 0..n {
        ui.compute(Rect::new(0.0, 0.0, 400.0 + (i % 50) as f32, 300.0), &M);
    }
    println!("resize solve (all text re-wrapped) = {:?}/solve", t.elapsed() / n);

    let t = Instant::now();
    for i in 0..n {
        ui.set_text(labels[(i % 20) as usize], if (i / 20) % 2 == 0 { "short" } else { "other label" });
        ui.compute(VIEW, &M);
    }
    println!("one text leaf changed   = {:?}/solve", t.elapsed() / n);

    let t = Instant::now();
    for _ in 0..1_000_000 {
        ui.compute(VIEW, &M);
    }
    println!("clean compute (no-op)   = {:?}/call", t.elapsed() / 1_000_000);
}
