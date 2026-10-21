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
    let tree = ui.add(row, Style::new().fixed_width(100.0));
    let a = ui.add(row, Style::new().flex(1.0));
    let b = ui.add(row, Style::new().flex(3.0));
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
    let panel = ui.add(row, Style::new().fixed_width(10.0));
    ui.add_text(panel, Style::new(), text("a very long unbreakable label").no_wrap());
    let rest = ui.add(row, Style::new().flex(1.0));
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
    ui.set_style(a, |s| s.height(px(10.0)));
    assert!(!ui.compute(VIEW, &M), "an identical style must not dirty");
    ui.set_style(a, |s| s.height(px(20.0)));
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
    ui.set_style(b, |s| s.height(px(30.0)));
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
    ui.add(row, Style::new().fixed_width(200.0));
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
    let panel = ui.add(row, Style::new().fixed_width(100.0));
    let rest = ui.add(row, Style::new().flex(1.0));
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(rest).x, 100.0);
    ui.set_style(panel, |s| s.hidden(true));
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
    let kids: Vec<_> = (0..3).map(|_| ui.add(row, Style::new().flex(1.0))).collect();
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
            labels.push(ui.add_text(row, Style::new().flex(1.0), text("some label text that wraps maybe")));
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

// --- regressions from the PR #4 review ---

fn paragraph_height(scale: f32, column: f32) -> f32 {
    let m = Cells { scale };
    let mut ui = Layout::new();
    let col = ui.add(ui.root(), Style::new().width(px(column)));
    let t = ui.add_text(col, Style::new(), text("aaaa bbbb cccc dddd"));
    ui.compute(VIEW, &m);
    ui.rect(t).height
}

#[test]
fn a_scale_change_drops_cached_text_sizes() {
    // Same physical width (160), half the logical one: the memo key alone
    // would collide and return the one-line size.
    let mut ui = Layout::new();
    let col = ui.add(ui.root(), Style::new().width(px(160.0)));
    let t = ui.add_text(col, Style::new(), text("aaaa bbbb cccc dddd"));
    ui.compute(VIEW, &Cells { scale: 1.0 });
    assert_eq!(ui.rect(t).height, 16.0);
    ui.set_style(col, |s| s.width(px(80.0)));
    ui.compute(VIEW, &Cells { scale: 2.0 });
    assert_eq!(ui.rect(t).height, paragraph_height(2.0, 80.0));
    assert_eq!(ui.rect(t).height, 32.0);
}

#[test]
fn min_content_first_does_not_hide_a_scale_change() {
    let mut ui = Layout::new();
    let col = ui.add(ui.root(), Style::new().width(px(160.0)));
    let t = ui.add_text(col, Style::new(), text("aaaa bbbb cccc dddd"));
    ui.compute(VIEW, &Cells { scale: 1.0 });
    ui.set_style(col, |s| s.width(px(80.0)));
    let hi = Cells { scale: 2.0 };
    ui.min_content(&hi); // documented order: before compute
    ui.compute(VIEW, &hi);
    assert_eq!(ui.rect(t).height, 32.0);
}

#[test]
fn siblings_share_edges_at_any_origin_and_scale() {
    let mut mismatches = Vec::new();
    for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
        let m = Cells { scale };
        for origin in [0.0, 0.3, 0.7, 1.0 / 3.0] {
            for kids in 3..8 {
                for half_steps in 0..=60 {
                    let width = 90.0 + half_steps as f32 * 0.5;
                    let mut ui = Layout::new();
                    let row = ui.add(
                        ui.root(),
                        Style::row().height(px(10.0)).padding_left(origin),
                    );
                    let ids: Vec<_> =
                        (0..kids).map(|_| ui.add(row, Style::new().flex(1.0))).collect();
                    ui.compute(Rect::new(0.0, 0.0, width, 50.0), &m);
                    for pair in ids.windows(2) {
                        let (a, b) = (ui.rect(pair[0]), ui.rect(pair[1]));
                        if (a.right() * scale).round() != (b.x * scale).round() {
                            mismatches.push((scale, origin, kids, width, a.right(), b.x));
                        }
                    }
                }
            }
        }
    }
    assert!(mismatches.is_empty(), "{} gaps/overlaps, e.g. {:?}", mismatches.len(), &mismatches[..mismatches.len().min(3)]);
}

#[test]
fn hiding_a_parent_clears_its_descendants_moved_flags() {
    let mut ui = Layout::new();
    let spacer = ui.add(ui.root(), Style::new().height(px(10.0)));
    let parent = ui.add(ui.root(), Style::new().height(px(20.0)));
    let child = ui.add(parent, Style::new().height(px(5.0)));
    ui.compute(VIEW, &M);
    ui.set_style(spacer, |s| s.height(px(30.0)));
    ui.compute(VIEW, &M);
    assert!(ui.moved(child));
    ui.set_style(parent, |s| s.hidden(true));
    ui.compute(VIEW, &M);
    assert!(!ui.moved(child));
    assert_eq!(ui.moved_count(), 0);
}

#[test]
fn a_no_op_compute_clears_the_moved_flags() {
    let mut ui = Layout::new();
    let a = ui.add(ui.root(), Style::new().height(px(10.0)));
    ui.compute(VIEW, &M);
    ui.set_style(a, |s| s.height(px(20.0)));
    assert!(ui.compute(VIEW, &M));
    assert!(ui.moved(a));
    assert!(!ui.compute(VIEW, &M));
    assert!(!ui.moved(a), "a poll on the next frame must not see it again");
    assert_eq!(ui.moved_count(), 0);
}

#[test]
fn min_content_is_cached_while_nothing_changes() {
    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row());
    ui.add(row, Style::new().fixed_width(50.0));
    for _ in 0..5 {
        assert_eq!(ui.min_content(&M).0, 50.0);
        ui.compute(VIEW, &M);
    }
    assert_eq!(ui.solves(), 1, "asking every frame must not re-solve every frame");
    ui.add(row, Style::new().fixed_width(25.0));
    assert_eq!(ui.min_content(&M).0, 75.0, "a change invalidates the cache");
}

#[test]
fn grow_zero_keeps_an_explicit_width() {
    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row());
    let a = ui.add(row, Style::new().width(px(100.0)).grow(0.0));
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(a).width, 100.0);
}

#[test]
fn restyling_the_root_with_the_same_style_is_a_no_op() {
    let mut ui = Layout::new();
    let root = ui.root();
    ui.set_style(root, |_| Style::new().padding(4.0));
    ui.compute(VIEW, &M);
    ui.set_style(root, |_| Style::new().padding(4.0));
    assert!(!ui.compute(VIEW, &M));
}

#[test]
#[should_panic(expected = "cannot have children")]
fn text_leaves_cannot_take_children() {
    let mut ui = Layout::new();
    let t = ui.add_text(ui.root(), Style::new(), text("x"));
    ui.add(t, Style::new());
}

// --- gaps called out by the second review ---

#[test]
fn grow_keeps_the_items_own_basis_and_flex_splits_the_whole_line() {
    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().height(px(10.0)));
    let a = ui.add(row, Style::new().width(px(100.0)).grow(1.0));
    let b = ui.add(row, Style::new().width(px(50.0)).grow(1.0));
    ui.compute(VIEW, &M); // 400 wide: 250 spare, split evenly
    assert_eq!((ui.rect(a).width, ui.rect(b).width), (225.0, 175.0));

    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().height(px(10.0)));
    let a = ui.add(row, Style::new().width(px(100.0)).flex(1.0));
    let b = ui.add(row, Style::new().width(px(50.0)).flex(3.0));
    ui.compute(VIEW, &M);
    assert_eq!((ui.rect(a).width, ui.rect(b).width), (100.0, 300.0));
}

#[test]
fn fixed_width_leaves_the_cross_axis_alone() {
    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().height(px(80.0)));
    let a = ui.add(row, Style::new().fixed_width(30.0).min_height(px(50.0)));
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(a).width, 30.0);
    assert_eq!(ui.rect(a).height, 80.0);
}

#[test]
fn percentages_wrap_justify_reverse_and_auto_margins() {
    let mut ui = Layout::new();
    let half = ui.add(ui.root(), Style::new().width(pct(0.5)).height(px(10.0)));
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(half).width, 200.0);

    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().wrap().width(px(100.0)).align_content(Justify::Start));
    let kids: Vec<_> = (0..3)
        .map(|_| ui.add(row, Style::new().size(40.0, 10.0).shrink(0.0)))
        .collect();
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(kids[0]).y, ui.rect(kids[1]).y);
    assert_eq!(ui.rect(kids[2]).y, 10.0, "third wraps to a second line");

    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().justify(Justify::SpaceBetween).height(px(10.0)));
    let a = ui.add(row, Style::new().size(50.0, 10.0));
    let b = ui.add(row, Style::new().size(50.0, 10.0));
    ui.compute(VIEW, &M);
    assert_eq!((ui.rect(a).x, ui.rect(b).right()), (0.0, 400.0));

    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().reverse().height(px(10.0)));
    let a = ui.add(row, Style::new().size(50.0, 10.0));
    let b = ui.add(row, Style::new().size(50.0, 10.0));
    ui.compute(VIEW, &M);
    assert!(ui.rect(a).x > ui.rect(b).x);

    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().height(px(10.0)));
    let a = ui.add(row, Style::new().size(50.0, 10.0).margin_left(auto()));
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(a).x, 350.0, "an auto margin absorbs the free space");
}

#[test]
fn a_long_unbreakable_word_stays_one_line_in_the_fake_and_set_text_remeasures() {
    let mut ui = Layout::new();
    let col = ui.add(ui.root(), Style::new().width(px(40.0)));
    let t = ui.add_text(col, Style::new(), text("supercalifragilistic"));
    ui.compute(VIEW, &M);
    assert_eq!(ui.rect(t).height, 16.0);
    ui.set_text(t, "a b c d e f g h");
    ui.compute(VIEW, &M);
    assert!(ui.rect(t).height > 16.0, "new text re-measured");
}

#[test]
fn non_finite_input_cannot_poison_the_solver() {
    let mut ui = Layout::new();
    let a = ui.add(ui.root(), Style::new().height(px(10.0)));
    assert!(ui.compute(Rect::new(0.0, 0.0, f32::INFINITY, f32::NAN), &M));
    assert!(ui.rect(a).width.is_finite() && ui.rect(a).height.is_finite());
    assert!(!ui.compute(Rect::new(0.0, 0.0, f32::INFINITY, f32::NAN), &M), "NaN must not re-solve forever");
    ui.compute(VIEW, &Cells { scale: f32::NAN });
    assert_eq!(ui.rect(a).width, 400.0);
}

#[test]
fn fixed_extents_hold_under_either_parent_axis() {
    let mut ui = Layout::new();
    let row = ui.add(ui.root(), Style::row().height(px(100.0)));
    let a = ui.add(row, Style::new().fixed_width(100.0).fixed_height(20.0));
    let col = ui.add(ui.root(), Style::new().width(px(300.0)));
    let b = ui.add(col, Style::new().fixed_width(100.0).fixed_height(5.0));
    ui.compute(VIEW, &M);
    assert_eq!((ui.rect(a).width, ui.rect(a).height), (100.0, 20.0));
    assert_eq!((ui.rect(b).width, ui.rect(b).height), (100.0, 5.0));
}
