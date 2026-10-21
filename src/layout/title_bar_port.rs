//! Typewritter's title bar, re-expressed with [`Layout`], checked against
//! the hand-written rect arithmetic it replaces.
//!
//! The `reference_*` functions below are copied verbatim from
//! `Typewritter/src/components/title_bar.rs` and `theme_switch.rs` (the
//! arithmetic only, `bar` being the title bar's rect). The test builds the
//! same four controls as a flex tree and compares every rect at every
//! window width Typewritter allows.

use super::*;

const HEIGHT: f32 = 60.0;
const SWITCH_SIZE: f32 = 30.0;
const SWITCH_INSET: f32 = 18.0;

fn reference_switch(bar: Rect) -> Rect {
    let middle = bar.y + (bar.height - 2.0) / 2.0;
    Rect::new(
        bar.right() - SWITCH_INSET - SWITCH_SIZE,
        middle - SWITCH_SIZE / 2.0,
        SWITCH_SIZE,
        SWITCH_SIZE,
    )
}

fn reference_sidebar(bar: Rect) -> Rect {
    Rect::new(bar.x + 188.0, bar.y + 13.0, 34.0, 34.0)
}

fn reference_focus(bar: Rect) -> Rect {
    Rect::new(reference_switch(bar).x - 94.0, bar.y + 13.0, 80.0, 34.0)
}

fn reference_search(bar: Rect) -> Rect {
    let left = reference_sidebar(bar).right() + 24.0;
    let right = reference_focus(bar).x - 24.0;
    let width = (right - left).clamp(0.0, 340.0);
    Rect::new(
        (bar.x + (bar.width - width) / 2.0).clamp(left, right.max(left) - width),
        bar.y + 12.0,
        width,
        36.0,
    )
}

/// The same bar as a flex tree. No coordinate is computed by hand: the
/// 188 is the brand block's width, the 24s are margins, the 94 is the
/// focus button's width plus its margin.
struct Bar {
    ui: Layout,
    sidebar: NodeId,
    search: NodeId,
    focus: NodeId,
    switch: NodeId,
}

fn bar_tree() -> Bar {
    let mut ui = Layout::new();
    let bar = ui.add(ui.root(), Style::row().height(px(HEIGHT)));

    // Left and right halves share the slack equally, so the search box
    // between them stays centred on the bar; each half is floored at its
    // own content, which gives Typewritter's clamp for free.
    let left = ui.add(bar, Style::row().flex(1.0));
    ui.add(left, Style::new().fixed_width(188.0)); // logo + wordmark live here
    let sidebar = ui.add(left, Style::new().size(34.0, 34.0).margin_top(13.0).shrink(0.0));

    let search = ui.add(
        bar,
        Style::new()
            .width(px(340.0))
            .min_width(px(0.0))
            .height(px(36.0))
            .margin_top(12.0)
            .margin_x(24.0),
    );

    let right = ui.add(bar, Style::row().flex(1.0).justify(Justify::End));
    let focus = ui.add(
        right,
        Style::new().size(80.0, 34.0).margin_top(13.0).margin_right(14.0).shrink(0.0),
    );
    let switch = ui.add(
        right,
        Style::new()
            .size(SWITCH_SIZE, SWITCH_SIZE)
            .margin_top(14.0)
            .margin_right(SWITCH_INSET)
            .shrink(0.0),
    );
    Bar {
        ui,
        sidebar,
        search,
        focus,
        switch,
    }
}

/// Rects that differ between the two, as (name, flex, reference).
fn compare(width: f32) -> Vec<(&'static str, Rect, Rect)> {
    struct Fixed;
    impl Measurer for Fixed {
        fn text_size(&self, _: &str, _: &Font, _: &FontParameters, _: Option<f32>) -> (f32, f32) {
            (0.0, 0.0)
        }
    }
    let mut bar = bar_tree();
    let view = Rect::new(0.0, 0.0, width, HEIGHT);
    bar.ui.compute(view, &Fixed);
    let reference = Rect::new(0.0, 0.0, width, HEIGHT);
    let pairs = [
        ("sidebar", bar.ui.rect(bar.sidebar), reference_sidebar(reference)),
        ("search", bar.ui.rect(bar.search), reference_search(reference)),
        ("focus", bar.ui.rect(bar.focus), reference_focus(reference)),
        ("switch", bar.ui.rect(bar.switch), reference_switch(reference)),
    ];
    pairs.into_iter().filter(|(_, a, b)| a != b).collect()
}

#[test]
fn title_bar_matches_the_hand_written_rects() {
    // Typewritter's window minimum for this bar is 620. The only
    // difference allowed is the search box landing on a whole pixel: the
    // hand-written version centres it with `(width - w) / 2`, which leaves
    // it on a half pixel at odd window widths (blurry edges on a 1x
    // display); Layout snaps to the physical grid.
    let mut unexpected = Vec::new();
    let mut half_pixel = 0;
    for width in 620..=2600 {
        for (name, flex, reference) in compare(width as f32) {
            let only_half_pixel_x = name == "search"
                && flex.y == reference.y
                && flex.width == reference.width
                && flex.height == reference.height
                && (flex.x - reference.x).abs() == 0.5;
            if only_half_pixel_x {
                half_pixel += 1;
            } else {
                unexpected.push(format!("{width}px {name}: flex {flex:?} vs reference {reference:?}"));
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "{} unexpected mismatches, first few:
{}",
        unexpected.len(),
        unexpected.iter().take(6).cloned().collect::<Vec<_>>().join("
")
    );
    assert!(half_pixel > 0, "the half-pixel difference disappeared; update this note");
}
